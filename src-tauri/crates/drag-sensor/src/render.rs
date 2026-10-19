//! Software rendering of the native wheel and the result toast into
//! tiny-skia pixmaps. The design is the product wheel's, number for
//! number (see `src/components/Wheel.tsx` + `src/styles.css`):
//! lapis disc, alternating gold/bronze segments, Shamash star hub.
//! Platform modules blit the pixmap their own way (X11 `PutImage`,
//! Windows `UpdateLayeredWindow`, macOS `NSImage`).

use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

use crate::arabic::{display_order, ShapedGlyph};
use crate::geom;
use crate::text::fonts;
use crate::{ToastKind, WheelModel};

// Palette (styles.css).
const LAPIS_950: (u8, u8, u8) = (7, 22, 48);
const LAPIS_900: (u8, u8, u8) = (11, 30, 58);
const GOLD: (u8, u8, u8) = (201, 162, 39);
const GOLD_BRIGHT: (u8, u8, u8) = (231, 198, 90);
const SEG_A: (u8, u8, u8) = (217, 184, 60);
const SEG_B: (u8, u8, u8) = (189, 148, 50);
const SEG_INK: (u8, u8, u8) = (34, 26, 4);
const CREAM: (u8, u8, u8) = (247, 240, 220);
const CREAM_DIM: (u8, u8, u8) = (217, 210, 186);
const DANGER: (u8, u8, u8) = (224, 104, 79);

fn col(c: (u8, u8, u8), a: u8) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, a)
}

fn paint(c: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(col(c, a));
    p.anti_alias = true;
    p
}

fn circle_path(cx: f32, cy: f32, r: f32) -> Path {
    let mut b = PathBuilder::new();
    let k = 0.552_284_8 * r;
    b.move_to(cx + r, cy);
    b.cubic_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
    b.cubic_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy);
    b.cubic_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r);
    b.cubic_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
    b.close();
    b.finish().expect("circle path")
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let mut b = PathBuilder::new();
    b.move_to(x + r, y);
    b.line_to(x + w - r, y);
    b.quad_to(x + w, y, x + w, y + r);
    b.line_to(x + w, y + h - r);
    b.quad_to(x + w, y + h, x + w - r, y + h);
    b.line_to(x + r, y + h);
    b.quad_to(x, y + h, x, y + h - r);
    b.line_to(x, y + r);
    b.quad_to(x, y, x + r, y);
    b.close();
    b.finish().expect("rounded rect path")
}

fn sector_path(n: usize, i: usize, grow: f32) -> Path {
    let (a0, a1) = geom::sector_angles(n, i);
    let mid = (a0 + a1) / 2.0;
    let (ox, oy) = if grow > 0.0 {
        geom::polar(grow, mid)
    } else {
        (0.0, 0.0)
    };
    let mut b = PathBuilder::new();
    let steps = ((a1 - a0) / 1.5).ceil() as usize;
    for s in 0..=steps {
        let a = a0 + (a1 - a0) * (s as f32 / steps.max(1) as f32);
        let (x, y) = geom::polar_abs(geom::R_OUT, a);
        if s == 0 {
            b.move_to(x + ox, y + oy);
        } else {
            b.line_to(x + ox, y + oy);
        }
    }
    for s in (0..=steps).rev() {
        let a = a0 + (a1 - a0) * (s as f32 / steps.max(1) as f32);
        let (x, y) = geom::polar_abs(geom::R_IN, a);
        b.line_to(x + ox, y + oy);
    }
    b.close();
    b.finish().expect("sector path")
}

fn star_path(cx: f32, cy: f32, r_outer: f32, r_inner: f32) -> Path {
    let pts = geom::star_points(cx, cy, r_outer, r_inner);
    let mut b = PathBuilder::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        if i == 0 {
            b.move_to(*x, *y);
        } else {
            b.line_to(*x, *y);
        }
    }
    b.close();
    b.finish().expect("star path")
}

fn plain_glyphs(s: &str) -> Vec<ShapedGlyph> {
    s.chars()
        .map(|ch| ShapedGlyph {
            ch,
            zero_advance: false,
        })
        .collect()
}

fn draw_centered(
    pm: &mut Pixmap,
    glyphs: &[ShapedGlyph],
    cx: f32,
    baseline: f32,
    size: f32,
    bold: bool,
    color: (u8, u8, u8),
) {
    let w = fonts().measure(glyphs, size, bold);
    fonts().draw(
        pm,
        glyphs,
        cx - w / 2.0,
        baseline,
        size,
        bold,
        col(color, 255),
    );
}

/// Render the wheel. `hover` is the segment currently under the pointer
/// (brightened + grown outward, as in the webview wheel).
pub fn render_wheel(model: &WheelModel, hover: Option<usize>) -> Pixmap {
    let size = geom::WHEEL_SIZE as u32;
    let mut pm = Pixmap::new(size, size).expect("wheel pixmap");
    let c = geom::CENTER;

    // Soft backdrop so the disc reads as floating even without a
    // compositor (alpha is honoured where the platform supports it).
    let backdrop = rounded_rect_path(
        2.0,
        2.0,
        geom::WHEEL_SIZE - 4.0,
        geom::WHEEL_SIZE - 4.0,
        34.0,
    );
    pm.fill_path(
        &backdrop,
        &paint(LAPIS_950, 168),
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // Halo.
    let halo = circle_path(c, c, geom::HALO_R);
    pm.fill_path(
        &halo,
        &paint(LAPIS_900, 235),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut stroke = Stroke::default();
    stroke.width = 1.5;
    pm.stroke_path(
        &halo,
        &paint(GOLD, 128),
        &stroke,
        Transform::identity(),
        None,
    );

    // Segments + labels. Labels are host-composed (extensions for the
    // conversion wheel, localized names for tools/actions); long labels
    // shrink to fit their sector.
    let n = model.segments.len();
    for (i, segment) in model.segments.iter().enumerate() {
        let is_hover = hover == Some(i);
        let path = sector_path(n, i, if is_hover { 10.0 } else { 0.0 });
        let fill = if is_hover {
            GOLD_BRIGHT
        } else if i % 2 == 0 {
            SEG_A
        } else {
            SEG_B
        };
        pm.fill_path(
            &path,
            &paint(fill, 255),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        let (lx, ly) = geom::label_pos(n, i);
        let (hx, hy) = if is_hover {
            geom::polar(10.0, {
                let (a0, a1) = geom::sector_angles(n, i);
                (a0 + a1) / 2.0
            })
        } else {
            (0.0, 0.0)
        };
        let label = &segment.label;
        let label_chars = label.chars().count();
        let size = if label_chars > 9 {
            12.5
        } else if label_chars > 6 {
            14.0
        } else {
            16.5
        };
        draw_centered(
            &mut pm,
            &display_order(label),
            lx + hx,
            ly + hy + size * 0.35,
            size,
            true,
            SEG_INK,
        );
    }

    // Hub.
    let hub = circle_path(c, c, geom::R_IN - 6.0);
    pm.fill_path(
        &hub,
        &paint(LAPIS_900, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut stroke = Stroke::default();
    stroke.width = 2.0;
    pm.stroke_path(
        &hub,
        &paint(GOLD, 255),
        &stroke,
        Transform::identity(),
        None,
    );
    let star = star_path(c, c - 14.0, 24.0, 10.0);
    pm.fill_path(
        &star,
        &paint(GOLD, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let core = circle_path(c, c - 14.0, 6.5);
    pm.fill_path(
        &core,
        &paint(LAPIS_900, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // Hub text: source extension + file name.
    draw_centered(
        &mut pm,
        &plain_glyphs(&model.hub_ext.to_uppercase()),
        c,
        c + 22.0,
        19.0,
        true,
        GOLD_BRIGHT,
    );
    let name = truncate_chars(&model.hub_title, 23);
    draw_centered(
        &mut pm,
        &display_order(&name),
        c,
        c + 44.0,
        11.5,
        false,
        CREAM_DIM,
    );
    pm
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

pub const TOAST_H: u32 = 92;

/// Render a result toast. Width adapts to the text (300..=640 px).
/// `rtl` mirrors the layout for Arabic.
pub fn render_toast(kind: ToastKind, title: &str, body: &str, rtl: bool) -> Pixmap {
    let title_g = display_order(title);
    let body_g = display_order(body);
    let tw = fonts().measure(&title_g, 16.0, true);
    let bw = fonts().measure(&body_g, 13.0, false);
    let text_w = tw.max(bw);
    let w = ((text_w + 46.0 + 36.0) as u32).clamp(300, 640);
    let mut pm = Pixmap::new(w, TOAST_H).expect("toast pixmap");
    let wf = w as f32;
    let hf = TOAST_H as f32;

    let bg = rounded_rect_path(1.0, 1.0, wf - 2.0, hf - 2.0, 16.0);
    pm.fill_path(
        &bg,
        &paint(LAPIS_900, 245),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut stroke = Stroke::default();
    stroke.width = 1.5;
    let accent = match kind {
        ToastKind::Success => GOLD,
        ToastKind::Error => DANGER,
    };
    pm.stroke_path(
        &bg,
        &paint(accent, 255),
        &stroke,
        Transform::identity(),
        None,
    );

    // Star mark.
    let star_cx = if rtl { wf - 34.0 } else { 34.0 };
    let star = star_path(star_cx, hf / 2.0, 15.0, 6.0);
    pm.fill_path(
        &star,
        &paint(GOLD, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let core = circle_path(star_cx, hf / 2.0, 4.0);
    pm.fill_path(
        &core,
        &paint(LAPIS_900, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // Text block.
    if rtl {
        let x = wf - 60.0;
        let tww = fonts().measure(&title_g, 16.0, true);
        fonts().draw(
            &mut pm,
            &title_g,
            x - tww,
            38.0,
            16.0,
            true,
            col(CREAM, 255),
        );
        let bww = fonts().measure(&body_g, 13.0, false);
        fonts().draw(
            &mut pm,
            &body_g,
            x - bww,
            64.0,
            13.0,
            false,
            col(CREAM_DIM, 255),
        );
    } else {
        fonts().draw(&mut pm, &title_g, 60.0, 38.0, 16.0, true, col(CREAM, 255));
        fonts().draw(
            &mut pm,
            &body_g,
            60.0,
            64.0,
            13.0,
            false,
            col(CREAM_DIM, 255),
        );
    }
    pm
}

/// Convert a tiny-skia (premultiplied RGBA) pixmap to the BGRA byte
/// order X11 `PutImage` and Win32 `UpdateLayeredWindow` both expect.
pub fn pixmap_to_bgra(pm: &Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pm.data().len());
    for px in pm.pixels() {
        out.push(px.blue());
        out.push(px.green());
        out.push(px.red());
        out.push(px.alpha());
    }
    out
}
