//! A message sent with `cooler-lcd flash TEXT` (from a Claude Code hook, a
//! script, a keybinding), shown in large type for one rotation.

use ab_glyph::FontVec;
use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{ellipsize, fill, rounded_rect, text, wrap};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

const WIDTH: f32 = SIZE as f32 - 2.0 * 44.0;
/// Text sizes to try, largest first, until the message fits in `MAX_LINES`.
const SIZES: [f32; 4] = [52.0, 42.0, 34.0, 28.0];
const MAX_LINES: usize = 6;

pub struct Flash {
    message: String,
    color: Option<Rgb>,
    icon: Option<String>,
    drawn: bool,
}

impl Flash {
    pub fn new(message: String, color: Option<Rgb>, icon: Option<String>) -> Self {
        Self {
            message,
            color,
            icon,
            drawn: false,
        }
    }
}

impl Screen for Flash {
    fn update(&mut self, _now: DateTime<Local>) -> bool {
        !std::mem::replace(&mut self.drawn, true)
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        fill(px, theme.background);
        let c = SIZE as f32 / 2.0;
        let icon_h = if self.icon.is_some() { 96.0 } else { 0.0 };
        let accent = self.color.unwrap_or(theme.accent);
        let (size, lines) = fit(&theme.font, &self.message, icon_h);
        let line_h = size * 1.25;
        let block = lines.len() as f32 * line_h + icon_h;
        // An accent bar above the message, the block centered under it.
        let mut top = c - block / 2.0 + 14.0;
        rounded_rect(px, c - 36.0, top - 44.0, 72.0, 8.0, 4.0, accent);
        if let Some(icon) = &self.icon {
            text(px, &theme.font, icon, 64.0, c, top + 55.0, accent);
            top += icon_h;
        }
        for (i, line) in lines.iter().enumerate() {
            let baseline = top + i as f32 * line_h + size * 0.8;
            text(px, &theme.font, line, size, c, baseline, theme.foreground);
        }
    }
}

/// The largest size at which the message wraps into at most `MAX_LINES`,
/// and its lines; the smallest size cuts it off with an ellipsis.
fn fit(font: &FontVec, message: &str, icon_h: f32) -> (f32, Vec<String>) {
    for size in SIZES {
        let lines = wrap(font, message, size, WIDTH);
        if lines.len() <= MAX_LINES && lines.len() as f32 * size * 1.25 + icon_h <= 392.0 {
            return (size, lines);
        }
    }
    let size = SIZES[SIZES.len() - 1];
    let mut lines = wrap(font, message, size, WIDTH);
    lines.truncate(MAX_LINES);
    if let Some(last) = lines.last_mut() {
        *last = ellipsize(font, &format!("{last}…"), size, WIDTH);
    }
    (size, lines)
}
