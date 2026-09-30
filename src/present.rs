//! The atomic presenter: one commit puts every viewport on screen.
//!
//! This is what the wall is built around. Driving one sink per viewport makes
//! the viewports share the display's page-flip budget, so each gets 60/N fps;
//! setting every plane in a single atomic commit instead lets all of them
//! advance on the same vsync.
//!
//! Built on the `drm` crate rather than hand-written ioctl bindings: it wraps
//! them safely, and its PlanarBuffer trait already carries the format modifier
//! that a Pi's HEVC frames arrive with.

use drm::buffer::{DrmFourcc, DrmModifier, Handle, PlanarBuffer};
use drm::control::atomic::AtomicModeReq;
use drm::control::{
    self, connector, crtc, framebuffer, plane, property, AtomicCommitFlags,
    Device as ControlDevice, FbCmd2Flags,
};
use drm::Device as BasicDevice;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::time::Duration;

/// A DRM device, opened for modesetting.
pub struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl BasicDevice for Card {}
impl ControlDevice for Card {}

/// The property handles of one plane, looked up once at startup rather than
/// per frame: a property lookup is an ioctl, and nine of them per vsync is not
/// something to repeat 60 times a second.
#[derive(Debug, Clone, Copy)]
pub struct PlaneProps {
    pub fb_id: property::Handle,
    pub crtc_id: property::Handle,
    pub src_x: property::Handle,
    pub src_y: property::Handle,
    pub src_w: property::Handle,
    pub src_h: property::Handle,
    pub crtc_x: property::Handle,
    pub crtc_y: property::Handle,
    pub crtc_w: property::Handle,
    pub crtc_h: property::Handle,
}

/// A viewport's destination on screen and the source rectangle it takes from
/// its feed. The seam viewwall needed a videocrop element for is just
/// src_w/src_h here: a property, not a pipeline element, and no caps to
/// renegotiate.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub plane: plane::Handle,
    pub props: PlaneProps,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// How much this viewport's destination was inset to leave a seam. The
    /// same amount has to come off the source, scaled, or the picture is
    /// simply stretched back over the gap.
    pub gap_w: u32,
    pub gap_h: u32,
}

impl Viewport {
    /// The source rectangle to read for this viewport.
    ///
    /// The gap is a property of the wall, so it must be the same number of
    /// pixels on screen whatever a feed's resolution. Taking it off the
    /// destination alone would change the scale factor; taking it off the
    /// source alone would show nothing. Both, in proportion, leave the scale
    /// exactly as it was and the seam exactly as asked for, and a feed that
    /// matched its viewport still lands 1:1, which is what keeps nine planes
    /// within a Pi 3's budget.
    pub fn source(&self) -> (u32, u32) {
        (
            crop(self.src_w, self.gap_w, self.crtc_w),
            crop(self.src_h, self.gap_h, self.crtc_h),
        )
    }
}

/// Take `gap` destination pixels off a source of `src`, given a destination of
/// `dst`.
///
/// Integer arithmetic ordered to multiply first: `src / dst * gap` truncates
/// to zero whenever the source is smaller than the destination, so an upscaled
/// viewport would get no seam at all.
pub fn crop(src: u32, gap: u32, dst: u32) -> u32 {
    if gap == 0 || dst == 0 {
        return src;
    }
    let take = (src * gap + dst / 2) / dst;
    src.saturating_sub(take).max(1)
}

/// One decoded frame's dmabuf, described so the drm crate can import it.
///
/// The modifier is passed through from the decoder untouched: on a Pi an HEVC
/// frame arrives as NV12 with DRM_FORMAT_MOD_BROADCOM_SAND128, the
/// column-tiled layout. KMS understands it natively, which is exactly what
/// GStreamer's videocrop and buffer pool could not do.
pub struct ImportedFrame {
    pub width: u32,
    pub height: u32,
    pub format: DrmFourcc,
    pub modifier: DrmModifier,
    pub handles: [Option<Handle>; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
}

impl PlanarBuffer for ImportedFrame {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<DrmModifier> {
        // None rather than Linear: add_planar_framebuffer asserts that this
        // and the MODIFIERS flag agree, and a linear buffer is passed without
        // the flag.
        (self.modifier != DrmModifier::Linear).then_some(self.modifier)
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<Handle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

/// Name the DRM card whose connectors are the display outputs.
///
/// A Pi 3 has one card and it is card0. A Pi 5 has two: card0 is v3d, the
/// render-only GPU with no connectors at all, and card1 is vc4-drm, which owns
/// both HDMI outputs. Opening card0 there gives a card that can never scan
/// out, and the failure surfaces later as a modeset that cannot find a
/// connector rather than as a bad device.
///
/// Connectors decide it rather than the driver name or the number: sysfs nests
/// each connector under its card as "card1-HDMI-A-1", so a card that has one
/// is a card that can drive a screen. A connected connector wins over a merely
/// present one, so a second card with nothing plugged in does not take
/// precedence over the one showing a picture. But a card with connectors and
/// nothing attached still beats a render node, which keeps an unplugged
/// display reporting that it found no connector instead of naming the wrong
/// card.
pub fn detect_card() -> String {
    let mut with_connected = Vec::new();
    let mut with_connectors = Vec::new();

    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return DEFAULT_CARD.to_string();
    };
    let mut cards: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // A connector ("card1-HDMI-A-1"), not a card.
        .filter(|n| n.starts_with("card") && !n.contains('-'))
        .collect();
    cards.sort();

    for card in cards {
        let connectors: Vec<_> = std::fs::read_dir("/sys/class/drm")
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&format!("{card}-")))
            .collect();
        if connectors.is_empty() {
            // A render-only node such as the Pi 5's v3d.
            continue;
        }
        with_connectors.push(card.clone());
        for connector in connectors {
            let status = std::fs::read_to_string(format!("/sys/class/drm/{connector}/status"));
            if status.is_ok_and(|s| s.trim() == "connected") {
                with_connected.push(card.clone());
                break;
            }
        }
    }

    for candidates in [&with_connected, &with_connectors] {
        if let Some(card) = candidates.first() {
            return format!("/dev/dri/{card}");
        }
    }
    DEFAULT_CARD.to_string()
}

const DEFAULT_CARD: &str = "/dev/dri/card0";

impl Card {
    pub fn open(path: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open {path}: {e} (are you root, or in the video group?)"))?;
        let card = Card(file);
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .map_err(|e| format!("this kernel refused universal planes: {e}"))?;
        card.set_client_capability(drm::ClientCapability::Atomic, true)
            .map_err(|e| format!("this kernel refused the atomic cap: {e}"))?;
        Ok(card)
    }

    /// Every property of an object, by name, so the names in PlaneProps can be
    /// turned into handles once at startup.
    pub fn props_of<H: control::ResourceHandle>(
        &self,
        handle: H,
    ) -> Result<HashMap<String, property::Handle>, String> {
        let props = self
            .get_properties(handle)
            .map_err(|e| format!("could not read properties: {e}"))?;
        let mut out = HashMap::new();
        for (id, _value) in props {
            if let Ok(info) = self.get_property(id) {
                out.insert(info.name().to_string_lossy().into_owned(), id);
            }
        }
        Ok(out)
    }

    /// The crtc's ACTIVE property, looked up once because every commit names
    /// it: re-asserting an ACTIVE that is already true is the cheapest way to
    /// say which crtc a commit is about.
    pub fn crtc_active(&self, crtc: crtc::Handle) -> Result<property::Handle, String> {
        self.props_of(crtc)?
            .get("ACTIVE")
            .copied()
            .ok_or_else(|| "crtc has no ACTIVE".to_string())
    }

    pub fn plane_props(&self, plane: plane::Handle) -> Result<PlaneProps, String> {
        let m = self.props_of(plane)?;
        let get = |k: &str| {
            m.get(k)
                .copied()
                .ok_or_else(|| format!("plane has no {k} property"))
        };
        Ok(PlaneProps {
            fb_id: get("FB_ID")?,
            crtc_id: get("CRTC_ID")?,
            src_x: get("SRC_X")?,
            src_y: get("SRC_Y")?,
            src_w: get("SRC_W")?,
            src_h: get("SRC_H")?,
            crtc_x: get("CRTC_X")?,
            crtc_y: get("CRTC_Y")?,
            crtc_w: get("CRTC_W")?,
            crtc_h: get("CRTC_H")?,
        })
    }

    /// The largest framebuffer the card makes, width and height, which is
    /// the largest picture any plane can show, however it is scaled: a
    /// framebuffer is what a plane reads, and the kernel refuses to make
    /// one larger than this with EINVAL. A Pi 3's is 2048 across, too
    /// narrow for 4K; a Pi 4's and a Pi 5's are larger.
    pub fn largest_picture(&self) -> Result<(u32, u32), String> {
        use std::ops::{Bound, RangeBounds};
        let res = self
            .resource_handles()
            .map_err(|e| format!("could not read the card's resources: {e}"))?;
        let most = |bound: Bound<&u32>| match bound {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n.saturating_sub(1),
            Bound::Unbounded => u32::MAX,
        };
        Ok((
            most(res.supported_fb_width().end_bound()),
            most(res.supported_fb_height().end_bound()),
        ))
    }

    /// Overlay planes that can drive this CRTC.
    ///
    /// The primary plane is left out deliberately: it already carries the
    /// console at full screen, and claiming it for a viewport means fighting
    /// that configuration. Viewports are overlays; the primary stays as the
    /// background.
    pub fn overlay_planes(&self, crtc: crtc::Handle) -> Result<Vec<plane::Handle>, String> {
        let res = self
            .resource_handles()
            .map_err(|e| format!("could not read the card's resources: {e}"))?;
        let handles = self
            .plane_handles()
            .map_err(|e| format!("could not list planes: {e}"))?;
        let mut out = Vec::new();
        for p in handles {
            let Ok(info) = self.get_plane(p) else {
                continue;
            };
            // possible_crtcs is a bitmask over the card's crtc list, which the
            // crate resolves for us rather than making us index it by hand.
            if !res.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
                continue;
            }
            if self.is_overlay(p) {
                out.push(p);
            }
        }
        Ok(out)
    }

    fn is_overlay(&self, plane: plane::Handle) -> bool {
        self.plane_type_of(plane) == Some(0)
    }

    /// A plane's type: 0 overlay, 1 primary, 2 cursor.
    fn plane_type_of(&self, plane: plane::Handle) -> Option<u64> {
        let props = self.get_properties(plane).ok()?;
        for (id, value) in props {
            let Ok(info) = self.get_property(id) else {
                continue;
            };
            if info.name().to_bytes() == b"type" {
                return Some(value);
            }
        }
        None
    }

    /// A single-colour framebuffer covering the whole display.
    ///
    /// The wall's viewports are overlay planes, and whatever sits behind them
    /// shows through the gaps: the console, by default, which is not what
    /// anyone wants behind a camera wall. Painting the primary plane once
    /// costs one buffer and nothing per frame: it is set during the modeset
    /// and never touched again.
    // chunks_exact_mut rather than the as_chunks_mut clippy suggests, which
    // needs Rust 1.88: Debian trixie ships 1.85, and the package should build
    // with it. unknown_lints because that clippy does not know the lint yet.
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    pub fn background(&self, width: u32, height: u32, colour: u32) -> Result<BackgroundFb, String> {
        let mut buf = self
            .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
            .map_err(|e| format!("could not allocate the background: {e}"))?;
        {
            let mut map = self
                .map_dumb_buffer(&mut buf)
                .map_err(|e| format!("could not map the background: {e}"))?;
            for px in map.as_mut().chunks_exact_mut(4) {
                px.copy_from_slice(&colour.to_le_bytes());
            }
        }
        let fb = self
            .add_framebuffer(&buf, 24, 32)
            .map_err(|e| format!("could not add the background framebuffer: {e}"))?;
        Ok(BackgroundFb { _buf: buf, fb })
    }

    /// The plane that covers the whole crtc, which the background goes on.
    pub fn primary_plane(&self, crtc: crtc::Handle) -> Result<plane::Handle, String> {
        let res = self
            .resource_handles()
            .map_err(|e| format!("could not read the card's resources: {e}"))?;
        let handles = self
            .plane_handles()
            .map_err(|e| format!("could not list planes: {e}"))?;
        for p in handles {
            let Ok(info) = self.get_plane(p) else {
                continue;
            };
            if !res.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
                continue;
            }
            if self.plane_type_of(p) == Some(1) {
                return Ok(p);
            }
        }
        Err("this crtc has no primary plane".into())
    }

    /// Put the background up, setting a mode first if one was asked for.
    ///
    /// A mode is optional because a display usually already has one: the
    /// console set it at boot, and adopting it is both faster and less
    /// disruptive than switching to a mode of our own. Setting one is for
    /// when the configuration names a resolution the display is not already
    /// in. Either way the crtc has to end up active, or plane commits are
    /// accepted and scanned out by nobody.
    pub fn attach(
        &self,
        crtc: crtc::Handle,
        connector: connector::Handle,
        mode: Option<&control::Mode>,
        background: Option<framebuffer::Handle>,
    ) -> Result<(), String> {
        let crtc_props = self.props_of(crtc)?;
        let conn_props = self.props_of(connector)?;

        let mut req = AtomicModeReq::new();
        if let Some(mode) = mode {
            let blob = self
                .create_property_blob(mode)
                .map_err(|e| format!("could not create the mode blob: {e}"))?;
            req.add_property(
                crtc,
                *crtc_props.get("MODE_ID").ok_or("crtc has no MODE_ID")?,
                blob,
            );
        }
        // The background rides along with the modeset rather than being a
        // commit of its own: the primary plane has to be configured anyway.
        if let Some(bg) = background {
            let (w, h) = self.crtc_size(crtc, mode)?;
            let plane = self.primary_plane(crtc)?;
            let p = self.plane_props(plane)?;
            for (prop, value) in [
                (p.fb_id, property::Value::Framebuffer(Some(bg))),
                (p.crtc_id, property::Value::CRTC(Some(crtc))),
                (p.src_x, property::Value::UnsignedRange(0)),
                (p.src_y, property::Value::UnsignedRange(0)),
                (p.src_w, property::Value::UnsignedRange((w as u64) << 16)),
                (p.src_h, property::Value::UnsignedRange((h as u64) << 16)),
                (p.crtc_x, property::Value::SignedRange(0)),
                (p.crtc_y, property::Value::SignedRange(0)),
                (p.crtc_w, property::Value::UnsignedRange(w as u64)),
                (p.crtc_h, property::Value::UnsignedRange(h as u64)),
            ] {
                req.add_property(plane, prop, value);
            }
        }
        req.add_property(
            crtc,
            *crtc_props.get("ACTIVE").ok_or("crtc has no ACTIVE")?,
            property::Value::Boolean(true),
        );
        req.add_property(
            connector,
            *conn_props
                .get("CRTC_ID")
                .ok_or("connector has no CRTC_ID")?,
            property::Value::CRTC(Some(crtc)),
        );
        self.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .map_err(|e| format!("could not attach to the display: {e}"))
    }

    /// The mode a display offers matching a configured resolution.
    ///
    /// A refresh rate is optional: without one the first mode at that size
    /// wins, which is the display's own preference among them.
    pub fn find_mode(
        &self,
        connector: connector::Handle,
        want: crate::config::ModeSpec,
    ) -> Result<control::Mode, String> {
        let info = self
            .get_connector(connector, false)
            .map_err(|e| format!("could not read the connector: {e}"))?;
        info.modes()
            .iter()
            .find(|m| {
                let (w, h) = m.size();
                w as u32 == want.width
                    && h as u32 == want.height
                    && want.refresh.is_none_or(|r| m.vrefresh() == r)
            })
            .copied()
            .ok_or_else(|| {
                let offered: Vec<String> = info
                    .modes()
                    .iter()
                    .map(|m| format!("{}x{}@{}", m.size().0, m.size().1, m.vrefresh()))
                    .collect();
                format!(
                    "this display does not offer {}x{}{}; it offers {}",
                    want.width,
                    want.height,
                    want.refresh.map(|r| format!("@{r}")).unwrap_or_default(),
                    offered.join(", ")
                )
            })
    }

    /// The mode a display says it prefers, or the first it offers if it names
    /// none.
    pub fn preferred_mode(&self, connector: connector::Handle) -> Result<control::Mode, String> {
        let info = self
            .get_connector(connector, false)
            .map_err(|e| format!("could not read the connector: {e}"))?;
        let modes = info.modes();
        modes
            .iter()
            .find(|m| m.mode_type().contains(control::ModeTypeFlags::PREFERRED))
            .or_else(|| modes.first())
            .copied()
            .ok_or_else(|| "the display offers no modes".to_string())
    }

    /// The size the crtc is scanning out: the mode being set, or the one it
    /// is already in.
    pub fn crtc_size(
        &self,
        crtc: crtc::Handle,
        mode: Option<&control::Mode>,
    ) -> Result<(u32, u32), String> {
        let mode = self.crtc_mode(crtc, mode)?;
        let (w, h) = mode.size();
        Ok((w as u32, h as u32))
    }

    /// The mode the crtc will be scanning out: the one being set, or the one
    /// it is already in.
    pub fn crtc_mode(
        &self,
        crtc: crtc::Handle,
        mode: Option<&control::Mode>,
    ) -> Result<control::Mode, String> {
        if let Some(mode) = mode {
            return Ok(*mode);
        }
        let info = self
            .get_crtc(crtc)
            .map_err(|e| format!("could not read the crtc: {e}"))?;
        info.mode()
            .ok_or_else(|| "the display has no mode set; give one in the configuration".to_string())
    }

    /// Import a decoded frame's dmabuf as a scanout framebuffer.
    pub fn import(&self, frame: &ImportedFrame) -> Result<framebuffer::Handle, String> {
        // The MODIFIERS flag and a modifier have to agree: the crate asserts
        // on the mismatch rather than letting the kernel reject it.
        let flags = if frame.modifier == DrmModifier::Linear {
            FbCmd2Flags::empty()
        } else {
            FbCmd2Flags::MODIFIERS
        };
        self.add_planar_framebuffer(frame, flags)
            .map_err(|e| format!("could not import a frame: {e}"))
    }

    /// Turn a dmabuf file descriptor into a buffer handle this device can use.
    ///
    /// The handle holds a reference to the underlying buffer, so it has to be
    /// closed as well as its framebuffer destroyed. Leaving them open leaks
    /// CMA: a wall whose cameras reconnect a few dozen times exhausts a Pi 3's
    /// pool and viewports start failing to commit.
    pub fn import_dmabuf(&self, fd: BorrowedFd<'_>) -> Result<Handle, String> {
        self.prime_fd_to_buffer(fd)
            .map_err(|e| format!("could not import a dmabuf: {e}"))
    }

    /// Release a buffer handle imported from a dmabuf. Named apart from the
    /// trait's close_buffer, which it calls, so it does not shadow it.
    pub fn drop_buffer(&self, handle: Handle) {
        if let Err(e) = ControlDevice::close_buffer(self, handle) {
            eprintln!("could not close a buffer handle: {e}");
        }
    }

    /// Put every viewport's newest framebuffer on screen in one commit.
    ///
    /// Submits and returns: the flip it asks for arrives later on the card's
    /// event queue, and the caller commits again when it does. Waiting here
    /// instead would make a second display wait on this one's vsync, and the
    /// two are not phase locked.
    pub fn commit(
        &self,
        crtc: crtc::Handle,
        crtc_active: property::Handle,
        viewports: impl IntoIterator<Item = (Viewport, Option<framebuffer::Handle>)>,
    ) -> Result<(), String> {
        let mut req = AtomicModeReq::new();
        // The crtc, named whatever else this request says.
        //
        // A page-flip event is armed per crtc in the new state, so a request
        // that touches no crtc has nothing to hang one on and is refused with
        // EINVAL before any driver sees it. That happens the moment every
        // viewport is blanked at once and the request is nothing but
        // detaches: a wall whose cameras all dropped together would stop
        // committing exactly when it most needed to put the background up.
        //
        // Re-asserting ACTIVE, which is already true, costs one property and
        // changes nothing. Leaving the event off instead would also be
        // accepted, but then no flip arrives and the wall falls back to the
        // 100ms timeout, running at 10Hz for as long as it stays blank.
        req.add_property(crtc, crtc_active, property::Value::Boolean(true));
        for (viewport, fb) in viewports {
            let p = &viewport.props;
            let Some(fb) = fb else {
                // No frame to show: detach the plane so the background shows
                // through, rather than leaving a still frame up.
                req.add_property(viewport.plane, p.fb_id, property::Value::Framebuffer(None));
                req.add_property(viewport.plane, p.crtc_id, property::Value::CRTC(None));
                continue;
            };
            // The source rectangle is 16.16 fixed point; the destination is
            // plain pixels. Mixing these up puts a 1/65536th-size image in the
            // corner, which is the classic first-atomic-commit bug.
            let (src_w, src_h) = viewport.source();
            for (prop, value) in [
                (p.fb_id, property::Value::Framebuffer(Some(fb))),
                (p.crtc_id, property::Value::CRTC(Some(crtc))),
                (p.src_x, property::Value::UnsignedRange(0)),
                (p.src_y, property::Value::UnsignedRange(0)),
                (
                    p.src_w,
                    property::Value::UnsignedRange((src_w as u64) << 16),
                ),
                (
                    p.src_h,
                    property::Value::UnsignedRange((src_h as u64) << 16),
                ),
                (
                    p.crtc_x,
                    property::Value::SignedRange(viewport.crtc_x as i64),
                ),
                (
                    p.crtc_y,
                    property::Value::SignedRange(viewport.crtc_y as i64),
                ),
                (
                    p.crtc_w,
                    property::Value::UnsignedRange(viewport.crtc_w as u64),
                ),
                (
                    p.crtc_h,
                    property::Value::UnsignedRange(viewport.crtc_h as u64),
                ),
            ] {
                req.add_property(viewport.plane, prop, value);
            }
        }

        self.atomic_commit(
            AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
            req,
        )
        .map_err(|e| format!("atomic commit failed: {e}"))
    }

    /// Wait for page flips and say which crtcs they were for: empty if none
    /// came within the timeout.
    ///
    /// Every flip the read returns, not just the first. One read drains all
    /// the events queued on the card, and two displays are not phase locked,
    /// so their flips regularly arrive together; answering with the first
    /// dropped the other, and a display whose flip is dropped never learns it
    /// may commit again.
    ///
    /// The timeout is what keeps a wall alive when a display stops sending
    /// events, whether unplugged mid-run or a commit that failed and so will
    /// never produce one. The caller takes a timeout as permission to commit
    /// again rather than as an error.
    pub fn next_flips(&self, timeout: Duration) -> Result<Vec<crtc::Handle>, String> {
        let mut fds = [rustix::event::PollFd::new(
            &self.0,
            rustix::event::PollFlags::IN,
        )];
        let until = rustix::event::Timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        let ready = rustix::event::poll(&mut fds, Some(&until))
            .map_err(|e| format!("poll on the drm fd failed: {e}"))?;
        if ready == 0 {
            return Ok(Vec::new());
        }
        let events = self
            .receive_events()
            .map_err(|e| format!("could not read drm events: {e}"))?;
        // Anything else, a vblank or an event we do not use, is not a flip
        // and so not a cue to commit.
        Ok(events
            .filter_map(|event| match event {
                control::Event::PageFlip(flip) => Some(flip.crtc),
                _ => None,
            })
            .collect())
    }
}

/// A framebuffer, and the buffer handle behind it, freed when the last
/// reference goes.
///
/// Destroyed from whichever thread drops it last: a feed thread for a frame
/// no commit ever named, the commit loop for one that was on screen. Either
/// way nothing can still be scanning it out, because every commit that named
/// it holds a reference until the flip after it.
pub struct Fb {
    card: Arc<Card>,
    pub fb: framebuffer::Handle,
    /// The GEM handle the framebuffer was made from, closed after it: the
    /// other order lets the kernel hand the id back out while the
    /// framebuffer still refers to it. Leaving it open leaks CMA, one buffer
    /// per reconnect, until viewports start failing to commit.
    handle: Handle,
}

impl Fb {
    /// Take ownership of a framebuffer and the handle it was made from.
    pub fn new(card: Arc<Card>, fb: framebuffer::Handle, handle: Handle) -> Self {
        Fb { card, fb, handle }
    }
}

impl Drop for Fb {
    /// Failure is reported and ignored: nothing holds the framebuffer any
    /// more and there is nothing better to do about it, and one the kernel
    /// still has a use for stays alive until it does not.
    fn drop(&mut self) {
        if let Err(e) = self.card.destroy_framebuffer(self.fb) {
            eprintln!("could not release a framebuffer: {e}");
        }
        self.card.drop_buffer(self.handle);
    }
}

/// Holds the background's buffer alive for as long as the wall runs.
pub struct BackgroundFb {
    _buf: drm::control::dumbbuffer::DumbBuffer,
    pub fb: framebuffer::Handle,
}
