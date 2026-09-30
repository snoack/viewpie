//! What a display's television says over HDMI-CEC: whether the wall is
//! being watched, and what is pressed on its remote.
//!
//! A television switched to another input still holds hotplug up and still
//! answers for its EDID, so to the kernel the wall is connected and on screen
//! all the same. The only thing that knows otherwise is the television, and
//! CEC is how it says so: whenever it changes input it broadcasts which
//! physical address it has switched to, and every device on the bus hears it,
//! including the one being switched away from. Very nearly whenever: an LG
//! says nothing when it leaves a device that has taken an address on the bus
//! and then answers none of its questions, which is why the wall answers
//! them, and hands its address back when it stops.
//!
//! Listening for that is the reliable half. Asking is not: an LG on the
//! wall's input, with no device claiming to be the active source, answers
//! that it is showing itself -- in the very words it broadcasts unprompted
//! when it does switch to its own tuner or apps. So the wall claims the
//! active source whenever the television switches to it, which cannot switch
//! anything since the television is already there, and from then on answers
//! for itself. It never claims otherwise. Doing so is how a device asks the television to
//! switch to it, and a wall that pulled the television over whenever it
//! restarted would be a wall nobody could watch anything else on.
//!
//! Anything uncertain reads as watched. A wall that is not paused when it
//! could have been costs some CPU; one that is paused while somebody looks
//! at it is a black screen.
//!
//! The remote is the easy part: the television passes its keys on to
//! whatever it is showing, which is why the wall joins the bus whether or
//! not it pauses off screen.

use crate::ffi::*;
use rustix::event::{poll, PollFd, PollFlags};
use rustix::ioctl::{self, opcode, Getter, Setter, Updater};
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type ConnectorInfo = Getter<{ opcode::read::<cec_connector_info>(b'a', 10) }, cec_connector_info>;
type PhysAddr = Getter<{ opcode::read::<u16>(b'a', 1) }, u16>;
type GetLogAddrs = Getter<{ opcode::read::<cec_log_addrs>(b'a', 3) }, cec_log_addrs>;
type SetLogAddrs<'a> = Updater<'a, { opcode::read_write::<cec_log_addrs>(b'a', 4) }, cec_log_addrs>;
type Transmit<'a> = Updater<'a, { opcode::read_write::<cec_msg>(b'a', 5) }, cec_msg>;
type Receive<'a> = Updater<'a, { opcode::read_write::<cec_msg>(b'a', 6) }, cec_msg>;
type DqEvent<'a> = Updater<'a, { opcode::read_write::<cec_event>(b'a', 7) }, cec_event>;
type SetMode = Setter<{ opcode::write::<u32>(b'a', 9) }, u32>;

/// Every adapter a wall has claimed an address on, so that it can be handed
/// back when the wall stops.
static CLAIMED: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// Hand back every address the walls claimed.
///
/// The address belongs to the adapter rather than to the process, so without
/// this it outlives the wall: a Pi whose wall was stopped would go on
/// claiming a place on the bus and then answer nothing, and an LG does not
/// say when it switches away from a device like that. The wall showing on
/// another of its inputs would stay paused.
///
/// Only for a wall that is stopped, not one that crashes, which the kernel
/// gives no chance to. A crash is restarted within seconds, though, and the
/// wall adopts the address it left and answers for it again.
pub fn release() {
    for adapter in CLAIMED.lock().unwrap().iter() {
        let mut none = cec_log_addrs::default();
        let _ = unsafe { ioctl::ioctl(adapter, SetLogAddrs::new(&mut none)) };
    }
}

/// What a display's television tells the wall.
#[derive(Default)]
pub struct Tv {
    /// Whether it is showing the wall.
    pub watched: AtomicBool,
    /// The keys pressed on its remote since the wall last took them, as CEC
    /// numbers them, oldest first. What each does is the wall's business,
    /// since that depends on what it is showing.
    pub keys: Mutex<Vec<u8>>,
}

/// Watch the display on `connector` of `card`, and say whether it is being
/// watched, and what is pressed on its remote.
///
/// An error when it has no CEC adapter this process can use, saying why.
pub fn watch(card: u32, connector: u32, name: &str) -> Result<Arc<Tv>, String> {
    let adapter = find(card, connector)?;
    let tv = Arc::new(Tv {
        watched: AtomicBool::new(true),
        keys: Mutex::default(),
    });
    let bus = Bus {
        adapter,
        name: name.to_string(),
        tv: tv.clone(),
        ours: CEC_PHYS_ADDR_INVALID as u16,
        claimed: false,
        asleep: false,
        before_sleep: true,
        asked: None,
        questioned: None,
        refused: false,
    };
    if let Ok(dup) = bus.adapter.try_clone() {
        CLAIMED.lock().unwrap().push(dup);
    }
    std::thread::spawn(move || bus.run());
    Ok(tv)
}

/// The adapter that drives this connector, set up to hear the television.
///
/// Found by what the kernel says each adapter is wired to, not by number: a
/// Pi 5's second HDMI port is cec1 whatever the wall calls it.
fn find(card: u32, connector: u32) -> Result<File, String> {
    let mut names: Vec<String> = std::fs::read_dir("/dev")
        .map_err(|e| format!("could not list /dev: {e}"))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("cec"))
        .collect();
    names.sort();
    let mut refused = None;
    for n in &names {
        let path = format!("/dev/{n}");
        let file = match File::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(e) => {
                refused.get_or_insert(format!("{path}: {e}"));
                continue;
            }
        };
        let Ok(info) = (unsafe { ioctl::ioctl(&file, ConnectorInfo::new()) }) else {
            continue;
        };
        if info.type_ != CEC_CONNECTOR_TYPE_DRM {
            continue;
        }
        let drm = unsafe { info.__bindgen_anon_1.drm };
        if (drm.card_no, drm.connector_id) != (card, connector) {
            continue;
        }
        claim(&file).map_err(|e| format!("{path}: {e}"))?;
        return Ok(file);
    }
    // A node this process may not open is more likely the answer than a
    // display with no CEC at all, so say that one if there was one.
    Err(refused.unwrap_or_else(|| "no CEC adapter drives it".into()))
}

/// Take a logical address as a playback device, and ask to hear what the
/// bus says.
///
/// An adapter already configured is left as it is. The configuration
/// belongs to the adapter rather than to whoever set it, and outlives the
/// process: a wall that restarts finds its own from last time, and one that
/// cleared whatever it found would take the address from under anything
/// else on the board using it.
fn claim(file: &File) -> Result<(), String> {
    let have = unsafe { ioctl::ioctl(file, GetLogAddrs::new()) }
        .map_err(|e| format!("could not read its logical addresses: {e}"))?;
    if have.num_log_addrs == 0 {
        let mut want = cec_log_addrs {
            cec_version: CEC_OP_CEC_VERSION_2_0 as u8,
            num_log_addrs: 1,
            vendor_id: CEC_VENDOR_ID_NONE,
            ..have
        };
        want.log_addr_type[0] = CEC_LOG_ADDR_TYPE_PLAYBACK as u8;
        want.primary_device_type[0] = CEC_OP_PRIM_DEVTYPE_PLAYBACK as u8;
        want.all_device_types[0] = CEC_OP_ALL_DEVTYPE_PLAYBACK as u8;
        // What the television lists the input as.
        for (to, from) in want.osd_name.iter_mut().zip(b"viewpie") {
            *to = *from as _;
        }
        unsafe { ioctl::ioctl(file, SetLogAddrs::new(&mut want)) }
            .map_err(|e| format!("could not claim a logical address: {e}"))?;
    }
    // A follower, which is what hears the broadcasts. The kernel still
    // answers the questions it can on the wall's behalf, such as its
    // physical address and name.
    unsafe { ioctl::ioctl(file, SetMode::new(CEC_MODE_INITIATOR | CEC_MODE_FOLLOWER)) }
        .map_err(|e| format!("could not listen on it: {e}"))
}

/// What the television is doing, as far as this display's side of the bus
/// can tell.
struct Bus {
    adapter: File,
    name: String,
    tv: Arc<Tv>,
    /// This display's physical address: which input of the television, and
    /// of anything between, it is plugged into. Invalid while unplugged.
    ours: u16,
    /// Whether the wall has claimed the active source, and so answers for it.
    claimed: bool,
    /// Whether the television said it was going into standby, and has not
    /// been heard to wake since.
    asleep: bool,
    /// Whether the wall was watched when it did. A television wakes on the
    /// input it went to sleep on, and does not say which that is.
    before_sleep: bool,
    /// When the television was last asked whether it is on.
    asked: Option<Instant>,
    /// When anyone on the bus, this wall included, last asked which source
    /// is active.
    questioned: Option<Instant>,
    /// Whether the kernel has refused a message, which is said once.
    refused: bool,
}

/// How often a sleeping television may be asked whether it has woken, which
/// bounds how long after waking the wall notices.
const ASK_AGAIN: Duration = Duration::from_secs(5);

/// How long after someone asks which source is active the television's
/// "0.0.0.0" is taken for its answer, rather than for it switching to
/// itself. It answers within a few hundred milliseconds.
const ANSWER_WINDOW: Duration = Duration::from_secs(2);

impl Bus {
    fn run(mut self) {
        let addr = unsafe { ioctl::ioctl(&self.adapter, PhysAddr::new()) }
            .unwrap_or(CEC_PHYS_ADDR_INVALID as u16);
        self.address_changed(addr);
        loop {
            let mut fds = [PollFd::new(&self.adapter, PollFlags::IN | PollFlags::PRI)];
            if let Err(e) = poll(&mut fds, None) {
                eprintln!("{}: CEC: {e}; no longer watching", self.name);
                self.set(true, "");
                return;
            }
            let ready = fds[0].revents();
            if ready.contains(PollFlags::PRI) {
                self.event();
            }
            if ready.contains(PollFlags::IN) {
                let mut msg = cec_msg::default();
                if unsafe { ioctl::ioctl(&self.adapter, Receive::new(&mut msg)) }.is_ok() {
                    self.heard(&msg);
                }
            }
        }
    }

    /// Take the adapter's next event. The one that matters is a change of
    /// physical address, which is being unplugged and plugged back in.
    ///
    /// One per wake-up, not a loop until the queue is empty: the adapter is
    /// blocking, so asking for an event when there is none waits for the
    /// next, and the bus goes unheard meanwhile. poll says again if there
    /// are more.
    fn event(&mut self) {
        let mut event: cec_event = unsafe { std::mem::zeroed() };
        if unsafe { ioctl::ioctl(&self.adapter, DqEvent::new(&mut event)) }.is_err() {
            return;
        }
        if event.event == CEC_EVENT_STATE_CHANGE {
            let change = unsafe { event.__bindgen_anon_1.state_change };
            if change.phys_addr != self.ours {
                self.address_changed(change.phys_addr);
            }
        }
    }

    /// Start over from not knowing: a display just plugged in, or first
    /// looked at, is taken to be watched until the television says
    /// otherwise.
    ///
    /// And asked. Whether the television is on is a question it answers
    /// truthfully, and so is which source is active when another device
    /// has claimed it. When nobody has, the television answers with itself
    /// even while it is showing this very input, so that answer is not
    /// taken to mean anything.
    fn address_changed(&mut self, addr: u16) {
        self.ours = addr;
        self.claimed = false;
        self.asleep = false;
        if addr == CEC_PHYS_ADDR_INVALID as u16 {
            // Unplugged, which the wall hears from the connector.
            return;
        }
        self.set(true, "");
        self.ask_power();
        self.ask_who();
    }

    /// Ask the television whether it is on.
    fn ask_power(&mut self) {
        self.asked = Some(Instant::now());
        self.send(
            CEC_LOG_ADDR_TV as u8,
            &[CEC_MSG_GIVE_DEVICE_POWER_STATUS as u8],
        );
    }

    /// Ask whoever holds the screen to say so.
    fn ask_who(&mut self) {
        self.questioned = Some(Instant::now());
        self.send(
            CEC_LOG_ADDR_BROADCAST as u8,
            &[CEC_MSG_REQUEST_ACTIVE_SOURCE as u8],
        );
    }

    fn heard(&mut self, msg: &cec_msg) {
        let len = (msg.len as usize).min(msg.msg.len());
        let m = &msg.msg[..len];
        if m.len() < 2 {
            // A poll: somebody checking that the address is taken.
            return;
        }
        let from = m[0] >> 4;
        let to = m[0] & 0xf;
        let op = m[1] as u32;
        let addr = |at: usize| (m.len() >= at + 2).then(|| u16::from_be_bytes([m[at], m[at + 1]]));

        // A television that went to sleep is not necessarily heard to wake:
        // it may say nothing about which input it woke on. It is not quiet
        // while asleep either, though -- an LG in standby broadcasts its
        // vendor id every second or two -- so hearing from it is the cue to
        // ask whether it is on, but not every time.
        if self.asleep
            && from == CEC_LOG_ADDR_TV as u8
            && op != CEC_MSG_REPORT_POWER_STATUS
            && self.asked.is_none_or(|at| at.elapsed() >= ASK_AGAIN)
        {
            self.ask_power();
        }

        match op {
            // The television switched inputs, or asked the device at an
            // address to show itself: either way, that address is what is
            // on screen now.
            CEC_MSG_ROUTING_CHANGE => {
                if let Some(to) = addr(4) {
                    self.showing(to, format!("the television switched to {}", source(to)));
                }
            }
            CEC_MSG_ROUTING_INFORMATION | CEC_MSG_SET_STREAM_PATH => {
                if let Some(to) = addr(2) {
                    self.showing(to, format!("the television switched to {}", source(to)));
                }
            }
            // Another source claiming the screen, never this wall itself,
            // which the kernel does not echo back. 0.0.0.0 is the television,
            // which says so unprompted when it switches to its tuner or apps,
            // and in answer to a question nobody else answered -- even while
            // it shows the wall. Only the first means anything.
            CEC_MSG_ACTIVE_SOURCE => {
                let answer = self
                    .questioned
                    .is_some_and(|at| at.elapsed() < ANSWER_WINDOW);
                if let Some(at) = addr(2).filter(|&at| at != 0 || !answer) {
                    self.showing(at, format!("{} took the screen", source(at)));
                }
            }
            CEC_MSG_STANDBY => self.sleep("the television went into standby"),
            CEC_MSG_REPORT_POWER_STATUS if from == CEC_LOG_ADDR_TV as u8 => {
                let status = m.get(2).copied().unwrap_or(0) as u32;
                if status == CEC_OP_POWER_STATUS_STANDBY || status == CEC_OP_POWER_STATUS_TO_STANDBY
                {
                    self.sleep("the television is in standby");
                } else if self.asleep {
                    // Awake, and on the input it went to sleep on, which an
                    // LG does not say: asked who is showing, it answers with
                    // itself whichever that is. So the wall goes back to
                    // what it was, and whoever holds the screen is asked to
                    // say so, in case something woke the television by
                    // taking it.
                    self.asleep = false;
                    self.set(self.before_sleep, "the television woke up");
                    self.ask_who();
                }
            }
            CEC_MSG_REQUEST_ACTIVE_SOURCE => {
                self.questioned = Some(Instant::now());
                if self.claimed {
                    self.claim();
                }
            }
            CEC_MSG_GIVE_DEVICE_POWER_STATUS if to != CEC_LOG_ADDR_BROADCAST as u8 => {
                self.send(
                    from,
                    &[
                        CEC_MSG_REPORT_POWER_STATUS as u8,
                        CEC_OP_POWER_STATUS_ON as u8,
                    ],
                );
            }
            // A key on the television's remote, which it passes on to
            // whatever it is showing. None is refused: some televisions stop
            // passing keys to a device that refuses one. A key held down
            // comes again every half second or so.
            CEC_MSG_USER_CONTROL_PRESSED => {
                if let Some(&code) = m.get(2) {
                    self.tv.keys.lock().unwrap().push(code);
                }
            }
            CEC_MSG_USER_CONTROL_RELEASED => {}
            // What a device owes the sender of a directed message it has no
            // answer for, rather than letting it wait out a timeout. Never in
            // answer to one, which would go back and forth for ever.
            CEC_MSG_FEATURE_ABORT => {}
            _ if to != CEC_LOG_ADDR_BROADCAST as u8 => {
                self.send(
                    from,
                    &[
                        CEC_MSG_FEATURE_ABORT as u8,
                        op as u8,
                        CEC_OP_ABORT_UNRECOGNIZED_OP as u8,
                    ],
                );
            }
            _ => {}
        }
    }

    /// The television is showing whatever is at `at`, which `why` says
    /// unless that is this display.
    ///
    /// When it is, the wall claims the active source, so that the next wall
    /// to start, or this one after a restart, can ask who is on screen and
    /// get a true answer.
    fn showing(&mut self, at: u16, why: String) {
        // Awake, whatever it said last: a television that says what it is
        // showing is showing something. Otherwise one that wakes on another
        // input and says so would be overruled by its answer to whether it
        // is on, which only says that it is.
        self.asleep = false;
        if at == self.ours {
            if !self.claimed {
                self.claim();
            }
            self.set(true, "the television switched to it");
        } else {
            self.claimed = false;
            self.set(self.below(at), &why);
        }
    }

    /// The television is in standby. Whether the wall was watched is kept
    /// from the first word of it, not the repeats: an LG says so again now
    /// and then while it sleeps.
    fn sleep(&mut self, why: &str) {
        if !self.asleep {
            self.asleep = true;
            self.before_sleep = self.tv.watched.load(Ordering::Relaxed);
        }
        self.claimed = false;
        self.set(false, why);
    }

    /// Whether showing `at` could be showing this display.
    ///
    /// Yes for this display's own address, and for any switch it sits
    /// behind: a receiver at 1.0.0.0 with the wall on its first input is
    /// showing the wall unless its own input is elsewhere, which only it
    /// knows. Not for 0.0.0.0, although it is above everything: that is the
    /// television showing its own tuner or apps.
    fn below(&self, at: u16) -> bool {
        if at == 0 || self.ours == CEC_PHYS_ADDR_INVALID as u16 {
            return false;
        }
        // The depth of `at` is how many of its four digits are set; this
        // display is below it if they match its own first ones.
        let depth = (0..4).take_while(|&i| digit(at, i) != 0).count();
        let mask = (0xffff_0000_u32 >> (4 * depth)) as u16;
        self.ours & mask == at
    }

    fn claim(&mut self) {
        self.claimed = true;
        let [hi, lo] = self.ours.to_be_bytes();
        self.send(
            CEC_LOG_ADDR_BROADCAST as u8,
            &[CEC_MSG_ACTIVE_SOURCE as u8, hi, lo],
        );
    }

    fn set(&self, watched: bool, why: &str) {
        if self.tv.watched.swap(watched, Ordering::Relaxed) != watched && !why.is_empty() {
            println!("{}: {why}", self.name);
        }
    }

    /// Send `body` to logical address `to`, and wait for the bus to take it.
    ///
    /// Nobody answering is not said: a bus with nobody else on it NACKs
    /// everything, and a missed reply is only a question asked again at the
    /// next change. A message the kernel refuses is, once, since it means the
    /// wall cannot say anything at all.
    fn send(&mut self, to: u8, body: &[u8]) {
        // From the adapter's own logical address, which the kernel wants
        // written in rather than filling it in: a message that names another
        // is refused. Read each time, since it is claimed again whenever
        // the display is plugged back in, and messages are few.
        let Ok(addrs) = (unsafe { ioctl::ioctl(&self.adapter, GetLogAddrs::new()) }) else {
            return;
        };
        let me = addrs.log_addr[0];
        if addrs.num_log_addrs == 0 || me as u32 == CEC_LOG_ADDR_INVALID {
            return;
        }
        let mut msg = cec_msg {
            len: 1 + body.len() as u32,
            ..Default::default()
        };
        msg.msg[0] = (me << 4) | to;
        msg.msg[1..=body.len()].copy_from_slice(body);
        if let Err(e) = unsafe { ioctl::ioctl(&self.adapter, Transmit::new(&mut msg)) } {
            if !self.refused {
                self.refused = true;
                eprintln!("{}: CEC: could not send: {e}", self.name);
            }
        }
    }
}

/// What a physical address is, for the log: the television's own sources
/// by name, anything else as CEC writes it.
fn source(addr: u16) -> String {
    if addr == 0 {
        "its own tuner or apps".into()
    } else {
        show(addr)
    }
}

/// A physical address the way CEC writes one: 1.0.0.0 is the television's
/// first input.
fn show(addr: u16) -> String {
    let d = |i| digit(addr, i);
    format!("{:x}.{:x}.{:x}.{:x}", d(0), d(1), d(2), d(3))
}

/// The `i`th of a physical address's four digits, from the television down.
fn digit(addr: u16, i: u16) -> u16 {
    (addr >> (12 - 4 * i)) & 0xf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ours: u16) -> Bus {
        Bus {
            adapter: File::open("/dev/null").unwrap(),
            name: String::new(),
            tv: Arc::default(),
            ours,
            claimed: false,
            asleep: false,
            before_sleep: true,
            asked: None,
            questioned: None,
            refused: false,
        }
    }

    #[test]
    fn a_display_is_below_its_own_input_and_every_switch_above_it() {
        let wall = at(0x1200);
        assert!(wall.below(0x1200));
        assert!(wall.below(0x1000));
        assert!(!wall.below(0x1100));
        assert!(!wall.below(0x2000));
        assert!(
            !wall.below(0x1210),
            "a device behind the wall is not the wall"
        );
        assert!(at(0x1234).below(0x1234));
    }

    /// A message as it comes off the bus, from `from` to `to`.
    fn msg(from: u8, to: u8, body: &[u8]) -> cec_msg {
        let mut m = cec_msg {
            len: 1 + body.len() as u32,
            ..Default::default()
        };
        m.msg[0] = (from << 4) | to;
        m.msg[1..=body.len()].copy_from_slice(body);
        m
    }

    fn tv_is_active(bus: &mut Bus) {
        bus.heard(&msg(0, 15, &[CEC_MSG_ACTIVE_SOURCE as u8, 0, 0]));
    }

    #[test]
    fn the_television_taking_the_screen_for_itself_pauses_the_wall() {
        let mut wall = at(0x1000);
        wall.tv.watched.store(true, Ordering::Relaxed);
        tv_is_active(&mut wall);
        assert!(!wall.tv.watched.load(Ordering::Relaxed));
    }

    #[test]
    fn the_television_answering_for_nobody_does_not() {
        let mut wall = at(0x1000);
        wall.tv.watched.store(true, Ordering::Relaxed);
        // Another wall starting up asks who holds the screen, and nobody
        // having claimed it, the television answers with itself.
        wall.heard(&msg(8, 15, &[CEC_MSG_REQUEST_ACTIVE_SOURCE as u8]));
        tv_is_active(&mut wall);
        assert!(wall.tv.watched.load(Ordering::Relaxed));
    }

    #[test]
    fn a_switch_to_the_wall_resumes_it() {
        let mut wall = at(0x1000);
        wall.heard(&msg(
            0,
            15,
            &[CEC_MSG_ROUTING_CHANGE as u8, 0x10, 0, 0x30, 0],
        ));
        assert!(!wall.tv.watched.load(Ordering::Relaxed));
        wall.heard(&msg(
            0,
            15,
            &[CEC_MSG_ROUTING_CHANGE as u8, 0x30, 0, 0x10, 0],
        ));
        assert!(wall.tv.watched.load(Ordering::Relaxed));
        assert!(wall.claimed, "on screen, the wall answers for the screen");
    }

    #[test]
    fn a_television_that_wakes_elsewhere_and_says_so_keeps_the_wall_paused() {
        let mut wall = at(0x1000);
        wall.tv.watched.store(true, Ordering::Relaxed);
        wall.heard(&msg(0, 15, &[CEC_MSG_STANDBY as u8]));
        // Asleep on the wall, it was woken onto its apps, said so, and then
        // answered that it is on.
        tv_is_active(&mut wall);
        wakes(&mut wall);
        assert!(!wall.tv.watched.load(Ordering::Relaxed));
    }

    fn wakes(wall: &mut Bus) {
        wall.heard(&msg(
            0,
            4,
            &[
                CEC_MSG_REPORT_POWER_STATUS as u8,
                CEC_OP_POWER_STATUS_ON as u8,
            ],
        ));
    }

    #[test]
    fn a_television_wakes_on_the_input_it_slept_on() {
        let mut wall = at(0x1000);
        wall.tv.watched.store(true, Ordering::Relaxed);
        tv_is_active(&mut wall);
        wall.heard(&msg(0, 15, &[CEC_MSG_STANDBY as u8]));
        wall.heard(&msg(0, 15, &[CEC_MSG_STANDBY as u8]));
        wakes(&mut wall);
        assert!(
            !wall.tv.watched.load(Ordering::Relaxed),
            "asleep on its apps, it wakes on them"
        );

        wall.heard(&msg(0, 15, &[CEC_MSG_ROUTING_CHANGE as u8, 0, 0, 0x10, 0]));
        wall.heard(&msg(0, 15, &[CEC_MSG_STANDBY as u8]));
        wakes(&mut wall);
        assert!(
            wall.tv.watched.load(Ordering::Relaxed),
            "asleep on the wall, it wakes on it"
        );
    }

    #[test]
    fn a_television_that_wakes_without_a_word_is_watched() {
        let mut wall = at(0x1000);
        wall.tv.watched.store(true, Ordering::Relaxed);
        wall.heard(&msg(0, 15, &[CEC_MSG_STANDBY as u8]));
        wakes(&mut wall);
        assert!(wall.tv.watched.load(Ordering::Relaxed));
    }

    #[test]
    fn the_television_itself_is_not_the_wall() {
        assert!(!at(0x1000).below(0x0000));
    }

    #[test]
    fn nothing_is_the_wall_while_it_is_unplugged() {
        assert!(!at(0xffff).below(0x1000));
    }

    #[test]
    fn keys_are_passed_on_in_the_order_pressed() {
        let mut wall = at(0x1000);
        for key in [CEC_OP_UI_CMD_RIGHT, CEC_OP_UI_CMD_DOWN, CEC_OP_UI_CMD_BACK] {
            wall.heard(&msg(0, 4, &[CEC_MSG_USER_CONTROL_PRESSED as u8, key as u8]));
            wall.heard(&msg(0, 4, &[CEC_MSG_USER_CONTROL_RELEASED as u8]));
        }
        assert_eq!(*wall.tv.keys.lock().unwrap(), [0x04, 0x02, 0x0d]);
    }

    #[test]
    fn addresses_are_written_as_cec_writes_them() {
        assert_eq!(show(0x1000), "1.0.0.0");
        assert_eq!(show(0x3a0f), "3.a.0.f");
    }
}
