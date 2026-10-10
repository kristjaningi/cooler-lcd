//! The screens the panel can show. Each one gathers its own data and draws
//! itself; the main loop decides which one is visible.
//!
//! Adding a screen: create a module implementing [`Screen`] and register it
//! in [`build`]. Data that is slow to fetch (network, large files) belongs in
//! a background thread, because `update` runs inside the frame loop: the
//! sender keeps resending the last frame, but the picture freezes meanwhile.

mod away;
mod dashboard;
mod flash;
mod radar;
mod sensors;
mod usage;

use std::time::Duration;

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use crate::config::Config;
use crate::theme::Theme;

pub trait Screen {
    /// Refreshes the screen's data. Returns true when the picture changed and
    /// needs redrawing. Called once per tick while the screen is visible.
    fn update(&mut self, now: DateTime<Local>) -> bool;

    /// Draws the whole 480x480 picture.
    fn draw(&self, px: &mut Pixmap, theme: &Theme);

    /// Time between frames while this screen is up. Animated screens ask for
    /// less than the default second.
    fn interval(&self) -> Duration {
        Duration::from_secs(1)
    }

    /// JPEG quality for this screen's frames. At 90 and above the encoder
    /// keeps full-resolution color, so text edges stay clean; animated
    /// screens can trade a little of that for much faster encoding.
    fn jpeg_quality(&self) -> u8 {
        90
    }

    /// Every condition worth taking over the panel for right now, each as a
    /// short text that stays the same for as long as the condition lasts.
    /// Asked about once a second while the carousel is showing (not while
    /// away or flashing, and not with a single screen), whether or not this
    /// screen is the visible one, so keep it cheap. A condition the carousel
    /// hasn't seen in a while brings this screen to the front for one
    /// rotation; one that lasts, or flickers on and off, doesn't again.
    fn alerts(&mut self, _now: DateTime<Local>) -> Vec<String> {
        Vec::new()
    }

    /// True while the first data is still on its way, so `--preview` can
    /// wait for it instead of capturing an empty screen.
    fn loading(&self) -> bool {
        false
    }
}

/// Names accepted in the config's `screens` list.
pub const NAMES: &[&str] = &["dashboard", "usage", "radar"];

pub fn build(name: &str, config: &Config) -> Option<Box<dyn Screen>> {
    match name {
        "dashboard" => Some(Box::new(dashboard::Dashboard::new())),
        "usage" => Some(Box::new(usage::Usage::new())),
        "radar" => Some(Box::new(radar::Radar::new(config.radar.heading))),
        _ => None,
    }
}

/// The dim clock shown instead of the carousel while nobody is at the
/// desktop.
pub fn away() -> Box<dyn Screen> {
    Box::new(away::Away::new())
}

/// A message from `cooler-lcd flash`.
pub fn flash(message: String) -> Box<dyn Screen> {
    Box::new(flash::Flash::new(message))
}
