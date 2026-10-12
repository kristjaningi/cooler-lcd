//! A dim clock shown while nobody is at the desktop, in place of the
//! carousel: no usage numbers on display and no radar traffic fetched.

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{fill, mix, text};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

const BLACK: Rgb = Rgb(0, 0, 0);

pub struct Away {
    time: Option<String>,
}

impl Away {
    pub fn new() -> Self {
        Self { time: None }
    }
}

impl Screen for Away {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let time = Some(now.format("%H:%M").to_string());
        let changed = self.time != time;
        self.time = time;
        changed
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        fill(px, BLACK);
        if let Some(time) = &self.time {
            let c = SIZE as f32 / 2.0;
            let dim = clock_color(theme.foreground, theme.background);
            text(px, &theme.font, time, 110.0, c, c + 38.0, dim);
        }
    }
}

/// The clock's color on black: the light end of the theme (its foreground
/// on a dark theme, its background on a light one), or white if neither is
/// light, dimmed.
fn clock_color(foreground: Rgb, background: Rgb) -> Rgb {
    let luma = |c: Rgb| (0.2126 * c.0 as f32 + 0.7152 * c.1 as f32 + 0.0722 * c.2 as f32) / 255.0;
    let light = if luma(foreground) >= luma(background) {
        foreground
    } else {
        background
    };
    let light = if luma(light) < 0.5 {
        Rgb(255, 255, 255)
    } else {
        light
    };
    mix(BLACK, light, 0.3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_shows_whatever_the_theme() {
        let tokyo = clock_color(Rgb(0xc0, 0xca, 0xf5), Rgb(0x1a, 0x1b, 0x26));
        assert_eq!(tokyo, mix(BLACK, Rgb(0xc0, 0xca, 0xf5), 0.3), "as before");
        // Omarchy's White: black on white.
        assert_eq!(
            clock_color(Rgb(0, 0, 0), Rgb(255, 255, 255)),
            Rgb(77, 77, 77)
        );
        // Both ends dark.
        assert_eq!(
            clock_color(Rgb(0x10, 0x0f, 0x0f), Rgb(0x30, 0x30, 0x30)),
            Rgb(77, 77, 77)
        );
    }
}
