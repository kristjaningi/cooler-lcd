//! Drawing helpers shared by all screens. Pixmaps are always fully opaque,
//! so tiny-skia's premultiplied RGBA is plain RGBA.

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont, point};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

use crate::theme::Rgb;

pub fn fill(px: &mut Pixmap, color: Rgb) {
    px.fill(tiny_skia::Color::from_rgba8(color.0, color.1, color.2, 255));
}

pub fn rounded_rect(px: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgb) {
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

/// Draws `s` horizontally centered on `cx` with its baseline at `baseline`.
pub fn text(
    px: &mut Pixmap,
    font: &FontVec,
    s: &str,
    size: f32,
    cx: f32,
    baseline: f32,
    color: Rgb,
) {
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
