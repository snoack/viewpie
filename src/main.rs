//! viewpie: a camera wall built on ffmpeg and one atomic KMS commit.
//!
//! Every viewport on a display goes up in a single atomic commit, so each
//! advances on that display's own vsync rather than sharing a page-flip
//! budget: the alternative, one flip per viewport, divides the refresh rate
//! by the number of viewports. There is no X11, Wayland or compositor
//! underneath; the wall talks to the kernel directly.
//!
//! Feeds are decoded by ffmpeg/libav, in hardware where the board has a block
//! for the codec. A hardware-decoded frame is imported by its dmabuf and
//! never copied, including the column-tiled format a Pi's HEVC decoder emits;
//! a software-decoded one is copied into a buffer of our own. A viewport may
//! rotate through several feeds, and the wall is described by a YAML
//! configuration.
//!
//! Recovery is per feed: a camera that goes away is reconnected on a backoff
//! and its viewport blanked meanwhile, a display that is not there yet is
//! waited for, and anything worse is left to systemd restarting the process.

mod cec;
mod config;
mod decode;
mod ffi;
mod layout;
mod present;

use config::Config;
use decode::{Decoder, FeedStats, Refused};
use drm::buffer::{Buffer, DrmFourcc, DrmModifier, Handle};
use drm::control::{
    connector, crtc, framebuffer, plane, property, Device as ControlDevice, FbCmd2Flags,
};
use ffi::*;
use present::{Card, Fb, ImportedFrame, Viewport};
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One camera's latest frame, published by its decoder thread and read by the
/// commit loop for every viewport that shows it.
///
/// One per feed however many viewports name it: a camera shown twice is
/// still one connection and one decoder, and both viewports scan out the
/// same buffer.
struct Source {
    /// What the logs call this feed, as the configuration names it.
    name: String,
    /// The newest frame this feed has decoded.
    ///
    /// One lock for the framebuffer and its size together, because they are
    /// only meaningful as a pair: reading them separately can hand the commit
    /// loop one frame's buffer with another frame's dimensions, and a source
    /// rectangle larger than the buffer is refused with EINVAL.
    ///
    /// Shared rather than copied out: a commit takes its own reference to
    /// what it names, so whatever the feed publishes afterwards, the
    /// framebuffer and the decoder's buffer behind it live until the display
    /// has flipped past them.
    shown: Mutex<Option<Arc<Shown>>>,
    stats: Arc<FeedStats>,
    /// Whether any display showing this feed is being watched. Always, unless
    /// the wall pauses off screen; while it is not, the feed disconnects and
    /// decodes nothing.
    wanted: AtomicBool,
    /// Whether it is a camera's stream for fullscreen, connected only while
    /// a display shows it, and not worth a line in the report otherwise.
    on_demand: bool,
}

impl Source {
    /// A feed that has not decoded anything yet.
    fn new(name: &str) -> Self {
        Source {
            name: name.to_string(),
            shown: Mutex::new(None),
            stats: Arc::default(),
            wanted: AtomicBool::new(true),
            on_demand: false,
        }
    }

    /// Its newest frame, if it has one, and how many it has published by
    /// then, which is how a Tap tells a new frame from one it has shown.
    fn frame(&self) -> (Option<Arc<Shown>>, u64) {
        let shown = self.shown.lock().unwrap().clone();
        (shown, self.stats.published.load(Ordering::Acquire))
    }

    /// A camera's stream for fullscreen, which waits to be wanted before it
    /// connects at all.
    fn on_demand(name: &str) -> Self {
        Source {
            wanted: AtomicBool::new(false),
            on_demand: true,
            ..Source::new(name)
        }
    }
}

/// One viewport's view of one feed: how the frames the feed published fared
/// on the way to this viewport.
///
/// Apart from the Source because the numbers are the viewport's, not the
/// camera's. The same 25fps feed can present every frame on a 60Hz display
/// and skip some on a 24Hz one, and a rotating viewport only presents while
/// the feed is its turn. Only the commit loop touches these.
struct Tap {
    source: Arc<Source>,
    /// How many times a *new* frame from this feed reached a commit.
    ///
    /// Counting commits would flatter the wall: the loop commits every
    /// viewport every pass, so a feed that stalled would keep scoring while
    /// its last frame sat on screen. Only a framebuffer the loop has not
    /// already presented counts.
    presented: u64,
    /// Frames the decoder published that no commit ever picked up for this
    /// viewport, because a newer one replaced them first. This is the number
    /// that says whether the presenter is keeping up with the feed. Only
    /// while the feed is the one on screen: the frames a rotating viewport
    /// does not show while it shows another feed were never its to show.
    skipped: u64,
    /// The generation this viewport last committed from this feed. Per feed
    /// rather than per viewport, because a rotating viewport switches between
    /// feeds whose counts have nothing to do with each other: comparing one
    /// against the other's would report a frame as skipped that never
    /// existed.
    last_seen: u64,
    /// What `presented` and the time on screen read at the previous report,
    /// which the next reports what happened since. Kept rather than worked
    /// out again from the clock: a rotation stepped on the remote since then
    /// counts from the step, and has forgotten what it would have said
    /// before it.
    reported: (u64, Duration),
}

impl Tap {
    fn new(source: Arc<Source>) -> Self {
        Tap {
            source,
            presented: 0,
            skipped: 0,
            last_seen: 0,
            reported: (0, Duration::ZERO),
        }
    }

    /// Say how the feed fared on its way to the screen since the previous
    /// report, as `label`, given how long it has been on screen in all.
    ///
    /// Presenting only happens while a feed is the one on screen, so it is
    /// divided by however much of the window that was: a rotating pair would
    /// otherwise each read half their real rate, which looks exactly like a
    /// presenter falling behind. Skipped is a total, like errors: frames lost
    /// are running evidence.
    ///
    /// Counted whether or not it is said, `say` being false, so that the
    /// next report that does say it covers only its own window.
    fn report(&mut self, label: &str, on_screen: Duration, say: bool) {
        let rate = match self.rate(on_screen) {
            Some(rate) => format!("{rate:.1}"),
            // Off screen for the whole window, so it presented nothing and
            // there is no span to divide by. Said as a dash rather than 0.0,
            // which would read as a feed that failed.
            None => "-".to_string(),
        };
        if say {
            println!("  {label}: presented {rate} fps  skipped={}", self.skipped);
        }
    }

    /// The rate the feed presented at since the previous report, given how
    /// long it has been on screen in all, or None if it has not been on
    /// screen since; and this taken as the previous report from now on.
    fn rate(&mut self, on_screen: Duration) -> Option<f64> {
        let (presented, was) = std::mem::replace(&mut self.reported, (self.presented, on_screen));
        let span = on_screen.saturating_sub(was).as_secs_f64();
        (span > 0.0).then(|| (self.presented - presented) as f64 / span)
    }

    /// Count a commit that shows generation `gen` of this feed. `returning`
    /// when the feed was not on screen at the commit before, so the frames it
    /// published meanwhile are not counted as skipped: nothing was trying to
    /// show them.
    fn count(&mut self, gen: u64, returning: bool) {
        if gen == self.last_seen {
            return;
        }
        self.presented += 1;
        if !returning {
            self.skipped += gen.saturating_sub(self.last_seen).saturating_sub(1);
        }
        self.last_seen = gen;
    }
}

/// One viewport's feeds, and which of them it is showing.
///
/// A viewport that rotates keeps every feed decoding, not just the visible
/// one: switching is then an index, and the incoming feed already has a frame
/// ready. Decoding only the visible one would cost a black viewport for as
/// long as the next feed took to produce its first frame, which is the thing
/// that made switching slow when the pipeline had to be rebuilt instead.
///
/// Which feed is showing is worked out from the wall's clock rather than kept:
/// the turn is the time since the rotation started divided by the interval.
/// Every viewport with the same interval therefore switches on the same
/// commit, on every display, however long the wall runs. A thread per
/// viewport sleeping out its interval drifted apart from its neighbours by
/// however late each wake-up was, and could land either side of a commit
/// even when it had not.
///
/// The rotation starts with the wall, and again whenever someone steps it on
/// the remote, from the feed they stepped to: a feed stepped to gets a whole
/// interval before the rotation takes over again. Every viewport on the
/// display is started again at the same moment, so the ones that switched
/// together go on doing so. The other display's are left alone, and no
/// longer switch with these.
struct Feeds {
    taps: Vec<Tap>,
    /// Which feed the last commit showed, so one coming back on screen can be
    /// told from one that stayed.
    last_active: Option<usize>,
    /// How long each feed holds the viewport, in milliseconds. Never zero:
    /// the configuration refuses an interval under a second.
    interval: u64,
    /// When on the wall's clock the rotation started, and which feed it
    /// started from.
    since: Duration,
    first: usize,
    /// How long each feed had been on screen, in total, by `since`.
    credited: Vec<Duration>,
}

impl Feeds {
    fn new(taps: Vec<Tap>, interval: Duration) -> Self {
        Feeds {
            credited: vec![Duration::ZERO; taps.len()],
            taps,
            last_active: None,
            interval: millis(interval),
            since: Duration::ZERO,
            first: 0,
        }
    }

    /// Which feed is on screen `clock` into the wall's run.
    fn active(&self, clock: Duration) -> usize {
        let turn = millis(clock.saturating_sub(self.since)) / self.interval;
        ((self.first as u64 + turn) % self.taps.len() as u64) as usize
    }

    /// How long feed `n` has been on screen, in total, `clock` into the
    /// wall's run.
    ///
    /// Worked out rather than counted, for the same reason the turn is: it
    /// follows from the clock. Each full cycle since the rotation started
    /// gives every feed one interval, and the cycle in progress gives feed
    /// `n` whatever of its own turn has passed.
    fn on_screen(&self, n: usize, clock: Duration) -> Duration {
        let interval = self.interval;
        let len = self.taps.len();
        let cycle = interval.saturating_mul(len as u64);
        let ran = millis(clock.saturating_sub(self.since));
        let (full, into) = (ran / cycle, ran % cycle);
        // How many turns into the cycle feed `n`'s comes.
        let later = ((n + len - self.first) % len) as u64;
        let partial = into.saturating_sub(later * interval).min(interval);
        self.credited[n] + Duration::from_millis(full * interval + partial)
    }

    /// Step `by` feeds from the one on screen `clock` into the wall's run,
    /// back when negative, and start the rotation again from there.
    ///
    /// Nothing for a viewport showing a single feed, which has no rotation
    /// to step.
    fn step(&mut self, by: i32, clock: Duration) {
        let len = self.taps.len();
        if len < 2 {
            return;
        }
        self.credited = (0..len).map(|n| self.on_screen(n, clock)).collect();
        let at = self.active(clock) as i64;
        self.first = (at + i64::from(by)).rem_euclid(len as i64) as usize;
        self.since = clock;
    }
}

/// A span in whole milliseconds, which is as fine as a rotation needs: a
/// display commits every 16ms at best. Saturating rather than wrapping, since
/// a Duration can hold more milliseconds than a u64 can, though no interval or
/// uptime a wall will ever see comes near that.
fn millis(d: Duration) -> u64 {
    d.as_millis().try_into().unwrap_or(u64::MAX)
}

/// A framebuffer, the frame that owns its memory, and the size to read it at.
///
/// Every holder keeps both alive: the source until a newer frame replaces it,
/// and each display from the commit that names it until the flip after the
/// one that took it off screen. Whichever lets go last frees them, so nothing
/// has to know who else might still be scanning a buffer out.
struct Shown {
    fb: Arc<Fb>,
    width: u32,
    height: u32,
    /// Dropping the frame returns its capture buffer to the decoder. None
    /// for a software-decoded frame, whose pixels have already been copied
    /// into `fb` and need not be kept.
    _frame: Option<decode::Frame>,
}

const USAGE: &str = "\
usage: viewpie --config <file>
       viewpie --check-libav

Drives a wall of camera feeds from one atomic KMS commit. The wall is
described entirely by the configuration file; the package installs a commented
example of one at /etc/viewpie/viewpie.yaml.

--check-libav says whether the installed libav can reach the Pi's hardware
decoders, and exits 1 if it cannot.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // One way in, and it is the file. Urls on the command line were how the
    // prototype was tried out before there was a configuration to write, and
    // keeping them meant every unrecognised argument, --help included, was
    // read as a camera url: asking for help brought a wall up on the screen
    // with the word "--help" as its one feed. A wall is not a thing to start
    // by accident.
    let path = match args.split_first() {
        Some((flag, rest)) if flag == "--check-libav" => match rest {
            [] => check_libav(),
            [extra, ..] => fail(&format!("unexpected argument '{extra}'")),
        },
        Some((flag, rest)) if flag == "--config" || flag == "-c" => match rest {
            [path] => path,
            // Named rather than ignored: a second path here is a typo, or a
            // url left over from the older spelling, and silently running
            // with the first one hides both.
            [] => fail(&format!("{flag} needs a file")),
            [_, extra, ..] => fail(&format!("unexpected argument '{extra}'")),
        },
        Some((arg, _)) => fail(&format!("unexpected argument '{arg}'")),
        None => fail("no configuration given"),
    };

    let config = match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text).map_err(|e| format!("{path}: {e}")),
        Err(e) => Err(format!("{path}: {e}")),
    };

    let config = match config {
        Ok(config) => config,
        Err(e) => {
            eprintln!("viewpie: {e}");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&config) {
        eprintln!("viewpie: {e}");
        std::process::exit(1);
    }
}

/// Say which hardware decode paths the installed libav has, and exit 1 if it
/// lacks any.
///
/// For the image build, which runs it right after installing the package.
/// Debian's libav satisfies the package's dependencies as well as Raspberry Pi
/// OS's does, so apt cannot tell a wall that decodes H.265 in software from
/// one that does not; the image would build either way and the difference
/// would show up as load on a Pi. The board itself is not asked: an image is
/// built on whatever machine runs Docker.
fn check_libav() -> ! {
    let mut ok = true;
    for path in &decode::HW_PATHS {
        let found = (path.in_libav)();
        println!(
            "{}: {} {}",
            path.codec,
            path.libav,
            if found { "found" } else { "missing" }
        );
        ok &= found;
    }
    std::process::exit(if ok { 0 } else { 1 });
}

/// Refuse the command line and say how it should have read.
///
/// Exit 2, the code the service unit treats as a configuration that cannot
/// work: restarting cannot fix an argument, so the unit stops and leaves the
/// message legible rather than burying it under a restart every five seconds.
fn fail(why: &str) -> ! {
    eprintln!("viewpie: {why}\n\n{USAGE}");
    std::process::exit(2);
}

/// One display: where it scans out, what it shows, and what it owes the
/// kernel before a buffer can be freed.
struct Wall {
    out: Output,
    /// Geometry, one per viewport, in the order the display lists them.
    viewports: Vec<Viewport>,
    /// The feeds behind those viewports, indexed in lockstep.
    feeds: Vec<Feeds>,
    /// The crtc's ACTIVE property, looked up once so every commit can name
    /// the crtc without an ioctl per frame to find it again.
    crtc_active: property::Handle,
    /// What each viewport's plane is scanning out, as of the last flip,
    /// indexed in lockstep with the others; None for a plane that is not
    /// attached to the crtc at all. Held so the frame and its
    /// framebuffer outlive every commit that could still be reading them:
    /// a feed that reconnects, or a decoder that wants its buffer back,
    /// cannot take them out from under the display.
    ///
    /// Per display, because a flip on one output says nothing about what the
    /// other is still scanning out.
    on_screen: Vec<Option<Arc<Shown>>>,
    /// The commit awaiting its flip, or None when the display owes one.
    /// Each is paced by its own flips, which is why one loop can drive both:
    /// two outputs are not phase locked and need not share a refresh rate.
    in_flight: Option<InFlight>,
    commits: u64,
    /// Whether the wall is showing its feeds. Only ever false for a wall that
    /// pauses off screen, while nobody is watching it.
    watched: bool,
    /// How many times its grid has gone off screen or come back, paused or
    /// swapped for one feed across the whole screen, so a report can tell a
    /// window that the grid was not on screen for all of.
    turns: u64,
    /// How to tell, for a wall that pauses off screen.
    watch: Option<Watch>,
    /// What the television says over CEC, or None for a display without it,
    /// whose remote does nothing and which pauses only for being unplugged.
    tv: Option<Arc<cec::Tv>>,
    /// Every camera the display shows, each once, in the order its viewports
    /// show them: what up and down on the remote go through, fullscreen.
    cameras: Vec<Camera>,
    /// The display's size, which a feed fullscreen is stretched over.
    size: (u32, u32),
    mode: Mode,
    /// When a key was last pressed on the remote, so a feed left fullscreen
    /// goes back to the grid when nobody has touched it for a while.
    pressed: Instant,
    /// What the planes are laid out for: a mode, or None once every one has
    /// been let go of. When it is not what the display is to show, which is
    /// nothing while it is paused, every plane is let go of first, and the
    /// new picture goes up once that has flipped.
    ///
    /// The scaler's line buffers are why. Every plane that scales holds one,
    /// which on a Pi 3 is every plane showing 4:2:0 video, since its colour
    /// is half the height of the picture. A grid of nine takes most of the
    /// 48K words there are, and a plane keeps its buffer until the commit
    /// replacing it has flipped. So a commit going straight from a camera
    /// fullscreen back to the grid asks for the fullscreen plane's buffer
    /// and the grid's at once, and is refused with ENOSPC, over and over.
    /// Letting go first costs a frame of background, and each layout starts
    /// from an empty pool.
    laid_out: Option<Mode>,
    /// How the picture fared, while a camera is fullscreen, and since when
    /// on the wall's clock the feed it counts has been that picture. Started
    /// afresh whenever the layout changes, and whenever the picture changes
    /// from the grid's stream to the one for fullscreen.
    fullscreen_tap: Option<(Tap, Duration)>,
}

/// A camera a display can show fullscreen.
struct Camera {
    /// The feed its viewports show.
    feed: Arc<Source>,
    /// Its sharper stream for fullscreen, if it has one. Shown once it has a
    /// frame, and the feed scaled up until then, so that changing camera is
    /// never a black screen while a stream connects.
    fullscreen: Option<Arc<Source>>,
}

/// What a display is showing.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// Its viewports, as the configuration lays them out.
    Grid,
    /// One of its cameras across the whole screen, by index in
    /// Wall::cameras.
    Fullscreen(usize),
}

/// How long a feed stays fullscreen with nobody pressing anything, before
/// the display goes back to its grid. A wall is for leaving on, and one left
/// showing a single camera would go on hiding all the others.
const FULLSCREEN_FOR: Duration = Duration::from_secs(10 * 60);

/// What a key on the remote does.
#[derive(PartialEq, Debug)]
enum Move {
    /// Step every rotation on the display by this many feeds.
    Step(i32),
    /// Show this instead.
    Show(Mode),
}

/// What key `code` does to a display in `mode` that shows `cameras` feeds,
/// or None for a key that does nothing there.
///
/// On the grid, left and right step the rotations, and down and up show the
/// first or last camera fullscreen. Fullscreen, down and up go on to the next
/// or previous camera, round from the end to the start, and back returns to
/// the grid. Left and right do nothing there, so that a stray press does not
/// change the grid being returned to.
fn key_move(mode: Mode, code: u8, cameras: usize) -> Option<Move> {
    let last = cameras.checked_sub(1)?;
    Some(match (code as u32, mode) {
        (CEC_OP_UI_CMD_RIGHT, Mode::Grid) => Move::Step(1),
        (CEC_OP_UI_CMD_LEFT, Mode::Grid) => Move::Step(-1),
        (CEC_OP_UI_CMD_DOWN, Mode::Grid) => Move::Show(Mode::Fullscreen(0)),
        (CEC_OP_UI_CMD_UP, Mode::Grid) => Move::Show(Mode::Fullscreen(last)),
        (CEC_OP_UI_CMD_DOWN, Mode::Fullscreen(at)) => {
            Move::Show(Mode::Fullscreen(if at >= last { 0 } else { at + 1 }))
        }
        (CEC_OP_UI_CMD_UP, Mode::Fullscreen(at)) => {
            Move::Show(Mode::Fullscreen(at.checked_sub(1).unwrap_or(last)))
        }
        (CEC_OP_UI_CMD_BACK, Mode::Fullscreen(_)) => Move::Show(Mode::Grid),
        _ => return None,
    })
}

/// How a wall that pauses off screen tells whether anyone is watching.
struct Watch {
    /// Whether the connector was connected when last looked at.
    connected: bool,
    /// When that was.
    looked: Instant,
}

/// How often a wall that pauses off screen looks at whether its display is
/// still plugged in. Only the connector's state as the kernel last saw it,
/// with no probing, so it costs one ioctl.
const CONNECTOR_POLL: Duration = Duration::from_secs(1);

/// A commit that has not flipped yet.
struct InFlight {
    /// When it was made, so a flip that is overdue can be said to be.
    at: Instant,
    /// Whether that has been said, so it is said once per commit rather
    /// than on every pass while it stays overdue.
    overdue_said: bool,
    /// What it named, by viewport index. Becomes on_screen once it has
    /// flipped; until then the old picture is still being read, so both are
    /// kept.
    named: Vec<(usize, Option<Arc<Shown>>)>,
}

impl Wall {
    /// The commit in flight has reached the screen: what it named is now
    /// what is being scanned out, what it replaced can go, and the display
    /// owes its next commit.
    ///
    /// Dropping the replaced frames is what frees them, and possibly returns
    /// their buffers to the decoder, unless some other holder still has them.
    fn flipped(&mut self) {
        let Some(commit) = self.in_flight.take() else {
            return;
        };
        for (i, shown) in commit.named {
            self.on_screen[i] = shown;
        }
    }

    /// Look again at whether anyone is watching, and say whether that
    /// changed.
    fn look(&mut self, card: &Card) -> bool {
        let Some(watch) = self.watch.as_mut() else {
            return false;
        };
        if watch.looked.elapsed() >= CONNECTOR_POLL {
            watch.looked = Instant::now();
            // A connector that cannot be read is taken to be there: pausing
            // a wall somebody is looking at is the worse mistake.
            let connected = card
                .get_connector(self.out.connector, false)
                .map_or(true, |info| info.state() != connector::State::Disconnected);
            if connected != watch.connected {
                watch.connected = connected;
                println!(
                    "{}: {}",
                    self.out.name,
                    if connected {
                        "plugged back in"
                    } else {
                        "unplugged"
                    }
                );
            }
        }
        let watched = watch.connected
            && self
                .tv
                .as_ref()
                .is_none_or(|tv| tv.watched.load(Ordering::Relaxed));
        if watched == self.watched {
            return false;
        }
        self.watched = watched;
        self.turns += 1;
        if watched {
            println!("{}: on screen; resuming its feeds", self.out.name);
        } else {
            println!("{}: off screen; pausing its feeds", self.out.name);
            // Nothing is on screen while paused, so whatever a feed
            // publishes meanwhile is not a frame this wall skipped.
            for feed in &mut self.feeds {
                feed.last_active = None;
            }
            // And whoever comes back to it finds the grid, not whichever
            // camera somebody left fullscreen.
            self.mode = Mode::Grid;
        }
        true
    }

    /// Do whatever the keys pressed on the remote since last time ask for,
    /// and say so.
    ///
    /// Taken whether or not the wall is on screen, so that a key pressed
    /// while it was not is not acted on long after, once it is.
    fn take_keys(&mut self, clock: Duration) {
        let keys = match &self.tv {
            Some(tv) => std::mem::take(&mut *tv.keys.lock().unwrap()),
            None => return,
        };
        for code in keys {
            self.pressed = Instant::now();
            let action = if self.watched {
                self.press(code, clock)
            } else {
                None
            };
            match action {
                Some(action) => println!("{}: remote: key 0x{code:02x}: {action}", self.out.name),
                None => println!("{}: remote: key 0x{code:02x}", self.out.name),
            }
        }
    }

    /// Do what key `code` does, `clock` into the wall's run, and say what
    /// that was, or None for a key that does nothing.
    fn press(&mut self, code: u8, clock: Duration) -> Option<String> {
        Some(match key_move(self.mode, code, self.cameras.len())? {
            Move::Step(by) => {
                for feed in &mut self.feeds {
                    feed.step(by, clock);
                }
                if by > 0 { "next feed" } else { "previous feed" }.into()
            }
            Move::Show(mode) => {
                self.show(mode);
                match mode {
                    Mode::Grid => "back to the grid".into(),
                    Mode::Fullscreen(at) => format!("fullscreen {}", self.cameras[at].feed.name),
                }
            }
        })
    }

    /// Go back to the grid if a feed has been left fullscreen for too long.
    fn expire(&mut self) {
        if self.mode != Mode::Grid && self.pressed.elapsed() >= FULLSCREEN_FOR {
            self.show(Mode::Grid);
            println!(
                "{}: back to the grid, nothing pressed for {} minutes",
                self.out.name,
                FULLSCREEN_FOR.as_secs() / 60
            );
        }
    }

    fn show(&mut self, mode: Mode) {
        if (self.mode == Mode::Grid) != (mode == Mode::Grid) {
            self.turns += 1;
            // Nothing on the grid is on screen while a feed is fullscreen,
            // so whatever its feeds publish meanwhile is not a frame it
            // skipped.
            for feed in &mut self.feeds {
                feed.last_active = None;
            }
        }
        self.mode = mode;
    }

    /// Whether the display needs `source` decoding: every feed of its grid,
    /// including while a camera is fullscreen, so going back is immediate,
    /// and the stream for fullscreen of the camera it shows that way.
    fn shows(&self, source: &Arc<Source>) -> bool {
        let fullscreen = match self.mode {
            Mode::Fullscreen(at) => self.cameras[at].fullscreen.as_ref(),
            Mode::Grid => None,
        };
        fullscreen.is_some_and(|f| Arc::ptr_eq(f, source))
            || self
                .feeds
                .iter()
                .any(|f| f.taps.iter().any(|t| Arc::ptr_eq(&t.source, source)))
    }
}

fn run(config: &Config) -> Result<(), String> {
    // Before any thread starts, so that every one inherits the mask.
    stop_cleanly();

    // Only for blocks the board has: a Pi 3 has no HEVC block for any libav
    // to reach, and a Pi 5 no H.264 one. Said rather than refused, since the
    // wall still works, only at a cost the log should explain.
    for path in &decode::HW_PATHS {
        if (path.on_board)() && !(path.in_libav)() {
            eprintln!(
                "this libav lacks {}, so {} decodes in software; \
                 Raspberry Pi OS's libav has it",
                path.libav, path.codec
            );
        }
    }

    // Not card1 with a fallback to card0: on a Pi 5 that fallback lands on
    // the render node, which has no connectors and can never scan out.
    let path = present::detect_card();
    let card = Arc::new(Card::open(&path)?);
    // The card's number, which is how a CEC adapter says which card its
    // connector is on.
    let card_no: u32 = path
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    // The overlay planes are one pool shared by every display, not a set per
    // crtc: on a vc4 each plane's possible_crtcs names all of them, so asking
    // per display would hand the same planes out twice and the two walls
    // would fight over them. Taken from the front as each display claims what
    // it needs.
    let mut spare = Vec::new();
    let mut claimed = Vec::new();

    // One per feed, filled by that feed's thread and read wherever the feed
    // is shown.
    let sources: Vec<Arc<Source>> = config
        .feeds
        .iter()
        .map(|f| Arc::new(Source::new(&f.name)))
        .collect();
    // And one per stream for fullscreen, by the feed's index, which connects
    // only while a display shows its camera that way.
    let fullscreen: Vec<Option<Arc<Source>>> = config
        .feeds
        .iter()
        .map(|f| {
            f.fullscreen_url
                .as_ref()
                .map(|_| Arc::new(Source::on_demand(&format!("{} (fullscreen)", f.name))))
        })
        .collect();

    let mut walls = Vec::new();
    for display in &config.displays {
        let taken: Vec<crtc::Handle> = walls.iter().map(|w: &Wall| w.out.crtc).collect();
        let mut wall = build(
            &card,
            &sources,
            &fullscreen,
            display,
            &taken,
            &mut spare,
            &mut claimed,
        )?;
        wall.tv = match cec::watch(card_no, wall.out.connector.into(), &wall.out.name) {
            Ok(tv) => Some(tv),
            Err(e) => {
                eprintln!(
                    "{}: {e}; its remote will do nothing{}",
                    wall.out.name,
                    if config.pause_off_screen {
                        ", and only unplugging it will pause the wall"
                    } else {
                        ""
                    }
                );
                None
            }
        };
        if config.pause_off_screen {
            wall.watch = Some(Watch {
                connected: true,
                looked: Instant::now(),
            });
        }
        walls.push(wall);
    }

    // Started once every wall is built, since until then nothing could show
    // what they decode: a second display that is still off would otherwise
    // have the first display's cameras decoding for nobody while it waited.
    for (i, (feed, source)) in config.feeds.iter().zip(&sources).enumerate() {
        // How many displays show this feed decides how many of its frames
        // can be held at once, and so how many buffers a software-decoded
        // one gets. Not how many viewports: every viewport on a display
        // names the same frame in the same commit.
        //
        // The same for its stream for fullscreen, which every display
        // showing the camera may show at once.
        let displays = config.displays_showing(i);
        let streams = [
            Some((&feed.url, source)),
            feed.fullscreen_url.as_ref().zip(fullscreen[i].as_ref()),
        ];
        for (url, source) in streams.into_iter().flatten() {
            let url = url.clone();
            let card = card.clone();
            let source = source.clone();
            std::thread::spawn(move || feed_thread(&url, card, source, displays));
        }
    }

    // Every stream from here on, which is what the loop decides is wanted
    // and the report reports.
    let sources: Vec<Arc<Source>> = sources
        .into_iter()
        .chain(fullscreen.into_iter().flatten())
        .collect();
    run_walls(&card, &sources, &mut walls)
}

/// Exit when told to stop, handing back any CEC address first.
///
/// Waited for on a thread of its own rather than handled: a handler can do
/// almost nothing safely, and releasing an address is an ioctl that waits on
/// the bus. The signals are blocked everywhere else, so they wait for this
/// thread rather than kill the process.
///
/// For every wall, not only one with a CEC address to hand back: in a
/// container the wall is process 1, which the kernel does not let SIGTERM
/// kill while it has the default disposition, so `docker stop` waited out
/// its ten seconds and sent SIGKILL instead. A blocked signal is kept for sigwait rather than ignored.
fn stop_cleanly() {
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    std::thread::spawn(move || {
        let mut signal = 0;
        unsafe { libc::sigwait(&set, &mut signal) };
        cec::release();
        std::process::exit(0);
    });
}

/// Hand `want` overlay planes to a display, from the pool every display
/// shares.
///
/// On a vc4 each overlay plane's possible_crtcs names every display crtc --
/// sixteen planes that any screen can use, not a set per screen. Asking the
/// card per display would therefore return the same planes twice and leave
/// two walls driving one plane each. So a plane is offered once: `usable` is
/// what this crtc could take, `spare` what no display has claimed, and
/// `claimed` everything already handed out.
///
/// None when the pool cannot cover the display, which is a configuration to
/// refuse at startup rather than a wall to bring up half-drawn.
fn claim_planes(
    usable: &[plane::Handle],
    want: usize,
    spare: &mut Vec<plane::Handle>,
    claimed: &mut Vec<plane::Handle>,
) -> Option<Vec<plane::Handle>> {
    for &p in usable {
        if !spare.contains(&p) && !claimed.contains(&p) {
            spare.push(p);
        }
    }
    if spare.len() < want {
        return None;
    }
    let planes: Vec<plane::Handle> = spare.drain(..want).collect();
    claimed.extend_from_slice(&planes);
    Some(planes)
}

/// How long to wait between looks for a display that is not there yet.
const DISPLAY_POLL: Duration = Duration::from_secs(5);

/// Bring up one display, and describe the viewports it will show.
fn build(
    card: &Arc<Card>,
    sources: &[Arc<Source>],
    fullscreen: &[Option<Arc<Source>>],
    display: &config::Display,
    taken: &[crtc::Handle],
    spare: &mut Vec<plane::Handle>,
    claimed: &mut Vec<plane::Handle>,
) -> Result<Wall, String> {
    // A screen that is off, or unplugged, or still waking up is a wall that
    // is not ready rather than a wall that is misconfigured, so wait for it
    // instead of exiting. Exiting looks the same to systemd as a bad
    // configuration does, and turns a switched-off television into a restart
    // every RestartSec for as long as it stays off.
    // Said once, not every poll: the condition persists, so repeating it
    // fills the journal without telling anyone anything new.
    let mut said = false;
    let out = loop {
        match first_connected(card, display.connector.as_deref(), taken) {
            Ok(out) => break out,
            Err(e) => {
                if !said {
                    eprintln!("{e}; waiting for one");
                    said = true;
                }
                std::thread::sleep(DISPLAY_POLL);
            }
        }
    };
    if said {
        println!("a display arrived");
    }
    // A configured resolution is looked up among the modes the display
    // offers; without one it keeps whatever it is already in.
    //
    // Unless it is in none. A display plugged in after boot has had no mode
    // set on it by anyone: the console would set one, but not while this
    // process holds the card, so the wall has to, or the screen stays dark
    // for as long as it runs. It gets the mode the display prefers, which is
    // the one the console would have chosen.
    let chosen;
    let mode = match display.mode {
        Some(want) => {
            chosen = card.find_mode(out.connector, want)?;
            Some(&chosen)
        }
        None if card.crtc_mode(out.crtc, None).is_err() => {
            chosen = card.preferred_mode(out.connector)?;
            Some(&chosen)
        }
        None => None,
    };
    // The refresh rate whether it was set or adopted: it is the wall's commit
    // rate, so a display already in 4K30 is why nine 30fps cameras come out
    // at thirty commits a second rather than sixty.
    let scanning = card.crtc_mode(out.crtc, mode)?;
    let (mode_w, mode_h) = (scanning.size().0 as u32, scanning.size().1 as u32);
    println!(
        "display {}: {mode_w}x{mode_h}@{} ({}) on crtc index {}",
        out.name,
        scanning.vrefresh(),
        if mode.is_some() { "set" } else { "already set" },
        out.crtc_index
    );
    // Painted behind the viewports so a gap, or a feed that has gone away,
    // shows the wall's own background rather than whatever the console left on
    // the screen.
    let background = card.background(mode_w, mode_h, display.background)?;
    card.attach(out.crtc, out.connector, mode, Some(background.fb))?;

    let planes = claim_planes(
        &card.overlay_planes(out.crtc)?,
        display.viewports.len(),
        spare,
        claimed,
    )
    .ok_or_else(|| {
        format!(
            "{} viewports on {} but only {} overlay planes left; \
             the wall's displays share one pool of them",
            display.viewports.len(),
            out.name,
            spare.len()
        )
    })?;

    // The grid is square in cells, so a cell is the display divided by the
    // layout's size. On a 16:9 screen that makes each cell 16:9 too, which is
    // what a camera wants.
    let grid = display.layout.size;
    let cell_w = mode_w / grid;
    let cell_h = mode_h / grid;

    // A gap wider than the cell it is taken out of would leave a viewport no
    // pixels at all: crtc_w below is a cell minus the gap, which underflows
    // and wraps to something near four billion, and the commit is refused
    // with an EINVAL that says nothing about spacing. Checked against the
    // real cell size rather than at parse time, because how large a cell is
    // depends on the mode, which is not known until the display is up.
    if display.spacing >= cell_w.min(cell_h) {
        return Err(format!(
            "a spacing of {} leaves no room in a {cell_w}x{cell_h} cell on {}; \
             the wall is {grid} cells across a {mode_w}x{mode_h} display",
            display.spacing, out.name
        ));
    }

    let mut feeds: Vec<Feeds> = Vec::new();
    let mut viewports: Vec<Viewport> = Vec::new();

    for (i, viewport) in display.viewports.iter().enumerate() {
        let (_, cell) = &display.layout.slots[viewport.slot];
        let props = card.plane_props(planes[i])?;

        // The gap is taken off the right and bottom of each viewport, so it
        // falls between neighbours. An edge that meets the display rather than
        // another viewport keeps its pixels: a border around the wall is not a
        // seam, and nothing is on the other side of it.
        let touches_right = cell.col + cell.span == grid;
        let touches_bottom = cell.row + cell.span == grid;
        let gap_w = if touches_right { 0 } else { display.spacing };
        let gap_h = if touches_bottom { 0 } else { display.spacing };

        viewports.push(Viewport {
            plane: planes[i],
            props,
            crtc_x: (cell.col * cell_w) as i32,
            crtc_y: (cell.row * cell_h) as i32,
            crtc_w: cell.span * cell_w - gap_w,
            crtc_h: cell.span * cell_h - gap_h,
            gap_w,
            gap_h,
            // Filled in once the feed's real size is known.
            src_w: 0,
            src_h: 0,
        });

        // Every feed the viewport rotates through, all of them decoding: it
        // shows whichever the clock says, and the others are ready.
        feeds.push(Feeds::new(
            viewport
                .feeds
                .iter()
                .map(|&f| Tap::new(sources[f].clone()))
                .collect(),
            viewport.rotation_interval,
        ));
    }

    let crtc_active = card.crtc_active(out.crtc)?;

    let mut cameras: Vec<usize> = Vec::new();
    for &f in display.viewports.iter().flat_map(|v| &v.feeds) {
        if !cameras.contains(&f) {
            cameras.push(f);
        }
    }

    Ok(Wall {
        out,
        crtc_active,
        // Nothing is attached until the first commit that shows something:
        // the planes were claimed, not configured.
        on_screen: vec![None; viewports.len()],
        // Nothing has flipped yet, so nothing would ever be committed: the
        // first commit has to be made rather than waited for.
        in_flight: None,
        viewports,
        feeds,
        commits: 0,
        watched: true,
        turns: 0,
        watch: None,
        tv: None,
        cameras: cameras
            .iter()
            .map(|&f| Camera {
                feed: sources[f].clone(),
                fullscreen: fullscreen[f].clone(),
            })
            .collect(),
        size: (mode_w, mode_h),
        mode: Mode::Grid,
        pressed: Instant::now(),
        laid_out: None,
        fullscreen_tap: None,
    })
}

/// Drive every display from one loop, each paced by its own flips.
///
/// One loop rather than a thread per display. Each commit asks for a
/// page-flip event, and that event is the cue to commit that display again,
/// so a display that is slower, or in a different refresh rate, simply asks
/// for its next commit later. A thread apiece would need the card locked
/// around every commit and would spin whenever a display was not ready;
/// waiting on the card's events blocks until one of them actually needs
/// something. It also means never committing while a flip is outstanding,
/// which the kernel refuses with EBUSY. A loop that retried instead spun at
/// thousands of rejections a second.
fn run_walls(card: &Arc<Card>, sources: &[Arc<Source>], walls: &mut [Wall]) -> Result<(), String> {
    // What every rotation is timed from. One for the whole process, so
    // viewports on different displays switch together too.
    let started = Instant::now();
    let mut marks = Marks {
        at: Duration::ZERO,
        decoded: vec![0; sources.len()],
        walls: walls
            .iter()
            .map(|_| WallMark {
                commits: 0,
                turns: 0,
            })
            .collect(),
    };
    let mut commit_gripe = Throttle::new();

    loop {
        // A feed is wanted while any display showing it is watched, so one
        // shown on two screens keeps decoding for whichever is still on.
        //
        // Keys first, since what a display shows fullscreen is wanted too.
        // On a clock read before any commit this pass, which a step is in
        // effect for: the next turn is at least a second away.
        let mut changed = false;
        let clock = started.elapsed();
        for wall in walls.iter_mut() {
            let mode = wall.mode;
            changed |= wall.look(card);
            wall.take_keys(clock);
            wall.expire();
            changed |= wall.mode != mode;
        }
        if changed {
            for source in sources {
                let wanted = walls.iter().any(|w| w.watched && w.shows(source));
                source.wanted.store(wanted, Ordering::Relaxed);
            }
        }

        for wall in walls.iter_mut() {
            if wall.in_flight.is_some() {
                continue;
            }
            let clock = started.elapsed();
            // A display is only due once its last commit has flipped, so
            // on_screen is what its planes show now.
            let showing = |i: usize| wall.on_screen[i].as_ref().map(Arc::as_ptr);
            let target = wall.watched.then_some(wall.mode);
            let mut ready = Vec::new();
            if wall.laid_out != target {
                // Every plane let go of, once, and whatever comes next laid
                // out afresh once that has flipped. Paused, that is all until
                // someone is watching again: the planes would otherwise hold
                // the last frames, and with them the decoders' buffers, for
                // as long as the pause lasted.
                ready = let_go(&wall.viewports, showing, 0);
                if ready.is_empty() {
                    wall.laid_out = target;
                    wall.fullscreen_tap = None;
                }
            }
            if wall.laid_out == target {
                ready = match target {
                    None => continue,
                    Some(Mode::Grid) => {
                        collect_ready(&mut wall.feeds, &wall.viewports, showing, clock)
                    }
                    Some(Mode::Fullscreen(at)) => collect_fullscreen(
                        &wall.viewports,
                        showing,
                        &wall.cameras[at],
                        wall.size,
                        &mut wall.fullscreen_tap,
                        clock,
                    ),
                };
            }
            // Committed even when nothing changed, and then it names no plane
            // at all: the flip it asks for is what paces this display, and
            // without one a new frame would wait out the poll below. Such a
            // commit is legal because Card::commit names the crtc whatever
            // else a request says.
            let request = ready
                .iter()
                .map(|(_, t, shown)| (*t, shown.as_ref().map(|s| s.fb.fb)));
            match card.commit(wall.out.crtc, wall.crtc_active, request) {
                Ok(()) => {
                    wall.commits += 1;
                    // Held until the flip: from here on the display may be
                    // reading these, whatever their feeds do next. Recorded
                    // only now, because a refused commit leaves every plane
                    // exactly as it was.
                    let named = ready.into_iter().map(|(i, _, s)| (i, s)).collect();
                    wall.in_flight = Some(InFlight {
                        at: Instant::now(),
                        overdue_said: false,
                        named,
                    });
                }
                Err(e) => {
                    // No flip will arrive for a commit that was refused, so
                    // this display stays due and tries again after the wait
                    // below.
                    commit_gripe.say(format_args!("commit on {}: {e}", wall.out.name));
                }
            }
        }

        // Only so the loop wakes when no flip does: a display whose commit
        // was refused has no flip coming, and retries after this long.
        match card.next_flips(Duration::from_millis(100)) {
            Ok(flipped) if !flipped.is_empty() => {
                // Which displays flipped decides whose buffers may be freed:
                // another may still be scanning out what it was given.
                for wall in walls.iter_mut().filter(|w| flipped.contains(&w.out.crtc)) {
                    wall.flipped();
                }
            }
            // Nothing arrived. A display that is due commits again
            // regardless: either there was nothing to show yet, or a commit
            // failed and owes itself a retry.
            Ok(_) => {}
            Err(e) => commit_gripe.say(format_args!("{e}")),
        }
        say_overdue(walls);

        let clock = started.elapsed();
        if clock - marks.at >= Duration::from_secs(10) {
            report(sources, walls, &mut marks, clock);
        }
    }
}

/// How long a commit may go without its flip before that is worth saying.
/// Many frames at any refresh rate a display offers, so a healthy one never
/// comes near it.
const FLIP_OVERDUE: Duration = Duration::from_secs(1);

/// Say which displays have a commit whose flip is overdue, and do nothing
/// else about it.
///
/// An accepted commit that asked for a flip event always gets one: the
/// kernel sends it when the commit takes effect, and even for a display that
/// is switched off or unplugged the atomic helpers complete it. So nothing is
/// let go of until it arrives, however long that takes. Letting go of a
/// display's frames on a timeout instead, as this once did, released buffers
/// the display might still be reading: a torn picture, or a capture buffer
/// back with the decoder while it was on screen. And a flip that then
/// arrived after all was taken for the next commit's.
///
/// What a display holds meanwhile is bounded: the frames its last commit
/// named and the ones it replaced, which the scanout pools and the decoders'
/// buffers already allow for. The one thing left for a flip that never comes,
/// which would take a driver bug, is that the display stops changing, and
/// this is what makes that visible rather than silent.
fn say_overdue(walls: &mut [Wall]) {
    for wall in walls.iter_mut() {
        let Some(commit) = wall.in_flight.as_mut() else {
            continue;
        };
        if !commit.overdue_said && commit.at.elapsed() >= FLIP_OVERDUE {
            commit.overdue_said = true;
            eprintln!(
                "{}: no flip {}s after a commit; holding its frames until one comes",
                wall.out.name,
                FLIP_OVERDUE.as_secs()
            );
        }
    }
}

/// The viewports whose plane has something new to show, and the framebuffer
/// to show for each: a frame it is not showing yet, or nothing, for a plane
/// that has to be let go of.
///
/// `showing` says which frame each viewport's plane is scanning out, if any,
/// as of the display's last flip.
///
/// Counts a viewport as presented only when the feed's generation has moved
/// on, so a stalled feed being committed again does not flatter the numbers.
///
/// `clock` is read once for the whole display, so every viewport in this
/// commit agrees on whose turn it is. So does each feed: its frame is read
/// once, and every viewport showing it names the same one. Reading it per
/// viewport could put two frames of one camera side by side whenever the
/// decoder published between the reads, and would hold one more of its
/// buffers than the display needs.
fn collect_ready(
    feeds: &mut [Feeds],
    viewports: &[Viewport],
    showing: impl Fn(usize) -> Option<*const Shown>,
    clock: Duration,
) -> Vec<(usize, Viewport, Option<Arc<Shown>>)> {
    let mut ready = Vec::new();
    // Each feed's frame, and its generation, as this commit reads them. A
    // list for the same reason the framebuffer cache is one: it holds a
    // handful of entries.
    let mut read: Vec<(*const Source, Option<Arc<Shown>>, u64)> = Vec::new();
    for (i, feed) in feeds.iter_mut().enumerate() {
        let at = feed.active(clock);
        let returning = feed.last_active.replace(at) != Some(at);
        let tap = &mut feed.taps[at];
        // Attached right now, not "has ever shown anything": a plane already
        // detached has nothing left to say, and a wall whose feeds have all
        // gone would otherwise rebuild the same detaches sixty times a
        // second. Keeping those commits legal is Card::commit's job, which
        // names the crtc whatever else a request says.
        let attached = showing(i).is_some();
        // A reference of the commit's own rather than a copy of the handle:
        // the feed may replace or clear its frame the moment the lock is
        // released, and what this commit names has to outlive that. The size
        // comes from the same frame rather than from whichever arrived next.
        let key = Arc::as_ptr(&tap.source);
        let (shown, gen) = match read.iter().find(|(k, _, _)| *k == key) {
            Some((_, shown, gen)) => (shown.clone(), *gen),
            None => {
                let (shown, gen) = tap.source.frame();
                read.push((key, shown.clone(), gen));
                (shown, gen)
            }
        };
        let Some(shown) = shown else {
            // No frame: the feed has gone away, so the plane is dropped
            // rather than left showing what it had.
            if attached {
                ready.push((i, viewports[i], None));
            }
            continue;
        };
        // Never zero: feed_session skips a frame with no visible pixels.
        let mut t = viewports[i];
        t.src_w = shown.width;
        t.src_h = shown.height;
        tap.count(gen, returning);
        // A plane already showing this very frame is left out of the commit,
        // and goes on showing it. A new frame is always a new Shown, so the
        // same one means the same framebuffer at the same size, and naming it
        // again would only give the kernel another plane to check. With
        // cameras at 15 to 30 frames a second and sixty commits, that was
        // half the planes in every commit.
        if showing(i) == Some(Arc::as_ptr(&shown)) {
            continue;
        }
        ready.push((i, t, Some(shown)));
    }
    ready
}

/// Every plane from viewport `from` on that is attached, to be let go of.
fn let_go(
    viewports: &[Viewport],
    showing: impl Fn(usize) -> Option<*const Shown>,
    from: usize,
) -> Vec<(usize, Viewport, Option<Arc<Shown>>)> {
    (from..viewports.len())
        .filter(|&i| showing(i).is_some())
        .map(|i| (i, viewports[i], None))
        .collect()
}

/// What a display showing one camera fullscreen commits: its frame on the
/// first viewport's plane, stretched over the whole screen the way a
/// viewport stretches one over its cell, and every other plane let go of.
///
/// The frame is from its stream for fullscreen once that has one, and from
/// the feed its viewports show until then, or for a camera without one.
/// It is counted on `tap` as a viewport's is on its own, which starts afresh
/// `clock` into the wall's run whenever the picture changes feed.
///
/// The grid's viewports present nothing meanwhile and count nothing, and
/// the frame is left out of the commit when the plane is showing it
/// already, as collect_ready does.
fn collect_fullscreen(
    viewports: &[Viewport],
    showing: impl Fn(usize) -> Option<*const Shown>,
    camera: &Camera,
    (width, height): (u32, u32),
    tap: &mut Option<(Tap, Duration)>,
    clock: Duration,
) -> Vec<(usize, Viewport, Option<Arc<Shown>>)> {
    let mut ready = let_go(viewports, &showing, 1);
    let Some(first) = viewports.first() else {
        return ready;
    };
    let (source, (shown, gen)) = match camera.fullscreen.as_ref().map(|f| (f, f.frame())) {
        Some(sharp @ (_, (Some(_), _))) => sharp,
        _ => (&camera.feed, camera.feed.frame()),
    };
    match shown {
        Some(shown) => {
            let returning = !tap
                .as_ref()
                .is_some_and(|(t, _)| Arc::ptr_eq(&t.source, source));
            if returning {
                *tap = Some((Tap::new(source.clone()), clock));
            }
            if let Some((tap, _)) = tap {
                tap.count(gen, returning);
            }
            if showing(0) != Some(Arc::as_ptr(&shown)) {
                let t = Viewport {
                    crtc_x: 0,
                    crtc_y: 0,
                    crtc_w: width,
                    crtc_h: height,
                    gap_w: 0,
                    gap_h: 0,
                    src_w: shown.width,
                    src_h: shown.height,
                    ..*first
                };
                ready.push((0, t, Some(shown)));
            }
        }
        // The camera has gone away: the background, as on the grid.
        None if showing(0).is_some() => ready.push((0, *first, None)),
        None => {}
    }
    ready
}

/// What the report calls one feed on one viewport.
///
/// The feed's own name, which is what its thread's messages go by, so a
/// report saying viewport 6 is showing nothing can be matched to the line
/// explaining why. The viewport says where on the wall it is, which the
/// feed's name alone does not. `star` marks the feed a rotating viewport is
/// currently showing; a viewport with one feed has nothing to mark.
fn feed_name(viewport: usize, feed: &str, star: bool) -> String {
    format!(
        "viewport {viewport} ({feed}{})",
        if star { " *" } else { "" }
    )
}

/// What the counters read at the previous report.
///
/// Rates are what happened since then, not since the process started. A
/// lifetime average cannot show a feed going down: a camera healthy for an
/// hour and then dead still reads 22.1 fps five minutes later, and 19.2 after
/// fifteen, because the hour of good frames is still in the numerator. The
/// point of a report every ten seconds is to say what is true now.
struct Marks {
    /// When on the wall's clock the previous report was made.
    at: Duration,
    /// Frames decoded, per feed, in the order the configuration lists them.
    decoded: Vec<u64>,
    walls: Vec<WallMark>,
}

struct WallMark {
    commits: u64,
    turns: u64,
}

/// Say what the last few seconds were like, in two parts: the feeds, then
/// each display.
///
/// Split along the same line as Source and Tap. Decoding and failing are
/// the camera's, and said once however many viewports show it. Presenting
/// and skipping are each viewport's, because the same feed can keep up on one
/// display and fall behind on another.
fn report(sources: &[Arc<Source>], walls: &mut [Wall], marks: &mut Marks, clock: Duration) {
    let secs = (clock - marks.at).as_secs_f64();
    marks.at = clock;
    // "last 10s", not the wall's uptime: every number below describes that
    // window and nothing earlier, and a header counting up from boot invited
    // reading them as averages over the whole run.
    println!("--- last {secs:.0}s ---");
    for (source, was) in sources.iter().zip(marks.decoded.iter_mut()) {
        let d = source.stats.decoded.load(Ordering::Relaxed);
        let wanted = source.wanted.load(Ordering::Relaxed);
        // A stream for fullscreen that no display is showing is idle, which
        // is all it is most of the time, and nothing to report.
        if source.on_demand && !wanted {
            *was = d;
            continue;
        }
        // A total, not a rate: how often a feed has failed is running
        // evidence, and a count that started over every ten seconds would
        // answer a much smaller question than the one being asked of it.
        let e = source.stats.errors.load(Ordering::Relaxed);
        // No hardware/software here: the feed says so once when it opens,
        // beside the codec that explains it, and repeating it every ten
        // seconds crowds the line without ever changing.
        if wanted {
            println!(
                "  {}: decoded {:.1} fps  errors={e}",
                source.name,
                (d - *was) as f64 / secs
            );
        } else {
            println!("  {}: paused  errors={e}", source.name);
        }
        *was = d;
    }
    for (wall, mark) in walls.iter_mut().zip(marks.walls.iter_mut()) {
        report_wall(wall, mark, secs, clock);
    }
}

/// `clock` is when on the wall's clock the window that `secs` spans ends.
fn report_wall(wall: &mut Wall, mark: &mut WallMark, secs: f64, clock: Duration) {
    // Rates over a window for some of which the grid was paused, or a feed
    // fullscreen instead, read low for no fault of the feeds', so such a
    // window says so.
    let away = wall.turns != mark.turns;
    mark.turns = wall.turns;
    // The grid's own lines only while it is on screen: fullscreen, its
    // viewports present nothing, and a page of dashes says nothing.
    let grid = wall.watched && wall.mode == Mode::Grid;
    if wall.watched {
        println!(
            "{}: {:.1} commits/s{}",
            wall.out.name,
            (wall.commits - mark.commits) as f64 / secs,
            if away {
                ", the grid off screen for some of it"
            } else {
                ""
            }
        );
        // The picture fullscreen, which may be the grid's stream scaled up
        // still, while the one for fullscreen connects: its name says which.
        match (wall.mode, &mut wall.fullscreen_tap) {
            (Mode::Fullscreen(_), Some((tap, since))) => {
                let label = format!("fullscreen: {}", tap.source.name);
                tap.report(&label, clock.saturating_sub(*since), true);
            }
            (Mode::Fullscreen(at), None) => {
                println!("  fullscreen: {}", wall.cameras[at].feed.name);
            }
            (Mode::Grid, _) => {}
        }
    } else {
        println!("{}: off screen, paused", wall.out.name);
    }
    mark.commits = wall.commits;
    for (i, feed) in wall.feeds.iter_mut().enumerate() {
        // Every feed, not only the visible one: a rotating viewport keeps
        // them all decoding, and one that has quietly stopped is worth seeing
        // before it comes back around.
        let active = feed.active(clock);
        let rotating = feed.taps.len() > 1;
        for n in 0..feed.taps.len() {
            let on_screen = feed.on_screen(n, clock);
            let tap = &mut feed.taps[n];
            let label = feed_name(i, &tap.source.name, rotating && n == active);
            tap.report(&label, on_screen, grid);
        }
    }
}

/// Reports a per-frame failure at most once a second.
///
/// Printing every one filled a Pi 3's 371MB /tmp with a single repeated line:
/// a fault that recurs 30 times a second says no more on the thousandth
/// telling than on the first.
struct Throttle {
    last: Option<Instant>,
    suppressed: u64,
}

impl Throttle {
    fn new() -> Self {
        Throttle {
            last: None,
            suppressed: 0,
        }
    }

    fn say(&mut self, msg: std::fmt::Arguments<'_>) {
        let now = Instant::now();
        let due = self
            .last
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
        if due {
            if self.suppressed > 0 {
                eprintln!("{msg} (and {} more like it)", self.suppressed);
            } else {
                eprintln!("{msg}");
            }
            self.last = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed += 1;
        }
    }
}

/// Keep one feed on screen for as long as the wall runs, reconnecting whenever
/// its camera goes away.
///
/// A camera that reboots, or a network that blinks, must not cost the
/// viewports showing it permanently. Each attempt is a session: it opens the
/// stream, decodes until something ends it, and comes back. Those viewports go
/// dark across the gap rather than holding the last frame: a still picture
/// passes for live video, and the background says plainly that the camera is
/// not there.
///
/// `displays` is how many displays show the feed, which bounds how many of
/// its frames can be held at once (see Scanout).
fn feed_thread(url: &str, card: Arc<Card>, source: Arc<Source>, displays: usize) {
    let mut backoff = Backoff::new();
    let name = &source.name;
    // Nothing is refused for its size if the card will not say what fits:
    // the kernel still has the last word when the frame arrives.
    let limit = card.largest_picture().unwrap_or((u32::MAX, u32::MAX));
    // Whether the stream has been refused as too large to show, and said
    // so, since the display last stopped wanting it: once each time a camera
    // is shown fullscreen, not on every try.
    let mut too_large = false;

    loop {
        // Not connected at all while nobody is watching: an open session
        // costs the camera a stream and the network its bitrate, decoded or
        // not.
        while !source.wanted.load(Ordering::Relaxed) {
            std::thread::sleep(PAUSE_POLL);
        }
        let started = Instant::now();
        let result = feed_session(url, &card, &source, displays, limit);
        let paused = !source.wanted.load(Ordering::Relaxed);
        match result {
            // Ended for the pause, which the display has said.
            Ok(()) if paused => {}
            Ok(()) => eprintln!("{name}: stream ended; reconnecting"),
            Err(Refused::TooLarge {
                size: (w, h),
                limit: (most_w, most_h),
            }) => {
                source.stats.errors.fetch_add(1, Ordering::Relaxed);
                // Too large goes on being too large until the camera sends
                // another size, so it is tried again no sooner than the
                // longest backoff.
                backoff.saturate();
                if !std::mem::replace(&mut too_large, true) {
                    eprintln!(
                        "{name} ({url}): {w}x{h} is larger than this display can show, \
                         {most_w}x{most_h} at most; trying again every {}s",
                        Backoff::MAX.as_secs()
                    );
                }
            }
            Err(Refused::Failed(e)) => {
                source.stats.errors.fetch_add(1, Ordering::Relaxed);
                // Not "retrying" when paused: it waits for the wall instead.
                if paused {
                    eprintln!("{name} ({url}): {e}");
                } else {
                    eprintln!(
                        "{name} ({url}): {e}; retrying in {:.1}s",
                        backoff.next().as_secs_f32()
                    );
                }
            }
        }
        // Blank every viewport showing this feed. A still frame left up
        // pretends to be live video, and a camera that has been dead for an
        // hour looks exactly like one that is working; black says plainly
        // that it is not.
        //
        // Nothing is freed here. The framebuffers the session made go when
        // their last holder does: those no display showed went with the
        // session's cache and pool, and the ones still on screen go after
        // each display's flip that blanks them. The next session imports its
        // own.
        source.shown.lock().unwrap().take();
        // Back as soon as someone is watching again, which is no reflection
        // on the camera.
        if paused {
            backoff.reset();
            too_large = false;
            continue;
        }
        // A session that ran for a while is evidence the camera is healthy,
        // so the next failure starts over at the short delay. Judging that by
        // whether frames arrived instead would spin at half a second on a
        // stream that opens and drops immediately.
        if started.elapsed() >= Backoff::STEADY {
            backoff.reset();
        }
        // Cut short when nobody wants the feed any more, like a pause: the
        // next try waits for someone to, and then goes at once.
        if !nap(&source, backoff.next()) {
            backoff.reset();
            too_large = false;
            continue;
        }
        backoff.advance();
    }
}

/// Sleep for `d`, or until nobody wants the feed any more, and say whether
/// anyone still does.
fn nap(source: &Source, d: Duration) -> bool {
    let until = Instant::now() + d;
    while source.wanted.load(Ordering::Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(PAUSE_POLL));
    }
    false
}

/// How often a paused feed looks at whether it is wanted again. Short
/// against the second or so a camera takes to send its first picture, which
/// is most of the wait for a wall coming back on screen.
const PAUSE_POLL: Duration = Duration::from_millis(250);

/// How long to wait before reconnecting a feed.
///
/// Doubling from half a second to thirty: a camera that blinks is back almost
/// at once, and one that is down for an hour is retried twice a minute rather
/// than hammered. Each attempt also costs the RTSP timeout before it fails,
/// so the interval a dead camera is actually retried at is that much longer
/// again.
struct Backoff(Duration);

impl Backoff {
    const FIRST: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(30);
    /// A session lasting this long counts as the camera having worked.
    const STEADY: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Backoff(Self::FIRST)
    }

    fn next(&self) -> Duration {
        self.0
    }

    fn advance(&mut self) {
        self.0 = (self.0 * 2).min(Self::MAX);
    }

    fn reset(&mut self) {
        self.0 = Self::FIRST;
    }

    fn saturate(&mut self) {
        self.0 = Self::MAX;
    }
}

/// One connection to a camera: decode until the stream stops, or say why the
/// session could not go on.
///
/// `limit` is the largest picture the display can show, and a stream larger
/// than that is refused before anything decodes it.
fn feed_session(
    url: &str,
    card: &Arc<Card>,
    source: &Source,
    displays: usize,
    limit: (u32, u32),
) -> Result<(), Refused> {
    let name = &source.name;
    let mut dec = Decoder::open(url, source.stats.clone(), limit, frames_held(displays))?;
    if let Some(unused) = &dec.unused {
        eprintln!("{name}: {unused}");
    }

    // GEM handle -> framebuffer. The decoder recycles a small pool of
    // buffers, so after the first few frames this is a lookup rather than an
    // import. Keyed on the handle rather than the dmabuf fd: a closed fd's
    // number is handed out again, so a stale entry would map a new buffer to
    // an old framebuffer.
    //
    // A list rather than a map: it holds as many entries as the decoder has
    // capture buffers, twenty by libav's default, and is searched once per
    // frame per feed. Hashing a u32 cost more than the scan does -- it was
    // the largest share of viewpie's own time in a profile of a nine-camera
    // wall, above the commit itself.
    let mut fbs: Vec<(Handle, Arc<Fb>)> = Vec::new();

    // Only allocated if this feed turns out to decode in software.
    let mut dumb: Option<Scanout> = None;
    let mut gripe = Throttle::new();
    let mut announced: Option<bool> = None;
    // Read once, before the loop borrows the decoder: it cannot change while
    // a session runs, since a different codec means a different stream.
    let codec = dec.codec();

    while let Some(frame) = dec.next_frame() {
        if !source.wanted.load(Ordering::Relaxed) {
            return Ok(());
        }
        let ptr = frame.as_ptr();
        // The visible size, not the buffer's. A decoder pads to its own
        // alignment, so 1080 becomes 1088 rows on a Pi, and scanning out the
        // padding puts a band of whatever it contains along the edge of the
        // viewport. Read outside the unsafe block, because the size travels
        // with the frame into the source so the commit loop cannot pair it with
        // another frame's buffer.
        let (w, h) = unsafe {
            (
                ((*ptr).width - (*ptr).crop_left as i32 - (*ptr).crop_right as i32) as u32,
                ((*ptr).height - (*ptr).crop_top as i32 - (*ptr).crop_bottom as i32) as u32,
            )
        };
        if w == 0 || h == 0 {
            continue;
        }
        let (fb, frame) = unsafe {
            // Report the path once, and again whenever it changes: a decoder
            // that starts on dmabufs and later hands back pixels sends every
            // frame down the copy path, which is not visible from a first
            // frame alone.
            let dmabuf = (*ptr).format == AVPixelFormat_AV_PIX_FMT_DRM_PRIME;
            if announced != Some(dmabuf) {
                // The url on the first of these, so one line says which
                // camera a viewport is and how it is being decoded: the two
                // are read together, and the url is what says which camera to
                // walk out and look at. Left off the repeats, which are about
                // the path changing rather than about which feed this is.
                let first = announced.is_none();
                announced = Some(dmabuf);
                eprintln!(
                    "{name}: {}{w}x{h} {codec}, decoded in {}",
                    if first {
                        format!("{url}, ")
                    } else {
                        String::new()
                    },
                    if dmabuf { "hardware" } else { "software" }
                );
            }
            if dmabuf {
                // Hardware: the frame already is a dmabuf, so putting it on
                // screen costs an import the first time and nothing after.
                let desc = (*ptr).data[0] as *const AVDRMFrameDescriptor;
                if desc.is_null() {
                    continue;
                }
                let fd = BorrowedFd::borrow_raw((*desc).objects[0].fd);
                let Ok(key) = card.import_dmabuf(fd) else {
                    gripe.say(format_args!("{name}: could not import a dmabuf"));
                    continue;
                };
                let fb = match fbs.iter().find(|(h, _)| *h == key) {
                    Some((_, fb)) => fb.clone(),
                    None => match import_drm_frame(card, &*desc, w, h) {
                        Ok(fb) => {
                            let fb = Arc::new(Fb::new(card.clone(), fb, key));
                            fbs.push((key, fb.clone()));
                            fb
                        }
                        Err(e) => {
                            gripe.say(format_args!("{name}: {e}"));
                            continue;
                        }
                    },
                };
                // The frame travels with its framebuffer: it is the decoder's
                // buffer, and the picture only holds still for as long as the
                // decoder cannot have it back.
                (fb, Some(frame))
            } else {
                // Software: the frame is in system memory and has to be copied
                // into a buffer the display controller can read. A Pi 5 has no
                // H.264 block at all, so on a mixed wall this is most
                // viewports.
                let fmt = (*ptr).format;
                if fmt != AVPixelFormat_AV_PIX_FMT_YUV420P
                    && fmt != AVPixelFormat_AV_PIX_FMT_YUVJ420P
                {
                    gripe.say(format_args!(
                        "{name}: unexpected software format {fmt}; only I420 is handled"
                    ));
                    continue;
                }
                if dumb.as_ref().is_none_or(|s| (s.w, s.h) != (w, h)) {
                    // The old buffers may still be on screen; whichever
                    // display holds them lets go of them after its flip.
                    // Let go of first, so they are not still counted
                    // against the new ones if memory is short.
                    drop(dumb.take());
                    // Not a frame to skip and try again on the next: the
                    // memory is not there, and every frame would fail the
                    // same way. Ending the session reconnects on the
                    // backoff, and says so.
                    dumb = Some(Scanout::new(card, w, h, displays)?);
                }
                let scanout = dumb.as_mut().unwrap();
                let (buf, fb) = match scanout.next() {
                    Ok(next) => next,
                    Err(e) => {
                        gripe.say(format_args!("{name}: {e}"));
                        continue;
                    }
                };
                if let Err(e) = write_i420(card, buf, &(*ptr).data, &(*ptr).linesize, w, h) {
                    gripe.say(format_args!("{name}: {e}"));
                    continue;
                }
                // The pixels are copied out, so the frame has nothing left
                // to keep alive.
                (fb, None)
            }
        };

        // Replacing the entry drops the source's reference to the previous
        // frame. Any display that committed it holds its own until the flip
        // after, so the buffer is not handed back while it is being read.
        *source.shown.lock().unwrap() = Some(Arc::new(Shown {
            fb,
            width: w,
            height: h,
            _frame: frame,
        }));
        // Published last, and with Release, so the commit loop never sees a
        // new generation before the framebuffer it refers to.
        source.stats.published.fetch_add(1, Ordering::Release);
    }
    if dec.stalled {
        return Err(format!("no picture for {}s", decode::PICTURE_TIMEOUT.as_secs()).into());
    }
    Ok(())
}

/// Import a hardware-decoded frame's dmabuf planes as one framebuffer.
unsafe fn import_drm_frame(
    card: &Card,
    desc: &AVDRMFrameDescriptor,
    w: u32,
    h: u32,
) -> Result<framebuffer::Handle, String> {
    let layer = &desc.layers[0];
    let mut handles = [None; 4];
    let mut pitches = [0u32; 4];
    let mut offsets = [0u32; 4];
    let mut modifier = DrmModifier::Linear;

    for i in 0..layer.nb_planes as usize {
        let pl = &layer.planes[i];
        let obj = &desc.objects[pl.object_index as usize];
        let fd = BorrowedFd::borrow_raw(obj.fd);
        handles[i] = Some(card.import_dmabuf(fd)?);
        pitches[i] = pl.pitch as u32;
        offsets[i] = pl.offset as u32;
        modifier = DrmModifier::from(obj.format_modifier);
    }

    let format = DrmFourcc::try_from(layer.format)
        .map_err(|_| format!("the decoder produced an unknown fourcc {:#x}", layer.format))?;

    card.import(&ImportedFrame {
        width: w,
        height: h,
        format,
        modifier,
        handles,
        pitches,
        offsets,
    })
}

/// The buffers a software-decoded feed is scanned out from.
///
/// A pool, because the hardware path's protection does not carry over. There
/// a frame *is* the scanout buffer, so holding the frame holds the pixels
/// still. Software frames are copied into buffers of our own, and writing one
/// the display is reading tears the viewport, continuously, which on a Pi 5
/// is every H.264 viewport because the board has no H.264 block at all.
///
/// So a buffer is written only once nothing else holds its framebuffer: not
/// the source, not a commit in flight, not a display still scanning it out.
/// Counting the holders answers that for any number of displays, where
/// alternating between two buffers only did for one that always flipped
/// before the next frame was written.
///
/// How many of a feed's frames can be in use at once, shown on `displays`
/// displays: the newest, which a commit can take at any moment, and on each
/// display the one its last commit named and the one it is still scanning
/// out until that commit flips. With one more being decoded or written
/// into, that is two plus two per display.
///
/// Both the scanout buffers of a feed decoded in software and the capture
/// buffers of one decoded in hardware are counted from it.
fn frames_held(displays: usize) -> usize {
    2 + 2 * displays
}

/// Every buffer frames_held says a feed can need.
///
/// All of them made up front, when the feed's size is known. A wall that
/// runs for long enough needs every one of them sooner or later: on one
/// display the fourth is wanted whenever a frame lands between a commit and
/// its flip, which is seconds in. Making them as they were first wanted
/// would only move that allocation to some arbitrary later moment, when CMA
/// may be short and the failure would be a feed that stutters for no
/// visible reason. Now it fails, if it fails, as the session starts.
struct Scanout {
    bufs: Vec<(drm::control::dumbbuffer::DumbBuffer, Arc<Fb>)>,
    /// The frame size these were made for. Kept because the buffer's own size
    /// is the padded one, 1.5x taller for the chroma planes, so comparing
    /// against that would reallocate on every frame, which churns KMS handles
    /// until a commit fails with EBUSY.
    w: u32,
    h: u32,
}

impl Scanout {
    /// Every buffer a feed shown on `displays` displays can need.
    fn new(card: &Arc<Card>, w: u32, h: u32, displays: usize) -> Result<Self, String> {
        let bufs = (0..frames_held(displays))
            .map(|_| new_dumb(card, w, h))
            .collect::<Result<_, _>>()?;
        Ok(Scanout { bufs, w, h })
    }

    /// A buffer to write the next frame into, and its framebuffer: one
    /// nothing else holds, and so one no display can be reading.
    ///
    /// A count of one cannot rise underneath this, because only the frame in
    /// the source can gain a holder, and this buffer is not in the source.
    fn next(&mut self) -> Result<(&mut drm::control::dumbbuffer::DumbBuffer, Arc<Fb>), String> {
        let n = self.bufs.len();
        let (buf, fb) = self
            .bufs
            .iter_mut()
            .find(|(_, fb)| Arc::strong_count(fb) == 1)
            // Cannot happen while the count above is right, so it is said
            // rather than waited out: something is holding frames it
            // should have let go of.
            .ok_or_else(|| format!("all {n} scanout buffers are still held; dropping a frame"))?;
        Ok((buf, fb.clone()))
    }
}

/// A scanout buffer the CPU can write into, plus its framebuffer.
fn new_dumb(
    card: &Arc<Card>,
    w: u32,
    h: u32,
) -> Result<(drm::control::dumbbuffer::DumbBuffer, Arc<Fb>), String> {
    // I420 as KMS sees it: three planes in one allocation. The buffer is
    // asked for at 8bpp and 1.5x the height, because a dumb buffer only knows
    // about bytes and the chroma planes ride underneath the luma. Each chroma
    // plane is half as wide as the luma, so it occupies half a row per line
    // and the two together take the remaining half height.
    let buf = card
        .create_dumb_buffer((w, h * 3 / 2), DrmFourcc::R8, 8)
        .map_err(|e| format!("could not allocate a scanout buffer: {e}"))?;
    let fb = match card.add_planar_framebuffer(&I420View { buf: &buf, w, h }, FbCmd2Flags::empty())
    {
        Ok(fb) => fb,
        Err(e) => {
            card.drop_buffer(buf.handle());
            return Err(format!(
                "could not add a framebuffer for the scanout buffer: {e}"
            ));
        }
    };
    // DumbBuffer frees nothing when dropped, so the handle is closed with the
    // framebuffer instead. Closing it is how a dumb buffer is destroyed: the
    // kernel's DESTROY_DUMB is the same GEM handle delete. Retiring only the
    // framebuffer, as this once did, leaked two buffers' worth of CMA every
    // time a software-decoded feed reconnected.
    let handle = buf.handle();
    Ok((buf, Arc::new(Fb::new(card.clone(), fb, handle))))
}

/// Describes a dumb buffer's single allocation as the three I420 planes KMS
/// expects.
struct I420View<'a> {
    buf: &'a drm::control::dumbbuffer::DumbBuffer,
    w: u32,
    h: u32,
}

impl drm::buffer::PlanarBuffer for I420View<'_> {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Yuv420
    }
    fn modifier(&self) -> Option<DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        // Chroma rows are half as wide as luma rows: that is what I420 means,
        // and a framebuffer describing them as full width is rejected.
        let p = self.buf.pitch();
        [p, p / 2, p / 2, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        let h = self.buf.handle();
        [Some(h), Some(h), Some(h), None]
    }
    fn offsets(&self) -> [u32; 4] {
        // Luma fills h rows of p bytes; then each chroma plane fills h/2 rows
        // of p/2 bytes. The offsets have to follow that, not the padded
        // height the buffer was allocated with.
        let p = self.buf.pitch();
        let luma = p * self.h;
        let chroma = (p / 2) * (self.h / 2);
        [0, luma, luma + chroma, 0]
    }
}

/// Copy one decoded I420 frame into a scanout buffer, row by row.
///
/// Row by row rather than one memcpy because the decoder's stride is its own
/// business and rarely matches the buffer's pitch.
unsafe fn write_i420(
    card: &Card,
    buf: &mut drm::control::dumbbuffer::DumbBuffer,
    data: &[*mut u8; 8],
    linesize: &[i32; 8],
    w: u32,
    h: u32,
) -> Result<(), String> {
    let pitch = buf.pitch();
    let mut map = card
        .map_dumb_buffer(buf)
        .map_err(|e| format!("could not map the scanout buffer: {e}"))?;
    let dst = map.as_mut();

    // Same layout the framebuffer describes: full-width luma, then two
    // half-width chroma planes.
    let luma = (pitch * h) as usize;
    let chroma = ((pitch / 2) * (h / 2)) as usize;
    let planes = [
        (0usize, 0usize, pitch as usize, w as usize, h as usize),
        (
            1,
            luma,
            (pitch / 2) as usize,
            (w / 2) as usize,
            (h / 2) as usize,
        ),
        (
            2,
            luma + chroma,
            (pitch / 2) as usize,
            (w / 2) as usize,
            (h / 2) as usize,
        ),
    ];

    for (plane, base, dst_pitch, pw, ph) in planes {
        let src = data[plane];
        if src.is_null() {
            continue;
        }
        let src_pitch = linesize[plane] as usize;
        for y in 0..ph {
            let off = base + y * dst_pitch;
            if off + pw > dst.len() {
                break;
            }
            std::ptr::copy_nonoverlapping(src.add(y * src_pitch), dst[off..].as_mut_ptr(), pw);
        }
    }
    Ok(())
}

/// The first connected connector, the crtc that drives it, and that crtc's
/// index.
///
/// The crtc must come from the connector's encoder, not from the first in the
/// list: on a Pi 5 the connected HDMI sits on crtc index 2, and driving index
/// 0 leaves the display dark while every commit still succeeds. The index
/// matters separately because a plane's possible_crtcs is a bitmask over it.
struct Output {
    connector: connector::Handle,
    crtc: crtc::Handle,
    crtc_index: u32,
    /// The connector's name, such as "HDMI-A-1". Carried so that messages
    /// about a wall say which screen they mean.
    name: String,
}

///
/// `taken` are the crtcs other displays of the wall already drive, so that a
/// display which has none of its own yet is not given one of theirs.
fn first_connected(
    card: &Card,
    want: Option<&str>,
    taken: &[crtc::Handle],
) -> Result<Output, String> {
    let res = card
        .resource_handles()
        .map_err(|e| format!("could not read the card's resources: {e}"))?;
    let crtcs = res.crtcs();

    for &conn in res.connectors() {
        let Ok(info) = card.get_connector(conn, false) else {
            continue;
        };
        if info.state() != connector::State::Connected {
            continue;
        }
        // A configuration may name which output to drive, which it has to
        // once there is more than one to choose between.
        //
        // as_str(), not Debug: the variant is spelled HDMIA and would give
        // "HDMIA-1", which matches neither sysfs nor what anyone would write.
        // as_str() follows the kernel's own table, so this is the name in
        // /sys/class/drm and the one a configuration can be expected to use.
        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        if want.is_some_and(|want| want != name) {
            continue;
        }
        // No mode is chosen here: the display already has one, set by the
        // console at boot, and adopting it avoids a disruptive switch. A
        // configured resolution will override this.
        if info.modes().is_empty() {
            continue;
        }
        // The crtc the display is already on, if it is on one: the console
        // put it there at boot.
        let current = info
            .current_encoder()
            .and_then(|enc| card.get_encoder(enc).ok())
            .and_then(|enc| enc.crtc());
        // A display plugged in after boot is on none, since nobody has set a
        // mode on it, so it is given one of the crtcs its encoders can drive.
        // Only those: on a Pi 5 the HDMI output can be driven by one crtc
        // alone, and any other leaves it dark while every commit succeeds.
        let Some(crtc) = current.or_else(|| {
            info.encoders()
                .iter()
                .filter_map(|&enc| card.get_encoder(enc).ok())
                .flat_map(|enc| res.filter_crtcs(enc.possible_crtcs()))
                .find(|crtc| !taken.contains(crtc))
        }) else {
            continue;
        };
        let Some(crtc_index) = crtcs.iter().position(|&c| c == crtc) else {
            continue;
        };
        return Ok(Output {
            connector: conn,
            crtc,
            crtc_index: crtc_index as u32,
            name,
        });
    }
    match want {
        Some(want) => Err(format!("{want} is not a connected display")),
        None => Err("no connected display".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name a configuration writes must be the one sysfs shows. The
    /// crate's enum variant is spelled HDMIA, so Debug would render
    /// "HDMIA-1" and no correctly-spelled connector would ever match, so a
    /// wall that names its output would silently refuse to start.
    #[test]
    fn a_connector_is_named_the_way_sysfs_names_it() {
        use drm::control::connector::Interface;
        assert_eq!(
            format!("{}-{}", Interface::HDMIA.as_str(), 1),
            "HDMI-A-1",
            "the name must match /sys/class/drm/card0-HDMI-A-1"
        );
        assert_eq!(format!("{}-{}", Interface::DisplayPort.as_str(), 2), "DP-2");
    }

    /// A viewport carrying nothing but a plane handle: collect_ready only
    /// decides whether to name it, never looks at its geometry.
    fn viewport_at(id: u32) -> Viewport {
        let prop: property::Handle = drm::control::from_u32(1).unwrap();
        Viewport {
            plane: drm::control::from_u32(id).unwrap(),
            props: present::PlaneProps {
                fb_id: prop,
                crtc_id: prop,
                src_x: prop,
                src_y: prop,
                src_w: prop,
                src_h: prop,
                crtc_x: prop,
                crtc_y: prop,
                crtc_w: prop,
                crtc_h: prop,
            },
            crtc_x: 0,
            crtc_y: 0,
            crtc_w: 640,
            crtc_h: 360,
            src_w: 0,
            src_h: 0,
            gap_w: 0,
            gap_h: 0,
        }
    }

    fn planes(ids: &[u32]) -> Vec<plane::Handle> {
        ids.iter()
            .map(|&i| drm::control::from_u32(i).unwrap())
            .collect()
    }

    /// Two displays must not be handed the same plane. On a vc4 every overlay
    /// plane reports every display crtc in possible_crtcs, so both displays
    /// are offered the identical list and only the pool keeps them apart.
    #[test]
    fn displays_share_one_pool_of_planes() {
        let usable = planes(&[1, 2, 3, 4]);
        let (mut spare, mut claimed) = (Vec::new(), Vec::new());

        let first = claim_planes(&usable, 3, &mut spare, &mut claimed).unwrap();
        // The same list again, as the card really would report it.
        let second = claim_planes(&usable, 1, &mut spare, &mut claimed).unwrap();

        assert_eq!(first, planes(&[1, 2, 3]));
        assert_eq!(second, planes(&[4]), "the second display gets what is left");
        assert!(
            !first.iter().any(|p| second.contains(p)),
            "a plane was handed to both displays"
        );
    }

    /// A wall that cannot fit is refused at startup rather than brought up
    /// with a viewport that has no plane to scan out from.
    #[test]
    fn a_display_that_cannot_fit_is_refused() {
        let usable = planes(&[1, 2]);
        let (mut spare, mut claimed) = (Vec::new(), Vec::new());

        assert!(claim_planes(&usable, 2, &mut spare, &mut claimed).is_some());
        assert!(
            claim_planes(&usable, 1, &mut spare, &mut claimed).is_none(),
            "the pool was empty and should not have stretched"
        );
    }

    /// The seam has to be the same width on screen whatever a feed's
    /// resolution, and a feed that matches its viewport has to stay 1:1.
    #[test]
    fn a_viewport_keeps_its_scale_factor_across_the_gap() {
        use present::crop;

        // Matching sizes: exactly the gap comes off, and the result is 1:1
        // against its 639-wide destination.
        assert_eq!(
            crop(640, 1, 639),
            639,
            "a feed that matched its viewport stays 1:1"
        );

        // Twice the size: twice the gap comes off, so the scale is unchanged.
        assert_eq!(crop(1280, 1, 639), 1278);
        assert_eq!(1278 / 639, 1280 / 640, "the scale factor is preserved");

        // Half the size: a fraction of a source pixel cannot be cropped, so
        // the seam costs a whole one and comes out wider than asked for.
        assert_eq!(
            crop(320, 1, 639),
            319,
            "an upscaled viewport still gets a seam"
        );

        // No gap asked for, nothing taken.
        assert_eq!(crop(640, 0, 640), 640);

        // A viewport against the display's edge has no gap, whatever the
        // scale.
        assert_eq!(crop(1280, 0, 640), 1280);
    }

    /// A plane that is already detached must not be detached again. A request
    /// made of nothing but detaches names no crtc, and a commit that touches
    /// no crtc has nothing to hang its page-flip event on, so the kernel
    /// refuses the lot with EINVAL. That is what a wall whose feeds all
    /// dropped together used to do, over and over, until one reconnected.
    #[test]
    fn a_blanked_viewport_is_only_detached_once() {
        let mut feeds: Vec<Feeds> = (0..2).map(|_| rotating(1, 8000)).collect();
        let viewports = vec![viewport_at(1), viewport_at(2)];

        // Both showing nothing and both still attached: both are detached.
        let ready = collect_ready(
            &mut feeds,
            &viewports,
            |_| Some(std::ptr::null()),
            Duration::ZERO,
        );
        assert_eq!(
            ready.len(),
            2,
            "both planes were attached and must be let go"
        );
        assert!(ready.iter().all(|(_, _, fb)| fb.is_none()));

        // The commit above succeeded, so neither is attached any more. There
        // is nothing left to say, and saying it would be refused.
        let ready = collect_ready(&mut feeds, &viewports, |_| None, Duration::ZERO);
        assert!(
            ready.is_empty(),
            "a detached plane was detached again, which commits nothing and fails"
        );

        // One still attached: only that one, and the request keeps a crtc.
        let ready = collect_ready(
            &mut feeds,
            &viewports,
            |i| (i == 1).then_some(std::ptr::null()),
            Duration::ZERO,
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, 1, "the index says which viewport it is");
        assert_eq!(ready[0].1.plane, viewports[1].plane);
    }

    /// Fullscreen, every plane but the first is let go of, and the first
    /// too while the camera has nothing to show, each once.
    #[test]
    fn fullscreen_lets_go_of_the_grid() {
        let viewports = vec![viewport_at(1), viewport_at(2), viewport_at(3)];
        // Neither stream has a frame: a camera whose stream for fullscreen
        // is still connecting, and whose feed has gone away too.
        let camera = Camera {
            feed: Arc::new(Source::new("porch")),
            fullscreen: Some(Arc::new(Source::on_demand("porch (fullscreen)"))),
        };
        let size = (1920, 1080);

        let ready = collect_fullscreen(
            &viewports,
            |_| Some(std::ptr::null()),
            &camera,
            size,
            &mut None,
            Duration::ZERO,
        );
        let let_go: Vec<usize> = ready.iter().map(|(i, _, _)| *i).collect();
        assert_eq!(let_go, [1, 2, 0]);
        assert!(ready.iter().all(|(_, _, fb)| fb.is_none()));

        let ready = collect_fullscreen(
            &viewports,
            |_| None,
            &camera,
            size,
            &mut None,
            Duration::ZERO,
        );
        assert!(ready.is_empty(), "nothing left attached to let go of");
    }

    #[test]
    fn down_and_up_go_through_the_cameras_and_back_returns_to_the_grid() {
        use Mode::*;
        let key = |mode, code: u32| key_move(mode, code as u8, 3);
        assert_eq!(
            key(Grid, CEC_OP_UI_CMD_DOWN),
            Some(Move::Show(Fullscreen(0)))
        );
        assert_eq!(key(Grid, CEC_OP_UI_CMD_UP), Some(Move::Show(Fullscreen(2))));
        assert_eq!(
            key(Fullscreen(0), CEC_OP_UI_CMD_DOWN),
            Some(Move::Show(Fullscreen(1)))
        );
        assert_eq!(
            key(Fullscreen(2), CEC_OP_UI_CMD_DOWN),
            Some(Move::Show(Fullscreen(0))),
            "round from the end"
        );
        assert_eq!(
            key(Fullscreen(0), CEC_OP_UI_CMD_UP),
            Some(Move::Show(Fullscreen(2))),
            "round from the start"
        );
        assert_eq!(
            key(Fullscreen(1), CEC_OP_UI_CMD_BACK),
            Some(Move::Show(Grid))
        );
        assert_eq!(key(Grid, CEC_OP_UI_CMD_BACK), None);
    }

    #[test]
    fn left_and_right_step_the_grid_and_nothing_else() {
        use Mode::*;
        let key = |mode, code: u32| key_move(mode, code as u8, 3);
        assert_eq!(key(Grid, CEC_OP_UI_CMD_RIGHT), Some(Move::Step(1)));
        assert_eq!(key(Grid, CEC_OP_UI_CMD_LEFT), Some(Move::Step(-1)));
        assert_eq!(key(Fullscreen(1), CEC_OP_UI_CMD_RIGHT), None);
        assert_eq!(key(Fullscreen(1), CEC_OP_UI_CMD_LEFT), None);
        assert_eq!(key(Grid, CEC_OP_UI_CMD_SELECT), None);
    }

    fn rotating(feeds: usize, interval_ms: u64) -> Feeds {
        Feeds::new(
            (0..feeds)
                .map(|n| Tap::new(Arc::new(Source::new(&format!("feed{n}")))))
                .collect(),
            Duration::from_millis(interval_ms),
        )
    }

    /// The turn follows from the clock alone, so two viewports with the same
    /// interval agree on it at any moment, however many feeds each has.
    #[test]
    fn viewports_with_one_interval_switch_together() {
        let three = rotating(3, 8000);
        let two = rotating(2, 8000);
        let ms = Duration::from_millis;
        for (at, a, b) in [
            (0, 0, 0),
            (7999, 0, 0),
            (8000, 1, 1),
            (16000, 2, 0),
            (24000, 0, 1),
            // A day in: no drift to accumulate.
            (86_400_000, 0, 0),
            (86_400_000 + 8000, 1, 1),
        ] {
            assert_eq!(three.active(ms(at)), a, "three feeds at {at}ms");
            assert_eq!(two.active(ms(at)), b, "two feeds at {at}ms");
        }
        assert_eq!(rotating(1, 8000).active(ms(123_456)), 0);
    }

    /// A rotating viewport only presents a feed while it is that feed's turn,
    /// so the frames published while another feed had the screen are not
    /// skipped ones. Counting them made a healthy rotating viewport report
    /// thousands.
    #[test]
    fn frames_published_off_screen_are_not_skipped() {
        let mut tap = Tap::new(Arc::new(Source::new("feed")));
        tap.count(1, true);
        tap.count(2, false);
        tap.count(4, false);
        assert_eq!((tap.presented, tap.skipped), (3, 1), "frame 3 was skipped");

        // Off screen for a turn while the feed went on to frame 200.
        tap.count(200, true);
        assert_eq!(
            tap.skipped, 1,
            "the frames published meanwhile were never its to show"
        );
        assert_eq!(tap.presented, 4);

        // And counted as before once it is back.
        tap.count(203, false);
        assert_eq!(tap.skipped, 3);

        // The same frame committed again is neither.
        tap.count(203, false);
        assert_eq!((tap.presented, tap.skipped), (5, 3));
    }

    /// A report covers only its own window, and only the time the feed was
    /// on screen in it, whether or not the one before was said.
    #[test]
    fn a_tap_reports_its_rate_since_the_report_before() {
        let mut tap = Tap::new(Arc::new(Source::new("feed")));
        let s = Duration::from_secs;
        for gen in 1..=250 {
            tap.count(gen, gen == 1);
        }
        assert_eq!(tap.rate(s(10)), Some(25.0));
        for gen in 251..=300 {
            tap.count(gen, false);
        }
        // On screen for 2s of the next window, as a rotating feed is.
        assert_eq!(tap.rate(s(12)), Some(25.0));
        assert_eq!(tap.rate(s(12)), None, "off screen the whole window");
    }

    /// Time on screen is what the report divides presented frames by, so it
    /// must credit each feed only its own turns, including one in progress.
    #[test]
    fn a_feed_is_credited_only_its_own_turns() {
        let feeds = rotating(3, 8000);
        let ms = Duration::from_millis;
        let on = |n, at| feeds.on_screen(n, ms(at)).as_millis();

        assert_eq!(on(0, 5000), 5000, "the first turn, in progress");
        assert_eq!(on(1, 5000), 0, "not yet its turn");
        assert_eq!(on(0, 12000), 8000, "a whole turn, then nothing");
        assert_eq!(on(1, 12000), 4000);
        // Two full cycles and a bit: every feed has had two turns, and the
        // second feed is halfway through its third.
        assert_eq!(on(0, 60000), 24000);
        assert_eq!(on(1, 60000), 16000 + 4000);
        assert_eq!(on(2, 60000), 16000);
        let total: u128 = (0..3).map(|n| on(n, 60000)).sum();
        assert_eq!(total, 60000, "every moment belongs to exactly one feed");

        assert_eq!(
            rotating(1, 8000).on_screen(0, ms(60000)),
            ms(60000),
            "a single feed is always on screen"
        );
    }

    /// Stepping moves on from whatever is on screen, and the feed stepped to
    /// holds the viewport for a whole interval before the rotation goes on.
    #[test]
    fn a_step_starts_the_rotation_again_from_the_feed_stepped_to() {
        let mut feeds = rotating(3, 8000);
        let ms = Duration::from_millis;
        assert_eq!(feeds.active(ms(5000)), 0);

        feeds.step(1, ms(5000));
        assert_eq!(feeds.active(ms(5000)), 1, "on at once");
        assert_eq!(
            feeds.active(ms(12999)),
            1,
            "past when its turn would have ended"
        );
        assert_eq!(feeds.active(ms(13000)), 2, "a whole interval, then on");

        feeds.step(-1, ms(14000));
        assert_eq!(feeds.active(ms(14000)), 1);
        feeds.step(-2, ms(15000));
        assert_eq!(
            feeds.active(ms(15000)),
            2,
            "back past the first wraps round"
        );
        feeds.step(4, ms(16000));
        assert_eq!(feeds.active(ms(16000)), 0);
        assert_eq!(feeds.active(ms(24000)), 1);
    }

    /// Viewports stepped together go on switching together, whatever feed
    /// each was on.
    #[test]
    fn viewports_stepped_together_switch_together() {
        let (mut three, mut two) = (rotating(3, 8000), rotating(2, 8000));
        let ms = Duration::from_millis;
        three.step(1, ms(5000));
        two.step(1, ms(5000));
        for at in [5000, 12999, 13000, 21000, 29000] {
            let (a, b) = (three.active(ms(at)), two.active(ms(at)));
            let (a2, b2) = (three.active(ms(at + 1)), two.active(ms(at + 1)));
            assert_eq!(a != a2, b != b2, "both switch at {at}ms or neither");
        }
    }

    /// A step must not lose or invent time on screen: the report divides by
    /// it, and every moment still belongs to exactly one feed.
    #[test]
    fn a_step_keeps_the_time_on_screen_already_had() {
        let mut feeds = rotating(3, 8000);
        let ms = Duration::from_millis;
        feeds.step(1, ms(5000));
        let on = |f: &Feeds, n, at| f.on_screen(n, ms(at)).as_millis();
        assert_eq!(on(&feeds, 0, 5000), 5000, "what it had before the step");
        assert_eq!(on(&feeds, 1, 7000), 2000);
        assert_eq!(on(&feeds, 0, 7000), 5000);
        feeds.step(-1, ms(7000));
        assert_eq!(on(&feeds, 0, 10000), 8000);
        let total: u128 = (0..3).map(|n| on(&feeds, n, 60000)).sum();
        assert_eq!(total, 60000);
    }

    #[test]
    fn a_single_feed_has_nothing_to_step() {
        let mut feeds = rotating(1, 8000);
        feeds.step(1, Duration::from_millis(5000));
        assert_eq!(feeds.active(Duration::from_millis(5000)), 0);
        assert_eq!(feeds.since, Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_up_to_a_ceiling() {
        let mut b = Backoff::new();
        let mut waits = Vec::new();
        for _ in 0..8 {
            waits.push(b.next());
            b.advance();
        }
        assert_eq!(
            waits,
            [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000].map(Duration::from_millis),
            "a blink is back at once, and the ramp stops at MAX"
        );
        for _ in 0..10 {
            b.advance();
        }
        assert_eq!(
            b.next(),
            Backoff::MAX,
            "a camera down for hours is not hammered"
        );
    }

    #[test]
    fn backoff_starts_over_once_a_camera_has_worked() {
        let mut b = Backoff::new();
        for _ in 0..10 {
            b.advance();
        }
        assert_eq!(b.next(), Backoff::MAX);
        b.reset();
        assert_eq!(
            b.next(),
            Duration::from_millis(500),
            "a feed that ran for a while reconnects promptly when it drops"
        );
    }
}
