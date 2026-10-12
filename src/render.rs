//! Turns a screen into a ready-to-send frame (64-byte header + JPEG).
//!
//! The main loop only calls `draw` when the screen, its content or the
//! theme changed, so most ticks just resend the last frame. The rotation
//! bar goes on top of the screen's picture, so moving it doesn't take a
//! redraw, and hardly any encoding either: every frame is encoded with a
//! restart marker after each row of JPEG blocks, which makes each row's
//! data stand on its own, so `move_bar` re-encodes only the bottom row (the
//! bar's) and splices it in place of the old one.

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
        }
    }

    /// Draws `screen`, to be encoded by the next `encode`.
    pub fn draw(&mut self, screen: &dyn Screen, theme: &Theme) {
        screen.draw(&mut self.scene, theme);
        self.quality = screen.jpeg_quality();
    }

    /// Encodes the whole of the last drawing, with a rotation bar `bar`
    /// pixels long along the bottom edge when there is one.
    pub fn encode(&mut self, theme: &Theme, bar: Option<u32>) -> Result<()> {
        let picture = match bar {
            Some(width) => {
                self.canvas.data_mut().copy_from_slice(self.scene.data());
                draw_bar(&mut self.canvas, theme, width, 0.0);
                &self.canvas
            }
            None => &self.scene,
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
