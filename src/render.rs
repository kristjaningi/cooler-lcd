//! Draws the dashboard and encodes it as a ready-to-send frame
//! (64-byte header + JPEG).
//!
//! The background and cards are drawn once per theme. The text is redrawn and
//! the JPEG re-encoded only when something visible changes (the minute, a
//! rounded temperature, the theme), so most ticks just resend the last frame.

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont, point};
use anyhow::Result;
use chrono::{DateTime, Local};
use jpeg_encoder::{ColorType, Encoder};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

use crate::device::{HEADER_LEN, write_header};
use crate::theme::{Rgb, Theme};

pub const SIZE: u32 = 480;
const JPEG_QUALITY: u8 = 90; // >= 90 keeps full-resolution color, so text edges stay clean

const CARD_Y: f32 = 284.0;
const CARD_W: f32 = 208.0;
const CARD_H: f32 = 172.0;
const CARD_XS: [f32; 2] = [24.0, 248.0];

pub struct Snapshot {
    pub now: DateTime<Local>,
    pub cpu_temp: Option<f32>,
    pub gpu_temp: Option<f32>,
}

/// Everything visible on screen; a new frame is encoded only when this changes.
#[derive(PartialEq)]
struct Content {
    time: String,
    date: String,
    temps: [Option<i32>; 2],
    theme_generation: u64,
}

pub struct Renderer {
    base: Pixmap,
    canvas: Pixmap,
    frame: Vec<u8>,
    shown: Option<Content>,
    theme_generation: u64,
}

impl Renderer {
    pub fn new(theme: &Theme) -> Self {
        let mut r = Self {
            base: Pixmap::new(SIZE, SIZE).unwrap(),
            canvas: Pixmap::new(SIZE, SIZE).unwrap(),
            frame: Vec::with_capacity(128 * 1024),
            shown: None,
            theme_generation: 0,
        };
        r.set_theme(theme);
        r
    }

    /// Redraws the static layer; the next `update` re-encodes.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme_generation += 1;
        let Rgb(r, g, b) = theme.background;
        self.base.fill(tiny_skia::Color::from_rgba8(r, g, b, 255));
        for (x, label) in CARD_XS.into_iter().zip(["CPU", "GPU"]) {
            rounded_rect(
                &mut self.base,
                x,
                CARD_Y,
                CARD_W,
                CARD_H,
                20.0,
                theme.surface,
            );
            text(
                &mut self.base,
                &theme.font,
                label,
                28.0,
                x + CARD_W / 2.0,
                CARD_Y + 46.0,
                theme.muted,
            );
        }
    }

    /// Re-encodes the frame if the visible content changed.
    pub fn update(&mut self, theme: &Theme, snap: &Snapshot) -> Result<()> {
        let content = Content {
            time: snap.now.format("%H:%M").to_string(),
            date: snap.now.format("%a %d %b").to_string(),
            temps: [snap.cpu_temp, snap.gpu_temp].map(|t| t.map(|t| t.round() as i32)),
            theme_generation: self.theme_generation,
        };
        if self.shown.as_ref() == Some(&content) {
            return Ok(());
        }

        self.canvas.data_mut().copy_from_slice(self.base.data());
        let center = SIZE as f32 / 2.0;
        text(
            &mut self.canvas,
            &theme.font,
            &content.time,
            130.0,
            center,
            180.0,
            theme.foreground,
        );
        text(
            &mut self.canvas,
            &theme.font,
            &content.date,
            36.0,
            center,
            240.0,
            theme.accent,
        );
        for (x, temp) in CARD_XS.into_iter().zip(content.temps) {
            let (value, color) = match temp {
                Some(t) => (format!("{t}°"), temp_color(theme, t)),
                None => ("--".into(), theme.muted),
            };
            text(
                &mut self.canvas,
                &theme.font,
                &value,
                84.0,
                x + CARD_W / 2.0,
                CARD_Y + 140.0,
                color,
            );
        }

        // Every pixel is opaque, so tiny-skia's premultiplied RGBA is plain RGBA.
        self.frame.clear();
        self.frame.resize(HEADER_LEN, 0);
        Encoder::new(&mut self.frame, JPEG_QUALITY).encode(
            self.canvas.data(),
            SIZE as u16,
            SIZE as u16,
            ColorType::Rgba,
        )?;
        write_header(&mut self.frame, SIZE, SIZE);
        self.shown = Some(content);
        Ok(())
    }

    /// The last encoded frame (header + JPEG); empty before the first update.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }
}

fn temp_color(theme: &Theme, t: i32) -> Rgb {
    match t {
        80.. => theme.red,
        65.. => theme.yellow,
        _ => theme.green,
    }
}

fn rounded_rect(px: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgb) {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, 255);
    paint.anti_alias = true;
    px.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// Draws `s` horizontally centered on `cx` with its baseline at `baseline`,
/// blending onto the (opaque) pixmap.
fn text(px: &mut Pixmap, font: &FontVec, s: &str, size: f32, cx: f32, baseline: f32, color: Rgb) {
    let scaled = font.as_scaled(PxScale::from(size));

    // Pen position of each glyph, relative to the start of the line.
    let mut glyphs: Vec<(GlyphId, f32)> = Vec::with_capacity(s.len());
    let mut pen = 0.0;
    let mut prev: Option<GlyphId> = None;
    for c in s.chars() {
        let id = font.glyph_id(c);
        if let Some(p) = prev {
            pen += scaled.kern(p, id);
        }
        glyphs.push((id, pen));
        pen += scaled.h_advance(id);
        prev = Some(id);
    }

    let left = cx - pen / 2.0;
    let (w, h) = (px.width() as i32, px.height() as i32);
    let data = px.data_mut();
    for (id, offset) in glyphs {
        let glyph = id.with_scale_and_position(size, point(left + offset, baseline));
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let (x, y) = (
                bounds.min.x as i32 + gx as i32,
                bounds.min.y as i32 + gy as i32,
            );
            if x < 0 || y < 0 || x >= w || y >= h {
                return;
            }
            let i = ((y * w + x) * 4) as usize;
            let a = coverage.clamp(0.0, 1.0);
            for (c, target) in [color.0, color.1, color.2].into_iter().enumerate() {
                let dst = data[i + c] as f32;
                data[i + c] = (dst + (target as f32 - dst) * a).round() as u8;
            }
        });
    }
}
