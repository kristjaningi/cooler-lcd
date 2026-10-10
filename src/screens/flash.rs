//! A message sent with `cooler-lcd flash TEXT` (from a Claude Code hook, a
//! script, a keybinding), shown in large type for one rotation.

use ab_glyph::FontVec;
use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{fill, rounded_rect, text, text_width};
use crate::render::SIZE;
use crate::theme::Theme;

const WIDTH: f32 = SIZE as f32 - 2.0 * 44.0;
/// Text sizes to try, largest first, until the message fits in `MAX_LINES`.
const SIZES: [f32; 4] = [52.0, 42.0, 34.0, 28.0];
const MAX_LINES: usize = 6;

pub struct Flash {
    message: String,
    drawn: bool,
}

impl Flash {
    pub fn new(message: String) -> Self {
        Self {
            message,
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
        let (size, lines) = fit(&theme.font, &self.message);
        let line_h = size * 1.25;
        let block = lines.len() as f32 * line_h;
        // An accent bar above the message, the block centered under it.
        let top = c - block / 2.0 + 14.0;
        rounded_rect(px, c - 36.0, top - 44.0, 72.0, 8.0, 4.0, theme.accent);
        for (i, line) in lines.iter().enumerate() {
            let baseline = top + i as f32 * line_h + size * 0.8;
            text(px, &theme.font, line, size, c, baseline, theme.foreground);
        }
    }
}

/// The largest size at which the message wraps into at most `MAX_LINES`,
/// and its lines; the smallest size cuts it off with an ellipsis.
fn fit(font: &FontVec, message: &str) -> (f32, Vec<String>) {
    for size in SIZES {
        let lines = wrap(font, message, size);
        if lines.len() <= MAX_LINES {
            return (size, lines);
        }
    }
    let size = SIZES[SIZES.len() - 1];
    let mut lines = wrap(font, message, size);
    lines.truncate(MAX_LINES);
    if let Some(last) = lines.last_mut() {
        last.push('…');
    }
    (size, lines)
}

/// Splits `message` into lines no wider than `WIDTH`, breaking between
/// words (and inside a word too long for a line of its own).
fn wrap(font: &FontVec, message: &str, size: f32) -> Vec<String> {
    let fits = |s: &str| text_width(font, s, size) <= WIDTH;
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in message.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if fits(&candidate) {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        for ch in word.chars() {
            line.push(ch);
            if !fits(&line) && line.chars().count() > 1 {
                line.pop();
                lines.push(std::mem::take(&mut line));
                line.push(ch);
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
