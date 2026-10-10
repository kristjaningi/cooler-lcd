//! User config at `~/.config/cooler-lcd/config.toml` (see `dist/config.toml`).
//! Every field is optional; a missing file means the defaults.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Screens to cycle through, in order.
    pub screens: Vec<String>,
    /// How long each screen stays up when there is more than one.
    pub rotate_seconds: u64,
    pub radar: Radar,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Radar {
    /// Compass bearing at the top of the scope, so the map can face the
    /// way the panel does. 0 is north up.
    pub heading: f32,
    /// Latitude and longitude at the middle of the scope; between Keflavik
    /// and Reykjavik when unset.
    #[serde(deserialize_with = "center")]
    pub center: Option<[f32; 2]>,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            screens: vec!["dashboard".into()],
            rotate_seconds: 15,
            radar: Radar::default(),
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
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::parse("screen = [\"dashboard\"]").is_err());
    }
}
