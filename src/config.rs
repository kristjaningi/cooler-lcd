//! User config at `~/.config/cooler-lcd/config.toml` (see `dist/config.toml`).
//! Every field is optional; a missing file means the defaults.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::NaiveTime;
use serde::Deserialize;

/// The radar's center, and where night mode's sun is worked out from,
/// unless the config sets one: between Keflavik and Reykjavik.
pub const CENTER: (f32, f32) = (64.05, -22.25);

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Screens to cycle through, in order.
    pub screens: Vec<String>,
    /// How long each screen stays up when there is more than one.
    pub rotate_seconds: u64,
    /// Screens from `screens` that start switched off: out of the rotation
    /// until `cooler-lcd on NAME`.
    pub off: Vec<String>,
    /// Whether a bar along the bottom edge counts down to the next screen.
    pub rotate_bar: bool,
    pub radar: Radar,
    pub night: Night,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Radar {
    /// Compass bearing at the top of the scope, so the map can face the
    /// way the panel does. 0 is north up.
    #[serde(deserialize_with = "heading")]
    pub heading: f32,
    /// Latitude and longitude at the middle of the scope; between Keflavik
    /// and Reykjavik when unset.
    #[serde(deserialize_with = "center")]
    pub center: Option<[f32; 2]>,
}

/// A finite number of degrees: TOML takes `nan` and `inf` too, which
/// would turn every point on the scope into NaN, leaving it blank.
fn heading<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    let heading = f32::deserialize(d)?;
    if !heading.is_finite() {
        return Err(serde::de::Error::custom(
            "radar heading is a compass bearing in degrees",
        ));
    }
    Ok(heading)
}

fn center<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<[f32; 2]>, D::Error> {
    let [lat, lon] = <[f32; 2]>::deserialize(d)?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(serde::de::Error::custom(
            "radar center is [latitude, longitude], within ±90 and ±180",
        ));
    }
    Ok(Some([lat, lon]))
}

impl Radar {
    /// Latitude and longitude at the middle of the scope.
    pub fn center(&self) -> (f32, f32) {
        self.center.map_or(CENTER, |[lat, lon]| (lat, lon))
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Night {
    /// When the panel dims on its own.
    #[serde(deserialize_with = "schedule")]
    pub schedule: Schedule,
    /// What the brightest colors keep of their brightness at night.
    #[serde(deserialize_with = "brightness")]
    pub brightness: f32,
}

impl Default for Night {
    fn default() -> Self {
        Self {
            schedule: Schedule::Hours(
                NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
                NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
            ),
            brightness: 0.6,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Schedule {
    /// From sunset to sunrise at the radar's center.
    Sunset,
    /// From the first time of day to the second, on the local clock,
    /// across midnight if the second comes first.
    Hours(NaiveTime, NaiveTime),
    /// Only when `cooler-lcd night on` says so.
    Never,
}

fn schedule<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Schedule, D::Error> {
    let s = String::deserialize(d)?;
    parse_schedule(&s).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "night schedule is \"sunset\", hours like \"22:00-07:00\", or \"never\" (got {s:?})"
        ))
    })
}

fn parse_schedule(s: &str) -> Option<Schedule> {
    match s.trim() {
        "sunset" => Some(Schedule::Sunset),
        "never" => Some(Schedule::Never),
        hours => {
            let (from, to) = hours.split_once('-')?;
            let time = |t: &str| NaiveTime::parse_from_str(t.trim(), "%H:%M").ok();
            let (from, to) = (time(from)?, time(to)?);
            (from != to).then_some(Schedule::Hours(from, to))
        }
    }
}

/// A fraction of full brightness: too little and nothing can be read.
fn brightness<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    let b = f32::deserialize(d)?;
    if !(0.2..=1.0).contains(&b) {
        return Err(serde::de::Error::custom(
            "night brightness is a fraction from 0.2 to 1",
        ));
    }
    Ok(b)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            screens: vec!["dashboard".into()],
            rotate_seconds: 15,
            off: Vec::new(),
            rotate_bar: true,
            radar: Radar::default(),
            night: Night::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = path();
        match fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn rotate(&self) -> Duration {
        Duration::from_secs(self.rotate_seconds.max(1))
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("cooler-lcd/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn parses_fields() {
        let c = Config::parse("screens = [\"dashboard\", \"x\"]\nrotate_seconds = 30").unwrap();
        assert_eq!(c.screens, ["dashboard", "x"]);
        assert_eq!(c.rotate(), Duration::from_secs(30));
        let c = Config::parse("[radar]\nheading = 111").unwrap();
        assert_eq!(c.radar.heading, 111.0);
        assert_eq!(c.radar.center, None);
        let c = Config::parse("[radar]\ncenter = [64.0, -22.0]").unwrap();
        assert_eq!(c.radar.center, Some([64.0, -22.0]));
        assert!(Config::parse("[radar]\ncenter = [-22.0, 640.0]").is_err());
        assert!(Config::parse("[radar]\nheading = nan").is_err());
        assert!(Config::parse("[radar]\nheading = -inf").is_err());
        let c = Config::parse("off = [\"radar\"]\nrotate_bar = false").unwrap();
        assert_eq!(c.off, ["radar"]);
        assert!(!c.rotate_bar);
    }

    #[test]
    fn parses_night() {
        let night = |s: &str| Config::parse(&format!("[night]\n{s}")).map(|c| c.night);
        let at = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!(
            Config::default().night.schedule,
            Schedule::Hours(at(22, 0), at(7, 0))
        );
        assert_eq!(
            night("schedule = \"sunset\"").unwrap().schedule,
            Schedule::Sunset
        );
        assert_eq!(
            night("schedule = \"never\"").unwrap().schedule,
            Schedule::Never
        );
        assert_eq!(
            night("schedule = \" 21:30 - 6:15 \"").unwrap().schedule,
            Schedule::Hours(at(21, 30), at(6, 15))
        );
        let n = night("brightness = 0.4").unwrap();
        assert_eq!(n.brightness, 0.4);
        for bad in [
            "schedule = \"dusk\"",
            "schedule = \"22:00\"",
            "schedule = \"22:00-22:00\"",
            "schedule = \"25:00-07:00\"",
            "schedule = 22",
            "brightness = 0.1",
            "brightness = 1.5",
            "brightness = nan",
            "bright = 0.5",
        ] {
            assert!(night(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_center_defaults_between_the_airports() {
        assert_eq!(Config::default().radar.center(), CENTER);
        let c = Config::parse("[radar]\ncenter = [65.68, -18.09]").unwrap();
        assert_eq!(c.radar.center(), (65.68, -18.09));
    }

    #[test]
    fn the_sample_config_is_the_defaults() {
        let sample = Config::parse(include_str!("../dist/config.toml")).unwrap();
        assert_eq!(sample, Config::default());
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::parse("screen = [\"dashboard\"]").is_err());
    }
}
