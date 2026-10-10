//! Drawing helpers shared by all screens. Pixmaps are always fully opaque,
//! so tiny-skia's premultiplied RGBA is plain RGBA.

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont, point};
use tiny_skia::{
    FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, SpreadMode,
    Transform,
};

use crate::theme::Rgb;

pub fn fill(px: &mut Pixmap, color: Rgb) {
    px.fill(tiny_skia::Color::from_rgba8(color.0, color.1, color.2, 255));
}

pub fn rounded_rect(px: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgb) {
    fill_rounded(px, x, y, w, h, r, &paint(color));
}

fn fill_rounded(px: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, paint: &Paint) {
    if let Some(path) = rounded_path(x, y, w, h, r) {
        px.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn rounded_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
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
    pb.finish()
}

/// A horizontal meter: a pill-shaped `track` with a fill running left to
/// right for `fraction` (0..=1) of it, shaded from `from` to `to` and lit by
/// a soft glow in `to`.
#[allow(clippy::too_many_arguments)]
pub fn bar(
    px: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fraction: f32,
    from: Rgb,
    to: Rgb,
    track: Rgb,
) {
    let r = h / 2.0;
    rounded_rect(px, x, y, w, h, r, track);
    if fraction <= 0.0 {
        return;
    }
    // Never narrower than the bar is tall, so the rounded ends stay round.
    let fw = (fraction.min(1.0) * w).max(h);
    for (grow, alpha) in [(7.0, 18), (4.0, 34), (2.0, 60)] {
        let mut glow = paint(to);
        glow.set_color_rgba8(to.0, to.1, to.2, alpha);
        fill_rounded(
            px,
            x - grow,
            y - grow,
            fw + 2.0 * grow,
            h + 2.0 * grow,
            r + grow,
            &glow,
        );
    }
    let color = |c: Rgb| tiny_skia::Color::from_rgba8(c.0, c.1, c.2, 255);
    let mut fill = paint(to);
    if let Some(shader) = LinearGradient::new(
        Point::from_xy(x, y),
        Point::from_xy(x + fw, y),
        vec![
            GradientStop::new(0.0, color(from)),
            GradientStop::new(1.0, color(to)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        fill.shader = shader;
    }
    fill_rounded(px, x, y, fw, h, r, &fill);
    // A faint highlight along the top edge gives the fill some depth.
    let mut shine = paint(Rgb(255, 255, 255));
    shine.set_color_rgba8(255, 255, 255, 38);
    fill_rounded(px, x + r / 2.0, y + 2.0, fw - r, h * 0.35, h * 0.2, &shine);
}

/// `a` blended toward `b` by `t` (0..=1).
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Center,
    Right,
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
    text_aligned(px, font, s, size, cx, baseline, color, Align::Center);
}

/// Draws `s` with its baseline at `baseline`, starting at, centered on or
/// ending at `x` depending on `align`.
#[allow(clippy::too_many_arguments)]
pub fn text_aligned(
    px: &mut Pixmap,
    font: &FontVec,
    s: &str,
    size: f32,
    x: f32,
    baseline: f32,
    color: Rgb,
    align: Align,
) {
    let (glyphs, pen) = layout(font, s, size);
    let left = match align {
        Align::Left => x,
        Align::Center => x - pen / 2.0,
        Align::Right => x - pen,
    };
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

/// How wide `s` is when drawn at `size`.
pub fn text_width(font: &FontVec, s: &str, size: f32) -> f32 {
    layout(font, s, size).1
}

/// Each glyph's pen position relative to the start of the line, and the
/// line's total advance.
fn layout(font: &FontVec, s: &str, size: f32) -> (Vec<(GlyphId, f32)>, f32) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut glyphs = Vec::with_capacity(s.len());
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
    (glyphs, pen)
}

fn paint(color: Rgb) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, 255);
    paint.anti_alias = true;
    paint
}
