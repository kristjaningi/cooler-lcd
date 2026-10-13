//! Turns a screen into a ready-to-send frame (64-byte header + JPEG).
//!
//! The main loop only calls `draw` when the screen, its content or the
//! theme changed, so most ticks just resend the last frame. The rotation
//! bar goes on top of the screen's picture, so moving it doesn't take a
//! redraw, and hardly any encoding either: every frame is encoded with a
//! restart marker after each row of JPEG blocks, which makes each row's
//! data stand on its own, so `move_bar` re-encodes only the bottom row (the
//! bar's) and splices it in place of the old one.
//!
//! At night the picture goes out through a tone curve that darkens every
//! color, the darker ones most (see `night_curve`). The screen's own
//! picture is left as drawn, so the bar can still be moved on top of it.

use anyhow::{Result, ensure};
use jpeg_encoder::{ColorType, Encoder};
use tiny_skia::Pixmap;

use crate::device::{HEADER_LEN, write_header};
use crate::draw::{mix, rounded_rect};
use crate::screens::Screen;
use crate::theme::Theme;

pub const SIZE: u32 = 480;
/// The rotation bar along the bottom edge. Screens keep their content
/// above `SIZE - BAR_H`. It has to fit in the bottom row of JPEG blocks,
/// which is 8 pixels tall at the least.
const BAR_H: f32 = 6.0;
const _: () = assert!(BAR_H <= 8.0);
/// How much faster than the brightest colors the darker ones fade at
/// night: a theme's background sinks nearly to black, where most of the
/// glow comes from, while text keeps its contrast with it.
const NIGHT_GAMMA: f32 = 1.2;

pub struct Renderer {
    /// The screen's picture as it last drew it.
    scene: Pixmap,
    /// The scene with the rotation bar on top, for a full encode.
    canvas: Pixmap,
    /// The scene's bottom row of blocks with the bar on top, for a splice.
    strip: Pixmap,
    strip_jpeg: Vec<u8>,
    quality: u8,
    frame: Vec<u8>,
    /// Where the bottom row's data starts in `frame`, just past the last
    /// restart marker; none until a full encode with this layout.
    tail_at: Option<usize>,
    /// At night, the brightness it's dimmed to and the curve doing it.
    night: Option<(f32, [u8; 256])>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            scene: Pixmap::new(SIZE, SIZE).unwrap(),
            canvas: Pixmap::new(SIZE, SIZE).unwrap(),
            strip: Pixmap::new(SIZE, 16).unwrap(),
            strip_jpeg: Vec::with_capacity(8 * 1024),
            quality: 90,
            frame: Vec::with_capacity(128 * 1024),
            tail_at: None,
            night: None,
        }
    }

    /// Dims frames from now on to `brightness` (see `night_curve`), or
    /// stops dimming them. Returns true on a change, which the frame on
    /// the panel doesn't show until the next `encode`.
    pub fn set_night(&mut self, brightness: Option<f32>) -> bool {
        if self.night.map(|(b, _)| b) == brightness {
            return false;
        }
        self.night = brightness.map(|b| (b, night_curve(b)));
        // A strip spliced in now would be dimmed differently from the
        // rest of the frame.
        self.tail_at = None;
        true
    }

    /// Draws `screen`, to be encoded by the next `encode`.
    pub fn draw(&mut self, screen: &dyn Screen, theme: &Theme) {
        screen.draw(&mut self.scene, theme);
        self.quality = screen.jpeg_quality();
    }

    /// Encodes the whole of the last drawing, with a rotation bar `bar`
    /// pixels long along the bottom edge when there is one.
    pub fn encode(&mut self, theme: &Theme, bar: Option<u32>) -> Result<()> {
        let picture = if bar.is_some() || self.night.is_some() {
            self.canvas.data_mut().copy_from_slice(self.scene.data());
            if let Some(width) = bar {
                draw_bar(&mut self.canvas, theme, width, 0.0);
            }
            if let Some((_, curve)) = &self.night {
                dim(&mut self.canvas, curve);
            }
            &self.canvas
        } else {
            &self.scene
        };
        self.tail_at = None;
        self.frame.clear();
        self.frame.resize(HEADER_LEN, 0);
        let (row_w, _) = block_row(self.quality);
        let mut encoder = Encoder::new(&mut self.frame, self.quality);
        encoder.set_restart_interval((SIZE / row_w) as u16);
        encoder.encode(picture.data(), SIZE as u16, SIZE as u16, ColorType::Rgba)?;
        write_header(&mut self.frame, SIZE, SIZE);
        self.tail_at = last_restart(&self.frame[HEADER_LEN..]).map(|at| HEADER_LEN + at);
        Ok(())
    }

    /// Moves the rotation bar on the last encoded frame, re-encoding only
    /// the bottom row of blocks; a full `encode` if there's nothing to
    /// splice into.
    pub fn move_bar(&mut self, theme: &Theme, bar: Option<u32>) -> Result<()> {
        let Some(tail_at) = self.tail_at else {
            return self.encode(theme, bar);
        };
        let (_, row_h) = block_row(self.quality);
        if self.strip.height() != row_h {
            self.strip = Pixmap::new(SIZE, row_h).unwrap();
        }
        // The scene's bottom rows, then the bar over them.
        let row_bytes = (SIZE * 4) as usize;
        let top = (SIZE - row_h) as usize * row_bytes;
        self.strip
            .data_mut()
            .copy_from_slice(&self.scene.data()[top..]);
        if let Some(width) = bar {
            draw_bar(&mut self.strip, theme, width, (SIZE - row_h) as f32);
        }
        if let Some((_, curve)) = &self.night {
            dim(&mut self.strip, curve);
        }
        self.strip_jpeg.clear();
        Encoder::new(&mut self.strip_jpeg, self.quality).encode(
            self.strip.data(),
            SIZE as u16,
            row_h as u16,
            ColorType::Rgba,
        )?;
        let scan = scan_data(&self.strip_jpeg);
        ensure!(scan.is_some(), "no scan data in the bar's strip");
        self.frame.truncate(tail_at);
        self.frame.extend_from_slice(scan.unwrap());
        self.frame.extend_from_slice(&[0xFF, 0xD9]);
        write_header(&mut self.frame, SIZE, SIZE);
        Ok(())
    }

    /// Draws and encodes `screen` without a rotation bar.
    pub fn render(&mut self, screen: &dyn Screen, theme: &Theme) -> Result<()> {
        self.draw(screen, theme);
        self.encode(theme, None)
    }

    /// The last encoded frame (header + JPEG); empty before the first render.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }
}

/// The tone curve for night: each channel scaled to `brightness` at its
/// brightest, and darker values down by more, along a `NIGHT_GAMMA` curve.
fn night_curve(brightness: f32) -> [u8; 256] {
    std::array::from_fn(|i| {
        (255.0 * brightness * (i as f32 / 255.0).powf(NIGHT_GAMMA)).round() as u8
    })
}

/// Passes the color of every pixel in `px` through `curve`; the alpha,
/// always opaque, stays as it is.
fn dim(px: &mut Pixmap, curve: &[u8; 256]) {
    for [r, g, b, _] in px.data_mut().as_chunks_mut::<4>().0 {
        for c in [r, g, b] {
            *c = curve[*c as usize];
        }
    }
}

/// The size of a row of JPEG blocks (MCUs) at `quality`: the encoder
/// halves the color resolution below 90, doubling the blocks.
fn block_row(quality: u8) -> (u32, u32) {
    if quality < 90 { (16, 16) } else { (8, 8) }
}

/// Where the data after the last restart marker starts in `jpeg`. In the
/// scan, a 0xFF byte of data is always followed by 0x00, so 0xFF then
/// 0xD0..=0xD7 is a marker.
fn last_restart(jpeg: &[u8]) -> Option<usize> {
    jpeg.windows(2)
        .rposition(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
        .map(|i| i + 2)
}

/// The entropy-coded data of a baseline JPEG: from after the start-of-scan
/// segment to before the end-of-image marker.
fn scan_data(jpeg: &[u8]) -> Option<&[u8]> {
    let mut i = 2; // past SOI
    loop {
        let (marker, len) = (jpeg.get(i + 1)?, jpeg.get(i + 2..i + 4)?);
        let len = u16::from_be_bytes([len[0], len[1]]) as usize;
        if *marker == 0xDA {
            return jpeg.get(i + 2 + len..jpeg.len().checked_sub(2)?);
        }
        i += 2 + len;
    }
}

/// A faint track across the bottom edge, filled `width` pixels in from the
/// left as the next screen draws near. `top` is the picture's offset from
/// the top of the screen.
fn draw_bar(px: &mut Pixmap, theme: &Theme, width: u32, top: f32) {
    let y = SIZE as f32 - BAR_H - top;
    let track = mix(theme.background, theme.surface, 0.8);
    rounded_rect(px, 0.0, y, SIZE as f32, BAR_H, 0.0, track);
    if width > 0 {
        let w = width.min(SIZE) as f32;
        rounded_rect(px, 0.0, y, w, BAR_H, BAR_H / 2.0, theme.accent);
        // Square off the left end, which runs into the screen's edge.
        rounded_rect(px, 0.0, y, w.min(BAR_H), BAR_H, 0.0, theme.accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Rgb;

    fn theme() -> Theme {
        Theme::fallback()
    }

    /// A busy picture, so every block has something to encode.
    fn scene() -> Pixmap {
        let mut px = Pixmap::new(SIZE, SIZE).unwrap();
        for (i, b) in px.data_mut().iter_mut().enumerate() {
            *b = if i % 4 == 3 { 255 } else { (i * 7 % 251) as u8 };
        }
        px
    }

    #[test]
    fn moving_the_bar_matches_a_full_encode() {
        let theme = theme();
        for quality in [90, 75] {
            let mut r = Renderer::new();
            r.scene = scene();
            r.quality = quality;
            r.encode(&theme, Some(10)).unwrap();
            for bar in [Some(200), Some(480), None] {
                r.move_bar(&theme, bar).unwrap();
                let spliced = r.frame.clone();
                r.encode(&theme, bar).unwrap();
                assert!(spliced == r.frame, "quality {quality}, bar {bar:?}");
            }
        }
    }

    #[test]
    fn moving_the_bar_at_night_matches_a_full_encode() {
        let theme = theme();
        for quality in [90, 75] {
            let mut r = Renderer::new();
            r.scene = scene();
            r.quality = quality;
            r.encode(&theme, Some(10)).unwrap();
            assert!(r.set_night(Some(0.6)));
            assert!(!r.set_night(Some(0.6)), "no change");
            for bar in [Some(200), None, Some(480)] {
                r.move_bar(&theme, bar).unwrap();
                let spliced = r.frame.clone();
                r.encode(&theme, bar).unwrap();
                assert!(spliced == r.frame, "quality {quality}, bar {bar:?}");
            }
            // Back to day, the first splice redoes the whole frame.
            r.set_night(None);
            r.move_bar(&theme, Some(100)).unwrap();
            let spliced = r.frame.clone();
            r.encode(&theme, Some(100)).unwrap();
            assert!(spliced == r.frame, "quality {quality}, by day again");
        }
    }

    #[test]
    fn the_night_curve_darkens_the_dark_most() {
        let curve = night_curve(0.6);
        assert_eq!((curve[0], curve[255]), (0, 153));
        assert!(curve.windows(2).all(|w| w[0] <= w[1]), "never reverses");
        // The background loses more of its light than the text does.
        let kept = |c: usize| curve[c] as f32 / c as f32;
        assert!(kept(0x1a) < kept(0xc0));

        let mut px = Pixmap::new(2, 1).unwrap();
        px.data_mut()
            .copy_from_slice(&[255, 0x1a, 0, 255, 0xc0, 0xc0, 0xc0, 255]);
        dim(&mut px, &curve);
        let night = [
            curve[255],
            curve[0x1a],
            0,
            255,
            curve[0xc0],
            curve[0xc0],
            curve[0xc0],
            255,
        ];
        assert_eq!(px.data(), night, "alpha untouched");
    }

    /// Relative luminance of an sRGB color, 0..=1.
    fn luminance(c: Rgb) -> f32 {
        let lin = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
    }

    #[test]
    fn text_stays_as_readable_at_night() {
        // Background, muted and foreground of a few Omarchy themes.
        let themes = [
            ("tokyo-night", 0x1a1b26, 0x565f89, 0xc0caf5),
            ("catppuccin", 0x1e1e2e, 0x6c7086, 0xcdd6f4),
            ("nord", 0x2e3440, 0x667080, 0xd8dee9),
            ("gruvbox", 0x282828, 0x7c6f64, 0xd4be98),
            ("everforest", 0x2d353b, 0x4f585e, 0xd3c6aa),
            ("matte-black", 0x121212, 0x555555, 0xbebebe),
        ];
        let rgb = |c: u32| Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8);
        let curve = night_curve(crate::config::Night::default().brightness);
        let dimmed = |c: Rgb| {
            Rgb(
                curve[c.0 as usize],
                curve[c.1 as usize],
                curve[c.2 as usize],
            )
        };
        // Contrast as WCAG has it, with `flare` for the light in the room:
        // its 5% by day, and at night in the dark only the panel's own
        // black, about a tenth of that.
        let contrast = |a: Rgb, b: Rgb, flare: f32| (luminance(a) + flare) / (luminance(b) + flare);
        for (name, bg, muted, fg) in themes {
            let (bg, muted, fg) = (rgb(bg), rgb(muted), rgb(fg));
            for (what, c) in [("muted", muted), ("foreground", fg)] {
                let day = contrast(c, bg, 0.05);
                let night = contrast(dimmed(c), dimmed(bg), 0.005);
                assert!(
                    night >= day,
                    "{name} {what}: {night:.2} at night, {day:.2} by day"
                );
            }
        }
    }

    #[test]
    fn finds_the_scan() {
        let mut jpeg = Vec::new();
        let px = Pixmap::new(16, 8).unwrap();
        Encoder::new(&mut jpeg, 90)
            .encode(px.data(), 16, 8, ColorType::Rgba)
            .unwrap();
        let scan = scan_data(&jpeg).unwrap();
        assert!(!scan.is_empty());
        assert_eq!(&jpeg[jpeg.len() - 2..], [0xFF, 0xD9]);
        assert_eq!(last_restart(&jpeg), None, "no restart markers asked for");
    }
}
