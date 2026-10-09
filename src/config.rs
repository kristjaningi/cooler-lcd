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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            screens: vec!["dashboard".into()],
            rotate_seconds: 15,
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
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::parse("screen = [\"dashboard\"]").is_err());
    }
}
