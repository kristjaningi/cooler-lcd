//! The screens the panel can show. Each one gathers its own data and draws
//! itself; the main loop decides which one is visible.
//!
//! Adding a screen: create a module implementing [`Screen`] and register it
//! in [`build`]. Data that is slow to fetch (network, large files) belongs in
//! a background thread, because `update` runs inside the frame loop and the
//! panel falls back to its logo after ~2-3 s without a frame.

mod dashboard;
mod radar;
mod sensors;
mod usage;

use std::time::Duration;

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

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

    /// True while the first data is still on its way, so `--preview` can
    /// wait for it instead of capturing an empty screen.
    fn loading(&self) -> bool {
        false
    }
}

/// Names accepted in the config's `screens` list.
pub const NAMES: &[&str] = &["dashboard", "usage", "radar"];

pub fn build(name: &str) -> Option<Box<dyn Screen>> {
    match name {
        "dashboard" => Some(Box::new(dashboard::Dashboard::new())),
        "usage" => Some(Box::new(usage::Usage::new())),
        "radar" => Some(Box::new(radar::Radar::new())),
        _ => None,
    }
}
