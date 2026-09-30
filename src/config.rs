//! Reading the wall's configuration.
//!
//! Walked by hand rather than derived: a mistake in a wall's configuration is
//! usually a mistake in its layout, and the messages that matter name the
//! slot rather than a path into a document.

use crate::layout::Layout;
use std::time::Duration;
use yaml_rust2::{Yaml, YamlLoader};

pub const DEFAULT_ROTATION_INTERVAL: Duration = Duration::from_secs(8);
pub const DEFAULT_BACKGROUND: u32 = 0xff00_0000;

#[derive(Debug, Clone)]
pub struct Config {
    /// Every camera the wall shows, in the order the file lists them.
    pub feeds: Vec<Feed>,
    pub displays: Vec<Display>,
    /// Whether to stop decoding for a display nobody is watching: one that
    /// is unplugged, or whose television is off or showing another input.
    pub pause_off_screen: bool,
}

/// A camera, named once and shown wherever a viewport names it.
#[derive(Debug, Clone)]
pub struct Feed {
    /// What the logs call it, and what a viewport says to show it.
    pub name: String,
    pub url: String,
    /// A sharper stream of the same camera, for showing it across the whole
    /// screen, where the one for a viewport would be scaled up soft.
    pub fullscreen_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Display {
    /// Which output to drive. Required once more than one display is
    /// configured, because "the first connected one" stops being an answer.
    pub connector: Option<String>,
    /// A mode to set, or None to keep the one the display is already in.
    pub mode: Option<ModeSpec>,
    /// Pixels of background between neighbouring viewports.
    pub spacing: u32,
    pub background: u32,
    pub layout: Layout,
    pub viewports: Vec<Viewport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: u32,
    pub height: u32,
    /// None means any refresh rate at that size.
    pub refresh: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Viewport {
    /// The slot in the layout this fills, in the order the layout lists them.
    pub slot: usize,
    /// Indices into Config::feeds, in the order the viewport rotates through
    /// them.
    pub feeds: Vec<usize>,
    pub rotation_interval: Duration,
}

impl Config {
    /// How many displays show the feed at index `feed`, on any of their
    /// viewports.
    pub fn displays_showing(&self, feed: usize) -> usize {
        self.displays
            .iter()
            .filter(|d| d.viewports.iter().any(|v| v.feeds.contains(&feed)))
            .count()
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let docs = YamlLoader::load_from_str(text).map_err(|e| format!("not valid YAML: {e}"))?;
        // A fresh install ships this file with everything commented out, so
        // an empty document is the ordinary state of a wall nobody has
        // described yet, not a corrupt one. Say what to do about it.
        let doc = docs
            .first()
            .ok_or("no wall is configured yet; the commented examples in this file show how")?;

        only(
            doc,
            "the configuration",
            &[
                "rotation_interval",
                "spacing",
                "background",
                "pause_off_screen",
                "feeds",
                "displays",
            ],
        )?;

        let feeds = feeds(&doc["feeds"])?;

        let rotation = match &doc["rotation_interval"] {
            Yaml::BadValue => DEFAULT_ROTATION_INTERVAL,
            other => rotation_interval(other)?,
        };
        let spacing = match &doc["spacing"] {
            Yaml::BadValue => 0,
            other => whole(other, "spacing")?,
        };
        let background = match &doc["background"] {
            Yaml::BadValue => DEFAULT_BACKGROUND,
            other => colour(other)?,
        };

        let pause_off_screen = match &doc["pause_off_screen"] {
            Yaml::BadValue => false,
            Yaml::Boolean(b) => *b,
            _ => return Err("pause_off_screen must be true or false".into()),
        };

        let raw = doc["displays"]
            .as_vec()
            .ok_or("the configuration needs a list of displays")?;
        if raw.is_empty() {
            return Err("no displays are configured".into());
        }

        let mut displays: Vec<Display> = Vec::new();
        for (i, d) in raw.iter().enumerate() {
            let display = display(d, &feeds, rotation, spacing, background, raw.len() > 1)
                .map_err(|e| format!("display {}: {e}", i + 1))?;
            // Two walls on one output would each set a mode and claim their
            // own planes from the pool the displays share, then scan out over
            // one another: the planes of whichever lost are simply spent, and
            // the screen shows an interleaving of two walls neither of which
            // was asked for.
            if let Some(name) = &display.connector {
                if let Some(j) = displays
                    .iter()
                    .position(|other| other.connector.as_deref() == Some(name.as_str()))
                {
                    return Err(format!(
                        "displays {} and {} both drive {name}",
                        j + 1,
                        i + 1
                    ));
                }
            }
            displays.push(display);
        }

        let config = Config {
            feeds,
            displays,
            pause_off_screen,
        };
        for (i, feed) in config.feeds.iter().enumerate() {
            // A feed no viewport names is a camera connected to and decoded
            // for nobody, or more likely a name misspelled on one side or the
            // other. Either way the wall would come up without it and say
            // nothing.
            if config.displays_showing(i) == 0 {
                return Err(format!("no viewport shows the feed '{}'", feed.name));
            }
        }
        Ok(config)
    }
}

/// The cameras, as a mapping from the name viewports use to the url, or to
/// a mapping of its settings.
///
/// A mapping rather than a list, so a name can only be given once: the YAML
/// parser refuses a repeated key before this ever sees it.
fn feeds(y: &Yaml) -> Result<Vec<Feed>, String> {
    let hash = y
        .as_hash()
        .ok_or("the configuration needs a mapping of feeds, from a name to a url")?;
    if hash.is_empty() {
        return Err("no feeds are configured".into());
    }
    hash.iter()
        .map(|(k, v)| {
            let name = feed_name(k)?;
            if let Some(url) = v.as_str() {
                return Ok(Feed {
                    name,
                    url: url.to_string(),
                    fullscreen_url: None,
                });
            }
            let what = format!("the feed '{name}'");
            if v.as_hash().is_none() {
                return Err(format!("{what} must be a url, or a mapping with one"));
            }
            only(v, &what, &["url", "fullscreen_url"])?;
            let url = |key: &str| match &v[key] {
                Yaml::BadValue => Ok(None),
                y => y
                    .as_str()
                    .map(|s| Some(s.to_string()))
                    .ok_or_else(|| format!("{what}'s {key} must be a url")),
            };
            Ok(Feed {
                url: url("url")?.ok_or_else(|| format!("{what} needs a url"))?,
                fullscreen_url: url("fullscreen_url")?,
                name,
            })
        })
        .collect()
}

/// A feed's name, where it is defined and where a viewport names it.
///
/// A number is a fine name to write, "1: rtsp://...", and YAML reads it as an
/// integer, so it is taken as the text it was written as rather than refused
/// for its type. Both places read it the same way, or "1" could be defined
/// and then not found.
fn feed_name(y: &Yaml) -> Result<String, String> {
    match y {
        Yaml::String(s) => Ok(s.clone()),
        Yaml::Integer(n) => Ok(n.to_string()),
        _ => Err("a feed's name must be a word or a number".into()),
    }
}

fn display(
    d: &Yaml,
    feeds: &[Feed],
    rotation: Duration,
    spacing: u32,
    background: u32,
    several: bool,
) -> Result<Display, String> {
    only(
        d,
        "a display",
        // No rotation_interval: it is a property of a viewport, defaulted at
        // the root, and a display is only the conduit between the two.
        &[
            "connector",
            "mode",
            "spacing",
            "background",
            "layout",
            "viewports",
        ],
    )?;

    let connector = d["connector"].as_str().map(str::to_string);
    if several && connector.is_none() {
        return Err(
            "a connector is needed once more than one display is configured, \
             because there is no longer a single connected output to mean"
                .into(),
        );
    }

    let mode = match d["mode"].as_str() {
        Some(text) => Some(mode(text)?),
        None => None,
    };
    let spacing = match &d["spacing"] {
        Yaml::BadValue => spacing,
        other => whole(other, "spacing")?,
    };
    let background = match &d["background"] {
        Yaml::BadValue => background,
        other => colour(other)?,
    };

    let raw = d["viewports"]
        .as_vec()
        .ok_or("a display needs a list of viewports")?;
    if raw.is_empty() {
        return Err("a display needs at least one viewport".into());
    }

    let layout = match d["layout"].as_str() {
        Some(text) => Layout::parse(text)?,
        None => Layout::inferred(raw.len()),
    };

    let mut viewports = Vec::new();
    for (i, v) in raw.iter().enumerate() {
        viewports.push(
            viewport(
                v,
                i,
                feeds,
                &layout,
                d["layout"].as_str().is_some(),
                rotation,
            )
            .map_err(|e| format!("viewport {}: {e}", i + 1))?,
        );
    }

    // Every slot needs a viewport: a named cell with nothing behind it would
    // be a hole the author did not ask for, and "." is how one is asked for.
    for (name, _) in &layout.slots {
        if !viewports.iter().any(|v| layout.slots[v.slot].0 == *name) {
            return Err(format!(
                "the layout has a slot '{name}' that no viewport fills"
            ));
        }
    }

    // And no slot may have two. The pair of checks is what makes the mapping
    // one to one, which is what the presenter assumes when it pairs the nth
    // viewport with the nth plane: two viewports on one slot would put two
    // planes at identical geometry, so one camera would sit invisibly
    // underneath the other and a plane would be spent showing nothing.
    for (i, v) in viewports.iter().enumerate() {
        if let Some(j) = viewports[..i].iter().position(|w| w.slot == v.slot) {
            return Err(format!(
                "viewports {} and {} both fill slot '{}'",
                j + 1,
                i + 1,
                layout.slots[v.slot].0
            ));
        }
    }

    Ok(Display {
        connector,
        mode,
        spacing,
        background,
        layout,
        viewports,
    })
}

fn viewport(
    v: &Yaml,
    index: usize,
    all: &[Feed],
    layout: &Layout,
    has_layout: bool,
    rotation: Duration,
) -> Result<Viewport, String> {
    only(v, "a viewport", &["slot", "feeds", "rotation_interval"])?;

    let slot = match v["slot"].as_str() {
        Some(name) => {
            if !has_layout {
                return Err(format!(
                    "a slot ('{name}') only means something with a layout to \
                     place it in"
                ));
            }
            layout
                .slots
                .iter()
                .position(|(n, _)| n == name)
                .ok_or_else(|| format!("there is no slot '{name}' in the layout"))?
        }
        None => {
            if has_layout {
                return Err("a slot is needed to say where this goes in the layout".into());
            }
            index
        }
    };

    let names = v["feeds"]
        .as_vec()
        .ok_or("a viewport needs a list of feeds")?;
    if names.is_empty() {
        return Err("a viewport needs at least one feed".into());
    }
    let mut feeds: Vec<usize> = Vec::new();
    for n in names {
        let name = feed_name(n)?;
        let Some(at) = all.iter().position(|f| f.name == name) else {
            return Err(format!("there is no feed named '{name}'"));
        };
        // Rotating through one camera twice shows it for twice as long,
        // which is a rotation_interval nobody asked for, and more likely a
        // copied line that should have named another camera.
        if feeds.contains(&at) {
            return Err(format!("the feed '{name}' is named twice"));
        }
        feeds.push(at);
    }

    let rotation_interval = match &v["rotation_interval"] {
        Yaml::BadValue => rotation,
        other => rotation_interval(other)?,
    };

    Ok(Viewport {
        slot,
        feeds,
        rotation_interval,
    })
}

/// "1920x1080" or "1920x1080@60".
fn mode(text: &str) -> Result<ModeSpec, String> {
    let bad = || format!("'{text}' is not a mode like 1920x1080 or 1920x1080@60");
    let (size, refresh) = match text.split_once('@') {
        Some((size, rate)) => (size, Some(rate.parse().map_err(|_| bad())?)),
        None => (text, None),
    };
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    Ok(ModeSpec {
        width: w.parse().map_err(|_| bad())?,
        height: h.parse().map_err(|_| bad())?,
        refresh,
    })
}

/// "#rrggbb", as CSS writes it.
fn colour(y: &Yaml) -> Result<u32, String> {
    let text = y
        .as_str()
        .ok_or("a background must be a colour like \"#000000\"")?;
    let hex = text.strip_prefix('#').unwrap_or(text);
    let rgb = u32::from_str_radix(hex, 16)
        .map_err(|_| format!("'{text}' is not a colour like \"#000000\""))?;
    if hex.len() != 6 {
        return Err(format!("'{text}' is not a colour like \"#000000\""));
    }
    // Opaque, in the format a scanout buffer wants.
    Ok(0xff00_0000 | rgb)
}

fn number(y: &Yaml, what: &str) -> Result<f64, String> {
    match y {
        Yaml::Integer(n) => Ok(*n as f64),
        Yaml::Real(s) => s.parse().map_err(|_| format!("{what} must be a number")),
        _ => Err(format!("{what} must be a number")),
    }
}

/// Refuse a mapping that carries a key this file does not understand.
///
/// A key nobody reads is almost always a key spelled wrong, and the wall then
/// comes up quietly ignoring whatever it said: "conector" leaves the wall on
/// the first connected display rather than the named one, and nothing in the
/// log mentions it. The parser cannot tell an unknown key from a typo, so it
/// treats both as a mistake worth stopping for.
///
/// Named rather than listed as a count, because the useful half of the
/// message is which word was not understood.
fn only(y: &Yaml, what: &str, allowed: &[&str]) -> Result<(), String> {
    let Some(hash) = y.as_hash() else {
        return Ok(());
    };
    let mut unknown: Vec<&str> = hash
        .keys()
        .filter_map(|k| k.as_str())
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    Err(format!(
        "{what} does not understand {}; it takes {}",
        unknown
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(", "),
        allowed.join(", ")
    ))
}

/// A count of pixels, cells or anything else that cannot be negative.
///
/// `as u32` would take a negative to 0 and a huge one to u32::MAX, both
/// silently: "spacing: -1" would read as no spacing at all rather than as the
/// mistake it is.
fn whole(y: &Yaml, what: &str) -> Result<u32, String> {
    let n = number(y, what)?;
    if !n.is_finite() || n < 0.0 || n > u32::MAX as f64 {
        return Err(format!("{what} must be a positive number of pixels"));
    }
    Ok(n as u32)
}

/// How long a rotating viewport holds each feed, given in seconds.
///
/// Duration::from_secs_f64 panics outright on a negative or a NaN, so a typo
/// as small as a leading minus would abort the process with a message from
/// libstd instead of the one this file exists to give.
///
/// At least a second. Anything shorter shows a camera for fewer frames than it
/// may take to deliver one, and the rotation is timed in whole milliseconds,
/// where a fraction of one would be a turn of no length at all.
fn rotation_interval(y: &Yaml) -> Result<Duration, String> {
    let what = "rotation_interval";
    let n = number(y, what)?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("{what} must be a positive number of seconds"));
    }
    if n < 1.0 {
        return Err(format!("{what} must be at least a second"));
    }
    Duration::try_from_secs_f64(n).map_err(|_| format!("{what} is too long"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_needs_only_its_feeds() {
        let c = Config::parse(
            r#"
feeds:
  a: rtsp://a
  b: rtsp://b
displays:
  - viewports:
      - {feeds: [a]}
      - {feeds: [b]}
"#,
        )
        .unwrap();
        let d = &c.displays[0];
        assert_eq!(
            d.spacing, 0,
            "viewports touch unless a spacing is asked for"
        );
        assert_eq!(d.background, DEFAULT_BACKGROUND);
        assert!(d.mode.is_none(), "the display keeps the mode it is in");
        assert_eq!(d.layout.size, 2, "two feeds infer a 2x2 grid");
        assert_eq!(d.viewports[1].rotation_interval, DEFAULT_ROTATION_INTERVAL);
        assert!(!c.pause_off_screen, "a wall decodes whether watched or not");
    }

    #[test]
    fn pausing_off_screen_is_yes_or_no() {
        let wall = |pause: &str| {
            Config::parse(&format!(
                "pause_off_screen: {pause}\nfeeds: {{a: rtsp://a}}\ndisplays: [{{viewports: [{{feeds: [a]}}]}}]"
            ))
        };
        assert!(wall("true").unwrap().pause_off_screen);
        assert!(!wall("false").unwrap().pause_off_screen);
        assert_eq!(
            wall("sometimes").unwrap_err(),
            "pause_off_screen must be true or false"
        );
    }

    #[test]
    fn a_layout_places_viewports_by_slot() {
        let c = Config::parse(
            r#"
feeds:
  e: rtsp://e
  a: rtsp://a
  b: rtsp://b
  c: rtsp://c
  d: rtsp://d
  f: rtsp://f
displays:
  - layout: |
      a a b
      a a c
      d e f
    viewports:
      - {slot: e, feeds: [e]}
      - {slot: a, feeds: [a]}
      - {slot: b, feeds: [b]}
      - {slot: c, feeds: [c]}
      - {slot: d, feeds: [d]}
      - {slot: f, feeds: [f]}
"#,
        )
        .unwrap();
        let d = &c.displays[0];
        // Listed in a different order than the picture: the slot decides.
        assert_eq!(d.layout.slots[d.viewports[0].slot].0, "e");
        assert_eq!(d.layout.slots[d.viewports[1].slot].0, "a");
        assert_eq!(d.layout.slots[d.viewports[1].slot].1.span, 2);
    }

    #[test]
    fn settings_fall_back_from_the_root() {
        let c = Config::parse(
            r##"
feeds:
  a: rtsp://a
rotation_interval: 20
spacing: 2
background: "#102030"
displays:
  - viewports: [{feeds: [a]}]
"##,
        )
        .unwrap();
        let d = &c.displays[0];
        assert_eq!(d.spacing, 2);
        assert_eq!(d.background, 0xff10_2030);
        assert_eq!(d.viewports[0].rotation_interval, Duration::from_secs(20));
    }

    #[test]
    fn a_display_overrides_what_it_names() {
        let c = Config::parse(
            r#"
feeds:
  a: rtsp://a
spacing: 2
displays:
  - spacing: 5
    viewports: [{feeds: [a]}]
"#,
        )
        .unwrap();
        assert_eq!(c.displays[0].spacing, 5);
    }

    #[test]
    fn a_viewport_overrides_its_rotation() {
        let c = Config::parse(
            r#"
feeds:
  a: rtsp://a
  b: rtsp://b
displays:
  - viewports:
      - {rotation_interval: 30, feeds: [a, b]}
"#,
        )
        .unwrap();
        assert_eq!(
            c.displays[0].viewports[0].rotation_interval,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn a_second_display_must_say_which_connector() {
        let err = Config::parse(
            r#"
feeds:
  a: rtsp://a
  b: rtsp://b
displays:
  - viewports: [{feeds: [a]}]
  - viewports: [{feeds: [b]}]
"#,
        )
        .unwrap_err();
        assert!(err.contains("connector is needed"), "{err}");
    }

    #[test]
    fn one_display_may_leave_the_connector_out() {
        let c = Config::parse("feeds:\n  a: rtsp://a\ndisplays:\n  - viewports: [{feeds: [a]}]\n")
            .unwrap();
        assert!(c.displays[0].connector.is_none());
    }

    #[test]
    fn a_slot_no_viewport_fills_is_refused() {
        let err = Config::parse(
            r#"
feeds:
  a: rtsp://a
displays:
  - layout: |
      a b
      c d
    viewports:
      - {slot: a, feeds: [a]}
"#,
        )
        .unwrap_err();
        assert!(err.contains("no viewport fills"), "{err}");
    }

    #[test]
    fn a_slot_that_is_not_in_the_layout_is_refused() {
        let err = Config::parse(
            r#"
feeds:
  z: rtsp://z
displays:
  - layout: |
      a b
      c d
    viewports:
      - {slot: z, feeds: [z]}
"#,
        )
        .unwrap_err();
        assert!(err.contains("no slot 'z'"), "{err}");
    }

    /// A negative interval used to reach Duration::from_secs_f64, which
    /// panics: a minus sign in the wrong place aborted the process with a
    /// message from libstd rather than saying which setting was wrong.
    #[test]
    fn a_time_that_cannot_pass_is_refused() {
        for bad in ["-1", "0"] {
            let err = Config::parse(&format!(
                "feeds:\n  a: rtsp://a\nrotation_interval: {bad}\ndisplays:\n  - viewports: [{{feeds: [a]}}]\n"
            ))
            .unwrap_err();
            assert!(err.contains("positive number of seconds"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_rotation_shorter_than_a_second_is_refused() {
        let err = Config::parse(
            "feeds:\n  a: rtsp://a\n  b: rtsp://b\ndisplays:\n  - viewports: [{rotation_interval: 0.5, feeds: [a, b]}]\n",
        )
        .unwrap_err();
        assert!(err.contains("at least a second"), "{err}");
    }

    /// Negative spacing used to saturate to 0 through `as u32`, so a mistake
    /// read as "no spacing" and the wall came up looking almost right.
    #[test]
    fn a_negative_spacing_is_refused() {
        let err = Config::parse(
            "feeds:\n  a: rtsp://a\nspacing: -1\ndisplays:\n  - viewports: [{feeds: [a]}]\n",
        )
        .unwrap_err();
        assert!(err.contains("positive number of pixels"), "{err}");
    }

    /// Two viewports on one slot would put two planes at the same geometry,
    /// hiding one camera under the other and spending a plane on nothing.
    #[test]
    fn two_viewports_cannot_share_a_slot() {
        let err = Config::parse(
            r#"
feeds:
  a: rtsp://a
  a2: rtsp://a2
  b: rtsp://b
  c: rtsp://c
  d: rtsp://d
displays:
  - layout: |
      a b
      c d
    viewports:
      - {slot: a, feeds: [a]}
      - {slot: a, feeds: [a2]}
      - {slot: b, feeds: [b]}
      - {slot: c, feeds: [c]}
      - {slot: d, feeds: [d]}
"#,
        )
        .unwrap_err();
        assert!(err.contains("both fill slot 'a'"), "{err}");
    }

    /// Both walls would drive one crtc, each modesetting over the other.
    #[test]
    fn two_displays_cannot_drive_one_connector() {
        let err = Config::parse(
            r#"
feeds:
  a: rtsp://a
  b: rtsp://b
displays:
  - connector: HDMI-A-1
    viewports: [{feeds: [a]}]
  - connector: HDMI-A-1
    viewports: [{feeds: [b]}]
"#,
        )
        .unwrap_err();
        assert!(err.contains("both drive HDMI-A-1"), "{err}");
    }

    /// A key nobody reads is a key spelled wrong. Silently ignoring it leaves
    /// the wall running as though the line had not been written: 'conector'
    /// would drive the first connected display rather than the named one,
    /// with nothing in the log to say why.
    #[test]
    fn a_key_that_is_not_understood_is_refused() {
        let cases = [
            ("colour: \"#101010\"\ndisplays: []\n", "colour"),
            (
                "feeds:\n  a: rtsp://a\ndisplays:\n  - conector: HDMI-A-1\n    viewports: [{feeds: [a]}]\n",
                "conector",
            ),
            ("feeds:\n  a: rtsp://a\ndisplays:\n  - viewports: [{feed: [a]}]\n", "feed"),
        ];
        for (text, bad) in cases {
            let err = Config::parse(text).unwrap_err();
            assert!(
                err.contains(&format!("'{bad}'")),
                "{bad} was accepted: {err}"
            );
            assert!(
                err.contains("does not understand"),
                "the message should name the word: {err}"
            );
        }
    }

    /// A display narrows what a display has: spacing and background. It is
    /// not where rotation is set, so asking for it there is a mistake to
    /// name rather than a default to invent.
    #[test]
    fn a_display_narrows_spacing_and_background_but_not_rotation() {
        let c = Config::parse(
            r##"
feeds:
  a: rtsp://a
  b: rtsp://b
spacing: 1
background: "#000000"
displays:
  - spacing: 7
    background: "#102030"
    viewports: [{feeds: [a, b]}]
"##,
        )
        .unwrap();
        let d = &c.displays[0];
        assert_eq!(d.spacing, 7);
        assert_eq!(d.background, 0xff10_2030);

        let err = Config::parse(
            "feeds:\n  a: rtsp://a\ndisplays:\n  - rotation_interval: 30\n    viewports: [{feeds: [a]}]\n",
        )
        .unwrap_err();
        assert!(
            err.contains("'rotation_interval'"),
            "a display is not where rotation is set: {err}"
        );
    }

    /// One camera in several places is the point of naming it: every
    /// viewport that names it points at the same feed.
    #[test]
    fn a_feed_may_be_shown_in_several_places() {
        let c = Config::parse(
            r#"
feeds:
  porch: rtsp://porch
  yard: rtsp://yard
displays:
  - connector: HDMI-A-1
    viewports:
      - {feeds: [yard, porch]}
      - {feeds: [porch]}
  - connector: HDMI-A-2
    viewports: [{feeds: [porch]}]
"#,
        )
        .unwrap();
        assert_eq!(c.feeds.len(), 2, "one feed per camera, however often shown");
        assert_eq!(c.feeds[0].name, "porch");
        assert_eq!(c.feeds[0].url, "rtsp://porch");
        assert_eq!(c.displays[0].viewports[0].feeds, [1, 0], "rotation order");
        assert_eq!(c.displays[0].viewports[1].feeds, [0]);
        assert_eq!(c.displays[1].viewports[0].feeds, [0]);
    }

    #[test]
    fn a_feed_may_have_a_stream_of_its_own_for_fullscreen() {
        let c = Config::parse(
            r#"
feeds:
  porch: rtsp://porch
  yard:
    url: rtsp://yard
    fullscreen_url: rtsp://yard-hd
  gate: {url: rtsp://gate}
displays:
  - viewports: [{feeds: [porch]}, {feeds: [yard]}, {feeds: [gate]}]
"#,
        )
        .unwrap();
        let streams: Vec<_> = c
            .feeds
            .iter()
            .map(|f| (f.url.as_str(), f.fullscreen_url.as_deref()))
            .collect();
        assert_eq!(
            streams,
            [
                ("rtsp://porch", None),
                ("rtsp://yard", Some("rtsp://yard-hd")),
                ("rtsp://gate", None),
            ]
        );
    }

    #[test]
    fn a_feed_written_out_needs_its_url_and_nothing_unknown() {
        let parse = |feed: &str| {
            Config::parse(&format!(
                "feeds:\n  yard: {feed}\ndisplays:\n  - viewports: [{{feeds: [yard]}}]\n"
            ))
            .unwrap_err()
        };
        let err = parse("{fullscreen_url: rtsp://yard-hd}");
        assert!(err.contains("needs a url"), "{err}");
        let err = parse("{url: rtsp://yard, fulscreen_url: rtsp://yard-hd}");
        assert!(err.contains("'fulscreen_url'"), "{err}");
        let err = parse("{url: [rtsp://yard]}");
        assert!(err.contains("url must be a url"), "{err}");
        let err = parse("[rtsp://yard]");
        assert!(err.contains("must be a url, or a mapping"), "{err}");
    }

    #[test]
    fn a_feed_that_is_not_defined_is_refused() {
        let err = Config::parse(
            "feeds:\n  porch: rtsp://porch\ndisplays:\n  - viewports: [{feeds: [porch, prch]}]\n",
        )
        .unwrap_err();
        assert!(err.contains("no feed named 'prch'"), "{err}");
        assert!(err.contains("viewport 1"), "the message says where: {err}");
    }

    #[test]
    fn a_feed_no_viewport_shows_is_refused() {
        let err = Config::parse(
            "feeds:\n  porch: rtsp://porch\n  yard: rtsp://yard\ndisplays:\n  - viewports: [{feeds: [porch]}]\n",
        )
        .unwrap_err();
        assert!(err.contains("no viewport shows the feed 'yard'"), "{err}");
    }

    #[test]
    fn a_viewport_cannot_rotate_through_a_feed_twice() {
        let err = Config::parse(
            "feeds:\n  porch: rtsp://porch\n  yard: rtsp://yard\ndisplays:\n  - viewports: [{feeds: [porch, yard, porch]}]\n",
        )
        .unwrap_err();
        assert!(err.contains("'porch' is named twice"), "{err}");
    }

    /// Two cameras under one name would leave one of them unreachable, and
    /// which one would depend on the parser.
    #[test]
    fn a_name_cannot_be_given_twice() {
        let err = Config::parse(
            "feeds:\n  porch: rtsp://a\n  porch: rtsp://b\ndisplays:\n  - viewports: [{feeds: [porch]}]\n",
        )
        .unwrap_err();
        assert!(err.contains("duplicated key"), "{err}");
    }

    #[test]
    fn a_wall_needs_feeds() {
        let err = Config::parse("displays:\n  - viewports: [{feeds: [porch]}]\n").unwrap_err();
        assert!(err.contains("mapping of feeds"), "{err}");
    }

    #[test]
    fn modes_are_read_with_and_without_a_rate() {
        assert_eq!(
            mode("1920x1080").unwrap(),
            ModeSpec {
                width: 1920,
                height: 1080,
                refresh: None
            }
        );
        assert_eq!(
            mode("1920x1080@60").unwrap(),
            ModeSpec {
                width: 1920,
                height: 1080,
                refresh: Some(60)
            }
        );
        assert!(mode("1080p").is_err());
    }
}
