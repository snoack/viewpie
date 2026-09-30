//! One RTSP feed, decoded to DRM_PRIME frames.
//!
//! Hardware decode is requested but never required: a Pi 5 has an HEVC block
//! and no H.264 one, so in a mixed wall some feeds come back in software. The
//! caller cannot tell the difference: both paths end in a frame it can hand
//! to KMS, the software one after an upload.

use crate::ffi::*;
use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a decoder may go without producing a picture before the session
/// is given up on.
///
/// A socket that goes quiet is already caught by the read timeout. This is for
/// the stall that one cannot see: a decoder that takes the stream and never
/// hands back a frame. A hardware decoder short of memory does exactly that,
/// answering "try again" every few seconds for as long as it is let, and the
/// packet it will not accept is held rather than read past, so the socket is
/// never read again either. The viewport would stay dark with nothing in the
/// log to say why, and never reconnect.
///
/// Counted from the start of the session, not from the first picture: the
/// stall seen in practice never produced one. Fifteen seconds is well past a
/// camera's wait for its next keyframe after connecting, which is the one
/// legitimate stretch without a picture.
pub const PICTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a decoder thread reports back about itself.
#[derive(Default)]
pub struct FeedStats {
    pub decoded: AtomicU64,
    pub errors: AtomicU64,
    /// Incremented once per frame handed to the presenter, so the commit loop
    /// can tell a new frame from the same buffer committed again.
    pub published: AtomicU64,
}

/// One decoded frame, and the capture buffer behind it.
///
/// Dropping it returns that buffer to the decoder, so a frame is kept until
/// the display has finished with it.
pub struct Frame(*mut AVFrame);

impl Frame {
    pub fn as_ptr(&self) -> *mut AVFrame {
        self.0
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { av_frame_free(&mut self.0) }
    }
}

// The frame is handed from the thread that decoded it to the ones that
// present it, and freed by whichever lets go last.
unsafe impl Send for Frame {}

// Shared once published, but only to be held: all a shared reference offers
// is as_ptr, and reading through that pointer is unsafe and the decoding
// thread's business, done before the frame is ever shared.
unsafe impl Sync for Frame {}

/// Why a stream could not be opened.
pub enum Refused {
    /// Anything another try might get past: a camera that is down, a
    /// network that blinked, a decoder that would not take it this time.
    Failed(String),
    /// A picture of `size`, larger than the `limit` the display takes at
    /// all. Every picture has to become a framebuffer before a plane can
    /// show it, scaled or not, and the kernel refuses one larger than that.
    /// So nothing that decodes it can make it showable, and it is refused
    /// before any decoder opens, rather than decoded to be thrown away.
    TooLarge { size: (u32, u32), limit: (u32, u32) },
}

impl From<String> for Refused {
    fn from(e: String) -> Self {
        Refused::Failed(e)
    }
}

impl From<&str> for Refused {
    fn from(e: &str) -> Self {
        Refused::Failed(e.to_string())
    }
}

pub struct Decoder {
    fmt: *mut AVFormatContext,
    codec_ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    stream_index: c_int,
    pub stats: Arc<FeedStats>,
    /// An option this build of libav did not recognise, if any. Carried back
    /// rather than printed here, so the caller can say which feed it was
    /// about in the same words it names every other line.
    pub unused: Option<String>,
    /// When the decoder last produced a picture, or the session started.
    last_picture: Instant,
    /// Whether next_frame gave up because PICTURE_TIMEOUT passed without a
    /// picture, rather than because the stream ended.
    pub stalled: bool,
}

// The pointers are owned by this struct and touched only from the thread that
// owns it; nothing is shared without a lock.
unsafe impl Send for Decoder {}

impl Decoder {
    /// What this stream is encoded as: "h264", "hevc", and so on.
    ///
    /// libav's own name for the codec rather than a table of our own, so a
    /// stream nobody anticipated still says what it is instead of reading as
    /// unknown.
    pub fn codec(&self) -> &'static str {
        unsafe {
            let name = avcodec_get_name((*self.codec_ctx).codec_id);
            if name.is_null() {
                return "unknown";
            }
            std::ffi::CStr::from_ptr(name).to_str().unwrap_or("unknown")
        }
    }

    /// Open an RTSP url and set up a decoder, preferring the DRM hwaccel,
    /// for a picture no larger than `limit`, of which the caller may hold
    /// `held` decoded frames at once.
    pub fn open(
        url: &str,
        stats: Arc<FeedStats>,
        limit: (u32, u32),
        held: usize,
    ) -> Result<Self, Refused> {
        unsafe {
            avformat_network_init();

            let mut opts: *mut AVDictionary = std::ptr::null_mut();
            // TCP: UDP loses packets under nine concurrent feeds and the
            // artefacts are indistinguishable from decoder bugs.
            dict_set(&mut opts, "rtsp_transport", "tcp");
            // Refuse audio during SDP negotiation rather than reading it and
            // throwing it away. A wall never plays sound, and a stream the
            // server was never asked to SETUP costs no bandwidth, no
            // depacketising and no packets to discard, nine times over.
            dict_set(&mut opts, "allowed_media_types", "video");
            // How long a socket read may block before the feed gives up and
            // reconnects. Without it a camera that accepts the TCP connection
            // and then says nothing hangs inside avformat_open_input for as
            // long as the kernel's own timeout, which is minutes: the thread
            // never reaches its own backoff, so the viewport stays black and
            // nothing explains why.
            //
            // Both spellings, because the option was renamed. ffmpeg 5.0
            // replaced the RTSP demuxer's "stimeout" with "timeout", and 7.x
            // offers only the latter (checked against 7.1.5 on Raspberry Pi
            // OS). An option a build does not recognise is left in the
            // dictionary rather than refused, so setting both covers either
            // vintage and the unused one costs nothing.
            for name in ["timeout", "stimeout"] {
                dict_set(&mut opts, name, "5000000");
            }
            let mut fmt: *mut AVFormatContext = std::ptr::null_mut();
            let c_url = CString::new(url).map_err(|_| "url has a NUL")?;
            let r = avformat_open_input(&mut fmt, c_url.as_ptr(), std::ptr::null_mut(), &mut opts);
            // What open_input did not understand it leaves behind, so this is
            // also where the dictionary is freed. Reported rather than
            // ignored: an option this build has never heard of is silently
            // dropped, which is how "stimeout" went on being set for years
            // after ffmpeg renamed it and left every feed with no read
            // timeout at all.
            let mut unknown = unused_options(&mut opts, url);
            if r < 0 {
                return Err(format!("open {url}: {}", av_err(r)).into());
            }
            let r = avformat_find_stream_info(fmt, std::ptr::null_mut());
            if r < 0 {
                avformat_close_input(&mut fmt);
                return Err(format!("stream info: {}", av_err(r)).into());
            }

            let mut codec: *const AVCodec = std::ptr::null();
            let idx =
                av_find_best_stream(fmt, AVMediaType_AVMEDIA_TYPE_VIDEO, -1, -1, &mut codec, 0);
            if idx < 0 {
                avformat_close_input(&mut fmt);
                return Err("no video stream".into());
            }

            // Its size as the stream describes it, which is known before any
            // decoder opens. Checked here because opening one is where a
            // picture too large to show costs the most: a Pi 3's H.264 block
            // tries to allocate a 4K stream's buffers from CMA, fails, and
            // leaves software to decode frames the display then refuses.
            //
            // The visible size, as the frame's is once cropped. A decoder
            // pads to its alignment, but that rounds to a multiple of 16 to
            // 128, which the limits are multiples of too, so padding cannot
            // take a picture over one.
            let par = stream_codecpar(fmt, idx);
            let size = ((*par).width.max(0) as u32, (*par).height.max(0) as u32);
            if size.0 > limit.0 || size.1 > limit.1 {
                avformat_close_input(&mut fmt);
                return Err(Refused::TooLarge { size, limit });
            }

            // Prefer a stateful m2m decoder where the board has one. A Pi 3
            // and Pi 4 decode H.264 in hardware through bcm2835-codec, which
            // ffmpeg reaches as h264_v4l2m2m; a Pi 5 dropped that block
            // entirely and has to fall back to software. HEVC is the other way
            // round: only a Pi 4/5 has it, and it is stateless, so it comes
            // through the DRM hwaccel below rather than a named decoder.
            //
            // Every feed gets it: bcm2835-codec is an M2M device, so an open
            // is a context rather than a claim on the block, and a
            // nine-viewport wall runs nine decode sessions. That is what makes
            // a Pi 3 usable at all.
            let software = avcodec_find_decoder((*codec).id);
            if let Some(name) = m2m_name(codec) {
                if m2m_device_present() {
                    let c_name = CString::new(name).unwrap();
                    let m2m = avcodec_find_decoder_by_name(c_name.as_ptr());
                    if !m2m.is_null() {
                        codec = m2m;
                    }
                }
            }

            // Only the format context is open at this point, so it is the one
            // thing an error still has to close by hand: Decoder is not built
            // yet, so Drop cannot. That matters because the open below is the
            // ordinary failure for a stream the decoder will not take, and
            // feed_thread retries forever, so a leak there is an
            // AVFormatContext and its socket every few seconds.
            let mut close = |e: String| {
                avformat_close_input(&mut fmt);
                e
            };

            // The hardware decoder may refuse the stream: too many sessions,
            // or one it cannot handle. A viewport showing a software-decoded
            // picture beats a viewport showing nothing.
            let mut codec_ctx = match open_codec(codec, par, true, held, &mut unknown) {
                Ok(ctx) => ctx,
                Err(_) if !software.is_null() && codec != software => {
                    match open_codec(software, par, false, held, &mut unknown) {
                        Ok(ctx) => ctx,
                        Err(e) => return Err(close(e).into()),
                    }
                }
                Err(e) => return Err(close(e).into()),
            };
            let unused = (!unknown.is_empty()).then(|| {
                format!(
                    "this ffmpeg does not know {}; continuing without it",
                    unknown.join(", ")
                )
            });

            let pkt = av_packet_alloc();
            if pkt.is_null() {
                avcodec_free_context(&mut codec_ctx);
                return Err(close("alloc packet".into()).into());
            }

            Ok(Decoder {
                unused,
                last_picture: Instant::now(),
                stalled: false,
                fmt,
                codec_ctx,
                pkt,
                stream_index: idx,
                stats,
            })
        }
    }

    /// Pull one decoded frame, or None if the stream needs more input.
    ///
    /// The frame is the caller's to hold and drop. That matters for a hardware
    /// decoder: the frame owns a capture buffer, and dropping it hands that
    /// buffer back to the decoder. Do it while the buffer is still being
    /// scanned out and the viewport tears or shows another feed's pixels, so a
    /// frame has to outlive the flip that replaced it.
    pub fn next_frame(&mut self) -> Option<Frame> {
        unsafe {
            // A packet held back because the decoder's input queue was full.
            // Dropping it instead loses a frame, and on a stream with few
            // keyframes that is visible for a long time.
            let mut pending = false;

            loop {
                let frame = av_frame_alloc();
                if frame.is_null() {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                let frame = Frame(frame);

                let r = avcodec_receive_frame(self.codec_ctx, frame.0);
                if r == 0 {
                    self.stats.decoded.fetch_add(1, Ordering::Relaxed);
                    self.last_picture = Instant::now();
                    return Some(frame);
                }
                // Freed here rather than leaked: this is the common path,
                // because a v4l2m2m decoder answers EAGAIN far more often
                // than it answers with a frame.
                drop(frame);
                if r != AVERROR_EAGAIN {
                    // EOF only follows a flush, which this loop never sends,
                    // so anything but EAGAIN is the stream really ending.
                    if r != AVERROR_EOF {
                        self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    return None;
                }
                // Checked where the decoder says "try again", since that is
                // what a stalled one keeps saying.
                if self.last_picture.elapsed() >= PICTURE_TIMEOUT {
                    self.stalled = true;
                    return None;
                }

                // The decoder wants more input. Resend the packet it refused
                // last time before reading another.
                if !pending {
                    av_packet_unref(self.pkt);
                    loop {
                        let r = av_read_frame(self.fmt, self.pkt);
                        if r < 0 {
                            // The demuxer is out of data: the stream ended,
                            // or the camera went away.
                            if r != AVERROR_EAGAIN {
                                return None;
                            }
                            continue;
                        }
                        if (*self.pkt).stream_index == self.stream_index {
                            break;
                        }
                        av_packet_unref(self.pkt);
                    }
                }

                let r = avcodec_send_packet(self.codec_ctx, self.pkt);
                // A full input queue is not a lost packet: keep it and offer
                // it again once receive_frame has drained something.
                pending = r == AVERROR_EAGAIN;
                if r < 0 && !pending {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.pkt);
            // Frees the hwaccel device reference with it: the context owns
            // the only one.
            avcodec_free_context(&mut self.codec_ctx);
            avformat_close_input(&mut self.fmt);
        }
    }
}

/// Which of the demuxer's options this build of libav did not recognise,
/// with the dictionary freed.
///
/// avformat_open_input consumes what it understands and returns the rest, so
/// an option that has been renamed, or was never spelled right, leaves no
/// trace whatever: the feed simply runs without it. Naming them once per
/// connection is the difference between a timeout that is not set and a
/// timeout nobody knows is not set.
///
/// Only for an rtsp url, and only for options that are not expected to go
/// unused. Everything set here belongs to the RTSP demuxer, so a local file
/// leaves all of it behind and has nothing wrong with it; and the two
/// spellings of the read timeout cannot both be consumed, since only one
/// exists in any given ffmpeg.
unsafe fn unused_options(opts: &mut *mut AVDictionary, url: &str) -> Vec<String> {
    let left = dict_take_keys(opts);
    if !url.starts_with("rtsp") {
        return Vec::new();
    }
    left.into_iter()
        .filter(|key| key != "timeout" && key != "stimeout")
        .collect()
}

/// Set `key` to `value` in a libav options dictionary.
unsafe fn dict_set(opts: &mut *mut AVDictionary, key: &str, value: &str) {
    let k = CString::new(key).unwrap();
    let v = CString::new(value).unwrap();
    av_dict_set(opts, k.as_ptr(), v.as_ptr(), 0);
}

/// The keys left in a libav options dictionary, which after an open are the
/// options it did not take, and the dictionary freed.
unsafe fn dict_take_keys(opts: &mut *mut AVDictionary) -> Vec<String> {
    let mut keys = Vec::new();
    let mut entry: *mut AVDictionaryEntry = std::ptr::null_mut();
    loop {
        entry = av_dict_get(*opts, c"".as_ptr(), entry, AV_DICT_IGNORE_SUFFIX as c_int);
        if entry.is_null() {
            break;
        }
        keys.push(
            std::ffi::CStr::from_ptr((*entry).key)
                .to_string_lossy()
                .into_owned(),
        );
    }
    av_dict_free(opts);
    keys
}

/// Build and open a codec context, or free it and say why.
///
/// One function rather than a first attempt and a hand-written retry, so a
/// setting cannot be established on one path and quietly forgotten on the
/// other: everything a context needs is here, and the fallback is the same
/// call with a different codec.
///
/// `hwaccel` asks for the DRM device. On a Pi 5 that binds the stateless HEVC
/// block through /dev/media1 and yields dmabufs in the SAND column format;
/// where the codec has no hardware path the create fails and the context is
/// simply opened without one. The software fallback passes false, since the
/// point of reaching it is that the hardware would not take the stream.
/// How many buffers a v4l2m2m decoder gets to decode into beyond the
/// frames its caller may be holding, so that it is never left waiting on
/// the display for one.
const SPARE_CAPTURE_BUFFERS: usize = 2;

/// How many buffers a v4l2m2m decoder gets for the compressed stream. Twice
/// as many as a Pi 3 decoded every camera of a wall with at full rate, for
/// the keyframes, which are many times the size of the frames between.
const OUTPUT_BUFFERS: usize = 4;

/// Open a decoder for `codec`, with the DRM hwaccel when `hwaccel`, whose
/// caller may hold `held` decoded frames at once. The options it did not
/// take go on `unknown`.
unsafe fn open_codec(
    codec: *const AVCodec,
    par: *const AVCodecParameters,
    hwaccel: bool,
    held: usize,
    unknown: &mut Vec<String>,
) -> Result<*mut AVCodecContext, String> {
    let mut ctx = avcodec_alloc_context3(codec);
    if ctx.is_null() {
        return Err("alloc codec context".into());
    }
    let fail = |ctx: &mut *mut AVCodecContext, e: String| {
        avcodec_free_context(ctx);
        e
    };

    let r = avcodec_parameters_to_context(ctx, par);
    if r < 0 {
        let msg = format!("parameters_to_context: {}", av_err(r));
        return Err(fail(&mut ctx, msg));
    }

    if hwaccel {
        let mut hw_device: *mut AVBufferRef = std::ptr::null_mut();
        if av_hwdevice_ctx_create(
            &mut hw_device,
            AVHWDeviceType_AV_HWDEVICE_TYPE_DRM,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        ) >= 0
        {
            // The context takes ownership and unrefs it when freed, so no
            // second copy is kept here: unreffing it twice is a double free,
            // and surfaces as a corrupted heap far from this line.
            (*ctx).hw_device_ctx = hw_device;
        }
    }

    // Ask for dmabufs rather than pixels. Without this the decoder offers a
    // software format first and libav takes it, so every frame is copied out
    // of a buffer the display controller could have scanned out directly.
    (*ctx).get_format = Some(prefer_drm_prime);

    // A v4l2m2m decoder's buffers come out of CMA, both kinds, and libav's
    // defaults are generous: twenty for decoded frames and sixteen for the
    // compressed stream. On a Pi 3 that took 160MB of its 256MB for a wall
    // of nine cameras, and a 1600x1200 camera fullscreen left 9MB, too
    // little for another camera reconnecting meanwhile to get any: the
    // kernel's refusals were for single 640x360 frames. Its caller's frames
    // and a couple more are all a decoder needs to decode into, and with
    // those and four for the stream, the same wall ran at the same rates
    // with 190MB free. The fewest it was tried with that still did was
    // three and two.
    let mut opts: *mut AVDictionary = std::ptr::null_mut();
    let name = std::ffi::CStr::from_ptr((*codec).name).to_string_lossy();
    if name.ends_with("_v4l2m2m") {
        for (key, n) in [
            ("num_capture_buffers", held + SPARE_CAPTURE_BUFFERS),
            ("num_output_buffers", OUTPUT_BUFFERS),
        ] {
            dict_set(&mut opts, key, &n.to_string());
        }
    }
    let r = avcodec_open2(ctx, codec, &mut opts);
    // Whatever the decoder did not take is left in the dictionary, and it is
    // said rather than dropped: a decoder that stopped knowing the option
    // would otherwise go back to its defaults, and CMA to running short,
    // with nothing to say why.
    let left = dict_take_keys(&mut opts);
    if r < 0 {
        let msg = format!("open codec: {}", av_err(r));
        return Err(fail(&mut ctx, msg));
    }
    unknown.extend(left);
    Ok(ctx)
}

/// The stateful m2m decoder for this codec, where one might exist.
///
/// Only H.264 qualifies: it is the codec bcm2835-codec decodes on a Pi 3/4.
/// HEVC on a Pi 4/5 is a stateless decoder reached through the DRM hwaccel
/// instead, not by naming a decoder.
unsafe fn m2m_name(codec: *const AVCodec) -> Option<&'static str> {
    if (*codec).id == AVCodecID_AV_CODEC_ID_H264 {
        Some("h264_v4l2m2m")
    } else {
        None
    }
}

/// Pick DRM_PRIME from the formats a decoder offers, or its first choice.
///
/// libav calls this once the stream's parameters are known, with the formats
/// the decoder can produce in preference order and AV_PIX_FMT_NONE at the
/// end. Taking the first entry, which is what happens with no callback,
/// means taking a software format and copying every frame.
unsafe extern "C" fn prefer_drm_prime(
    _ctx: *mut AVCodecContext,
    mut fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    let first = *fmts;
    while *fmts != AVPixelFormat_AV_PIX_FMT_NONE {
        if *fmts == AVPixelFormat_AV_PIX_FMT_DRM_PRIME {
            return AVPixelFormat_AV_PIX_FMT_DRM_PRIME;
        }
        fmts = fmts.add(1);
    }
    first
}

/// A hardware decode path `Decoder::open` reaches for.
pub struct HwPath {
    /// The codec, as a person would name it.
    pub codec: &'static str,
    /// What libav calls the path.
    pub libav: &'static str,
    /// Whether this build of libav has it.
    pub in_libav: fn() -> bool,
    /// Whether the board has the block behind it.
    pub on_board: fn() -> bool,
}

/// Every hardware path, whether or not this build of libav has it.
///
/// H.264 is h264_v4l2m2m, reached by name. HEVC is a DRM device handed to the
/// stock decoder, which only Raspberry Pi OS's libav takes: Debian's has
/// h264_v4l2m2m but not that hwaccel, and nothing fails without it. The
/// decoder opens, ignores the device and decodes in software, so asking libav
/// what the codec accepts is the only way to notice.
pub const HW_PATHS: [HwPath; 2] = [
    HwPath {
        codec: "H.264",
        libav: "h264_v4l2m2m",
        in_libav: || unsafe { !avcodec_find_decoder_by_name(c"h264_v4l2m2m".as_ptr()).is_null() },
        on_board: m2m_device_present,
    },
    HwPath {
        codec: "H.265",
        libav: "the hevc decoder's DRM hwaccel",
        in_libav: || unsafe { takes_drm_device(avcodec_find_decoder(AVCodecID_AV_CODEC_ID_HEVC)) },
        on_board: hevc_device_present,
    },
];

/// Whether a decoder will use a DRM device set as its hw_device_ctx, which
/// is how `open_codec` offers one.
unsafe fn takes_drm_device(codec: *const AVCodec) -> bool {
    if codec.is_null() {
        return false;
    }
    (0..)
        .map(|i| avcodec_get_hw_config(codec, i))
        .take_while(|c| !c.is_null())
        .any(|c| {
            (*c).device_type == AVHWDeviceType_AV_HWDEVICE_TYPE_DRM
                && (*c).methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0
        })
}

/// Whether the stateless HEVC block of a Pi 4 or Pi 5 is present.
///
/// Found by the name of its V4L2 device, which depends on the host's kernel
/// rather than the image: rpi-hevc-dec on current ones, rpivid on the 6.1
/// kernels older Raspberry Pi OS releases shipped.
fn hevc_device_present() -> bool {
    v4l2_device(|name| name.contains("hevc") || name.contains("rpivid"))
}

/// Whether a bcm2835-codec style m2m device is present.
///
/// ffmpeg's h264_v4l2m2m probes for one itself and fails at open time if there
/// is none, but that failure costs a stream restart; checking first keeps a
/// Pi 5, which has no H.264 block at all, on the software path without the
/// detour.
fn m2m_device_present() -> bool {
    // Asked once: a board does not grow a decoder while the process runs.
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PRESENT.get_or_init(|| v4l2_device(|name| name.contains("bcm2835-codec-decode")))
}

/// Whether the kernel has a V4L2 device whose name `matches`.
fn v4l2_device(matches: impl Fn(&str) -> bool) -> bool {
    std::fs::read_dir("/sys/class/video4linux")
        .into_iter()
        .flatten()
        .flatten()
        .any(|dev| {
            std::fs::read_to_string(dev.path().join("name")).is_ok_and(|name| matches(name.trim()))
        })
}

// The structs come from libav's own headers via bindgen, so these are plain
// field accesses rather than pointer arithmetic over a hand-written offset.
unsafe fn stream_codecpar(fmt: *mut AVFormatContext, idx: c_int) -> *const AVCodecParameters {
    let stream = *(*fmt).streams.offset(idx as isize);
    (*stream).codecpar
}
