//! Shared HUD panel framework — the floating tool panels.
//!
//! One platform-neutral core used by every sensor:
//! - [`layout`] computes the panel geometry from a [`PanelSpec`],
//! - [`PanelState`] is the interaction state machine (fields, chips,
//!   toggles, canvas strokes, paging) fed with pointer/key events,
//! - [`render_panel`] paints the state into a tiny-skia pixmap the
//!   platform blits into its native floating window (NSPanel / layered
//!   popup / X11 override-redirect — the panel is pixel-identical
//!   everywhere, like the wheel).
//!
//! The model is the reference's Crop panel: a small floating card near
//! the cursor with the tool's controls and Apply/Cancel.

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;

use tiny_skia::{
    Color, FillRule, IntSize, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};

use crate::arabic::display_order;
use crate::text::fonts;
use crate::{CanvasMode, CanvasShape, PageShapes, PanelField, PanelOutcome, PanelSpec};

// Palette (shared with render.rs — the wheel/toast family).
const LAPIS_950: (u8, u8, u8) = (7, 22, 48);
const LAPIS_900: (u8, u8, u8) = (11, 30, 58);
const FIELD_BG: (u8, u8, u8) = (16, 34, 66);
const FIELD_LINE: (u8, u8, u8) = (52, 74, 110);
const GOLD: (u8, u8, u8) = (201, 162, 39);
const GOLD_BRIGHT: (u8, u8, u8) = (231, 198, 90);
const SEG_INK: (u8, u8, u8) = (34, 26, 4);
const CREAM: (u8, u8, u8) = (247, 240, 220);
const CREAM_DIM: (u8, u8, u8) = (217, 210, 186);
const DANGER: (u8, u8, u8) = (224, 104, 79);

pub const FORM_W: u32 = 348;
pub const CANVAS_W: u32 = 540;
const PAD: f32 = 16.0;
const TITLE_H: f32 = 42.0;

fn col(c: (u8, u8, u8), a: u8) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, a)
}

fn paint(c: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(col(c, a));
    p.anti_alias = true;
    p
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
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

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PanelLayout {
    pub width: u32,
    pub height: u32,
    /// Control rect per field (text box / chips row / toggle row / info block).
    pub field_rects: Vec<Rect>,
    /// Pill rects per field (Chips fields only; empty vec otherwise).
    pub chip_rects: Vec<Vec<Rect>>,
    /// Wrapped line count per Info field (for painting).
    pub info_lines: Vec<Vec<String>>,
    pub canvas_rect: Option<Rect>,
    pub prev_rect: Option<Rect>,
    pub next_rect: Option<Rect>,
    /// Annotate tool-row pill rects.
    pub tool_rects: Vec<Rect>,
    /// Annotate text-entry box.
    pub label_rect: Option<Rect>,
    pub cancel_rect: Rect,
    pub apply_rect: Option<Rect>,
}

fn wrap_text(text: &str, size: f32, bold: bool, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in raw.split(' ') {
            let trial = if cur.is_empty() {
                word.to_string()
            } else {
                format!("{cur} {word}")
            };
            if fonts().measure(&display_order(&trial), size, bold) <= max_w || cur.is_empty() {
                cur = trial;
            } else {
                lines.push(std::mem::take(&mut cur));
                cur = word.to_string();
            }
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Pill widths for a chip row (shared by layout + renderer).
fn chip_widths(options: &[String], size: f32) -> Vec<f32> {
    options
        .iter()
        .map(|o| fonts().measure(&display_order(o), size, true) + 22.0)
        .collect()
}

pub fn layout(spec: &PanelSpec) -> PanelLayout {
    let width = if spec.canvas.is_some() { CANVAS_W } else { FORM_W };
    let wf = width as f32;
    let inner_w = wf - PAD * 2.0;
    let mut y = TITLE_H + 2.0;
    let mut field_rects = Vec::new();
    let mut chip_rects: Vec<Vec<Rect>> = Vec::new();
    let mut info_lines: Vec<Vec<String>> = Vec::new();

    for field in &spec.fields {
        match field {
            PanelField::Text { .. } => {
                let rect = Rect { x: PAD, y: y + 17.0, w: inner_w, h: 34.0 };
                field_rects.push(rect);
                chip_rects.push(Vec::new());
                info_lines.push(Vec::new());
                y += 17.0 + 34.0 + 10.0;
            }
            PanelField::Chips { options, .. } => {
                let rect = Rect { x: PAD, y: y + 17.0, w: inner_w, h: 30.0 };
                let widths = chip_widths(options, 12.5);
                let mut cx = if spec.rtl { PAD + inner_w } else { PAD };
                let mut pills = Vec::new();
                for w in &widths {
                    if spec.rtl {
                        cx -= w;
                        pills.push(Rect { x: cx, y: rect.y, w: *w, h: 30.0 });
                        cx -= 8.0;
                    } else {
                        pills.push(Rect { x: cx, y: rect.y, w: *w, h: 30.0 });
                        cx += w + 8.0;
                    }
                }
                field_rects.push(rect);
                chip_rects.push(pills);
                info_lines.push(Vec::new());
                y += 17.0 + 30.0 + 10.0;
            }
            PanelField::Toggle { .. } => {
                let rect = Rect { x: PAD, y, w: inner_w, h: 28.0 };
                field_rects.push(rect);
                chip_rects.push(Vec::new());
                info_lines.push(Vec::new());
                y += 28.0 + 8.0;
            }
            PanelField::Info { text, .. } => {
                let lines = wrap_text(text, 12.5, false, inner_w);
                let h = lines.len() as f32 * 18.0 + 4.0;
                field_rects.push(Rect { x: PAD, y, w: inner_w, h });
                chip_rects.push(Vec::new());
                info_lines.push(lines);
                y += h + 6.0;
            }
        }
    }

    let mut canvas_rect = None;
    let mut prev_rect = None;
    let mut next_rect = None;
    let mut tool_rects = Vec::new();
    let mut label_rect = None;
    if let Some(cv) = &spec.canvas {
        let cw = inner_w;
        let ch = (cw * cv.coord_h / cv.coord_w.max(1.0)).clamp(150.0, 330.0);
        let rect = Rect { x: PAD, y, w: cw, h: ch };
        canvas_rect = Some(rect);
        y += ch + 8.0;
        if cv.pages > 1 {
            let btn = 30.0;
            let (px, nx) = if spec.rtl {
                (wf - PAD - btn, PAD)
            } else {
                (PAD, wf - PAD - btn)
            };
            prev_rect = Some(Rect { x: px, y, w: btn, h: 26.0 });
            next_rect = Some(Rect { x: nx, y, w: btn, h: 26.0 });
            y += 26.0 + 8.0;
        }
        if cv.mode == CanvasMode::Annotate {
            let labels = if cv.tool_labels.len() == 3 {
                cv.tool_labels.clone()
            } else {
                vec!["Rect".into(), "Arrow".into(), "Text".into()]
            };
            let widths = chip_widths(&labels, 12.5);
            let mut cx = if spec.rtl { PAD + inner_w } else { PAD };
            for w in &widths {
                if spec.rtl {
                    cx -= w;
                    tool_rects.push(Rect { x: cx, y, w: *w, h: 30.0 });
                    cx -= 8.0;
                } else {
                    tool_rects.push(Rect { x: cx, y, w: *w, h: 30.0 });
                    cx += w + 8.0;
                }
            }
            y += 30.0 + 10.0;
            label_rect = Some(Rect { x: PAD, y: y + 17.0, w: inner_w, h: 34.0 });
            y += 17.0 + 34.0 + 10.0;
        }
    }

    // Buttons row: Apply at the trailing edge, Cancel beside it.
    let btn_y = y + 2.0;
    let apply_w = if spec.apply_label.is_empty() {
        0.0
    } else {
        fonts().measure(&display_order(&spec.apply_label), 13.5, true) + 30.0
    };
    let cancel_w = fonts().measure(&display_order(&spec.cancel_label), 13.5, true) + 30.0;
    let (cancel_rect, apply_rect) = if spec.rtl {
        let cancel = Rect { x: PAD, y: btn_y, w: cancel_w, h: 36.0 };
        let apply = if apply_w > 0.0 {
            Some(Rect { x: PAD + cancel_w + 10.0, y: btn_y, w: apply_w, h: 36.0 })
        } else {
            None
        };
        (cancel, apply)
    } else {
        let apply = if apply_w > 0.0 {
            Some(Rect { x: wf - PAD - apply_w, y: btn_y, w: apply_w, h: 36.0 })
        } else {
            None
        };
        let cancel_x = wf - PAD - apply_w - if apply_w > 0.0 { 10.0 } else { 0.0 } - cancel_w;
        (Rect { x: cancel_x, y: btn_y, w: cancel_w, h: 36.0 }, apply)
    };
    y = btn_y + 36.0 + PAD;

    PanelLayout {
        width,
        height: y as u32,
        field_rects,
        chip_rects,
        info_lines,
        canvas_rect,
        prev_rect,
        next_rect,
        tool_rects,
        label_rect,
        cancel_rect,
        apply_rect,
    }
}

// ---------------------------------------------------------------------------
// Interaction state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Field(usize),
    LabelBox,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelEvent {
    Down { x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Up { x: f32, y: f32 },
    Key(PanelKey),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelKey {
    Char(char),
    Backspace,
    Delete,
    Enter,
    Escape,
    Tab,
    Left,
    Right,
    Home,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelAction {
    /// Nothing changed.
    None,
    /// State changed — repaint.
    Redraw,
    Apply,
    Cancel,
    /// The canvas wants this page rendered by the host.
    NeedPage(u32),
}

#[derive(Debug, Clone)]
pub struct PanelState {
    pub spec: PanelSpec,
    pub layout: PanelLayout,
    pub texts: Vec<String>,
    pub chips: Vec<usize>,
    pub toggles: Vec<bool>,
    pub focus: Option<FocusTarget>,
    pub label_text: String,
    pub page: u32,
    pub pages: u32,
    pub shapes: BTreeMap<u32, Vec<CanvasShape>>,
    pub stroke_from: Option<(f32, f32)>,
    pub stroke_now: Option<(f32, f32)>,
    /// Annotate tool: 0 rect, 1 arrow, 2 text.
    pub tool: usize,
}

impl PanelState {
    pub fn new(spec: PanelSpec) -> PanelState {
        let layout = layout(&spec);
        let mut texts = Vec::new();
        let mut chips = Vec::new();
        let mut toggles = Vec::new();
        let mut first_text = None;
        for (i, f) in spec.fields.iter().enumerate() {
            match f {
                PanelField::Text { value, .. } => {
                    texts.push(value.clone());
                    chips.push(0);
                    toggles.push(false);
                    if first_text.is_none() {
                        first_text = Some(i);
                    }
                }
                PanelField::Chips { selected, .. } => {
                    texts.push(String::new());
                    chips.push(*selected);
                    toggles.push(false);
                }
                PanelField::Toggle { on, .. } => {
                    texts.push(String::new());
                    chips.push(0);
                    toggles.push(*on);
                }
                PanelField::Info { .. } => {
                    texts.push(String::new());
                    chips.push(0);
                    toggles.push(false);
                }
            }
        }
        let (page, pages) = spec
            .canvas
            .as_ref()
            .map(|c| (c.page, c.pages))
            .unwrap_or((1, 1));
        PanelState {
            focus: first_text.map(FocusTarget::Field),
            spec,
            layout,
            texts,
            chips,
            toggles,
            label_text: String::new(),
            page,
            pages,
            shapes: BTreeMap::new(),
            stroke_from: None,
            stroke_now: None,
            tool: 0,
        }
    }

    /// Host delivered a new canvas preview (PanelPageRequest answer).
    pub fn set_canvas_page(&mut self, page: u32, pages: u32) {
        self.page = page;
        self.pages = pages;
        self.stroke_from = None;
        self.stroke_now = None;
    }

    fn canvas_pt(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let rect = self.layout.canvas_rect?;
        let cv = self.spec.canvas.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let cx = ((x - rect.x) / rect.w * cv.coord_w).clamp(0.0, cv.coord_w);
        let cy = ((y - rect.y) / rect.h * cv.coord_h).clamp(0.0, cv.coord_h);
        Some((cx, cy))
    }

    fn push_shape(&mut self, shape: CanvasShape) {
        self.shapes.entry(self.page).or_default().push(shape);
    }

    fn pop_shape(&mut self) -> bool {
        if let Some(v) = self.shapes.get_mut(&self.page) {
            if v.pop().is_some() {
                return true;
            }
        }
        false
    }

    fn focus_next(&mut self) {
        let mut targets: Vec<FocusTarget> = self
            .spec
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f, PanelField::Text { .. }))
            .map(|(i, _)| FocusTarget::Field(i))
            .collect();
        if self.layout.label_rect.is_some() {
            targets.push(FocusTarget::LabelBox);
        }
        if targets.is_empty() {
            return;
        }
        let next = match self.focus {
            Some(cur) => match targets.iter().position(|t| *t == cur) {
                Some(i) => targets[(i + 1) % targets.len()],
                None => targets[0],
            },
            None => targets[0],
        };
        self.focus = Some(next);
    }

    fn edit_focused(&mut self, f: impl Fn(&mut String)) -> bool {
        match self.focus {
            Some(FocusTarget::Field(i)) => {
                if let Some(t) = self.texts.get_mut(i) {
                    f(t);
                    return true;
                }
                false
            }
            Some(FocusTarget::LabelBox) => {
                f(&mut self.label_text);
                true
            }
            None => false,
        }
    }

    pub fn handle(&mut self, ev: PanelEvent) -> PanelAction {
        match ev {
            PanelEvent::Key(key) => self.handle_key(key),
            PanelEvent::Down { x, y } => self.handle_down(x, y),
            PanelEvent::Move { x, y } => {
                if self.stroke_from.is_some() {
                    self.stroke_now = self.canvas_pt(x, y);
                    return PanelAction::Redraw;
                }
                PanelAction::None
            }
            PanelEvent::Up { x, y } => self.handle_up(x, y),
        }
    }

    fn handle_key(&mut self, key: PanelKey) -> PanelAction {
        match key {
            PanelKey::Escape => PanelAction::Cancel,
            PanelKey::Enter => {
                if self.spec.apply_label.is_empty() {
                    PanelAction::None
                } else {
                    PanelAction::Apply
                }
            }
            PanelKey::Tab => {
                self.focus_next();
                PanelAction::Redraw
            }
            PanelKey::Backspace => {
                let had_focus_text = matches!(self.focus, Some(FocusTarget::Field(_)) | Some(FocusTarget::LabelBox));
                if had_focus_text && self.edit_focused(|t| {
                    t.pop();
                }) {
                    return PanelAction::Redraw;
                }
                // No text focus (or label box): Backspace undoes the
                // last canvas shape on this page.
                if self.pop_shape() {
                    PanelAction::Redraw
                } else {
                    PanelAction::None
                }
            }
            PanelKey::Delete => {
                if self.pop_shape() {
                    PanelAction::Redraw
                } else {
                    PanelAction::None
                }
            }
            PanelKey::Char(c) => {
                if c.is_control() {
                    return PanelAction::None;
                }
                if self.edit_focused(|t| t.push(c)) {
                    PanelAction::Redraw
                } else {
                    PanelAction::None
                }
            }
            PanelKey::Left | PanelKey::Right | PanelKey::Home | PanelKey::End => {
                PanelAction::None // caret stays at end in v1 panels
            }
        }
    }

    fn handle_down(&mut self, x: f32, y: f32) -> PanelAction {
        if let Some(r) = self.layout.apply_rect {
            if r.contains(x, y) {
                return PanelAction::Apply;
            }
        }
        if self.layout.cancel_rect.contains(x, y) {
            return PanelAction::Cancel;
        }
        // Chips.
        for (fi, pills) in self.layout.chip_rects.iter().enumerate() {
            for (ci, pill) in pills.iter().enumerate() {
                if pill.contains(x, y) {
                    self.chips[fi] = ci;
                    return PanelAction::Redraw;
                }
            }
        }
        // Toggles (whole row flips).
        for (fi, f) in self.spec.fields.iter().enumerate() {
            if matches!(f, PanelField::Toggle { .. }) && self.layout.field_rects[fi].contains(x, y) {
                self.toggles[fi] = !self.toggles[fi];
                return PanelAction::Redraw;
            }
        }
        // Text fields.
        for (fi, f) in self.spec.fields.iter().enumerate() {
            if matches!(f, PanelField::Text { .. }) && self.layout.field_rects[fi].contains(x, y) {
                self.focus = Some(FocusTarget::Field(fi));
                return PanelAction::Redraw;
            }
        }
        // Annotate tool row + label box.
        for (ti, r) in self.layout.tool_rects.iter().enumerate() {
            if r.contains(x, y) {
                self.tool = ti;
                if ti == 2 {
                    self.focus = Some(FocusTarget::LabelBox);
                }
                return PanelAction::Redraw;
            }
        }
        if let Some(r) = self.layout.label_rect {
            if r.contains(x, y) {
                self.focus = Some(FocusTarget::LabelBox);
                return PanelAction::Redraw;
            }
        }
        // Pager.
        if let Some(r) = self.layout.prev_rect {
            if r.contains(x, y) && self.page > 1 {
                return PanelAction::NeedPage(self.page - 1);
            }
        }
        if let Some(r) = self.layout.next_rect {
            if r.contains(x, y) && self.page < self.pages {
                return PanelAction::NeedPage(self.page + 1);
            }
        }
        // Canvas.
        if let Some((cx, cy)) = self.canvas_pt(x, y) {
            let mode = self.spec.canvas.as_ref().map(|c| c.mode);
            if mode == Some(CanvasMode::Annotate) && self.tool == 2 {
                if !self.label_text.trim().is_empty() {
                    let text = self.label_text.clone();
                    self.push_shape(CanvasShape::Text { x: cx, y: cy, text });
                    return PanelAction::Redraw;
                }
                return PanelAction::None;
            }
            self.stroke_from = Some((cx, cy));
            self.stroke_now = Some((cx, cy));
            return PanelAction::Redraw;
        }
        PanelAction::None
    }

    fn handle_up(&mut self, x: f32, y: f32) -> PanelAction {
        let Some(from) = self.stroke_from.take() else {
            return PanelAction::None;
        };
        let to = self.canvas_pt(x, y).or(self.stroke_now);
        self.stroke_now = None;
        let Some((tx, ty)) = to else {
            return PanelAction::Redraw;
        };
        let mode = self.spec.canvas.as_ref().map(|c| c.mode);
        let is_arrow = mode == Some(CanvasMode::Annotate) && self.tool == 1;
        if is_arrow {
            let len = ((tx - from.0).powi(2) + (ty - from.1).powi(2)).sqrt();
            if len >= 4.0 {
                self.push_shape(CanvasShape::Arrow { x1: from.0, y1: from.1, x2: tx, y2: ty });
            }
        } else {
            let rx = from.0.min(tx);
            let ry = from.1.min(ty);
            let rw = (tx - from.0).abs();
            let rh = (ty - from.1).abs();
            if rw >= 2.0 && rh >= 2.0 {
                self.push_shape(CanvasShape::Rect { x: rx, y: ry, w: rw, h: rh });
            }
        }
        PanelAction::Redraw
    }

    /// Build the outcome for an Apply.
    pub fn outcome(&self) -> PanelOutcome {
        let mut values: HashMap<String, String> = HashMap::new();
        for (i, f) in self.spec.fields.iter().enumerate() {
            match f {
                PanelField::Text { key, .. } => {
                    values.insert(key.clone(), self.texts[i].clone());
                }
                PanelField::Chips { key, options, .. } => {
                    if let Some(o) = options.get(self.chips[i]) {
                        values.insert(key.clone(), o.clone());
                    }
                }
                PanelField::Toggle { key, .. } => {
                    values.insert(key.clone(), if self.toggles[i] { "true".into() } else { "false".into() });
                }
                PanelField::Info { .. } => {}
            }
        }
        let pages = self
            .shapes
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(page, shapes)| PageShapes { page: *page, shapes: shapes.clone() })
            .collect();
        PanelOutcome::Applied { values, pages }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw_text(
    pm: &mut Pixmap,
    text: &str,
    x: f32,
    baseline: f32,
    size: f32,
    bold: bool,
    color: (u8, u8, u8),
    rtl: bool,
) {
    let glyphs = display_order(text);
    let w = fonts().measure(&glyphs, size, bold);
    let start = if rtl { x - w } else { x };
    fonts().draw(pm, &glyphs, start, baseline, size, bold, col(color, 255));
}

fn star(pm: &mut Pixmap, cx: f32, cy: f32, r: f32) {
    let pts = crate::geom::star_points(cx, cy, r, r * 0.42);
    let mut b = PathBuilder::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        if i == 0 {
            b.move_to(*x, *y);
        } else {
            b.line_to(*x, *y);
        }
    }
    b.close();
    if let Some(path) = b.finish() {
        pm.fill_path(&path, &paint(GOLD, 255), FillRule::Winding, Transform::identity(), None);
    }
}

fn draw_pill(pm: &mut Pixmap, rect: Rect, label: &str, selected: bool, rtl: bool) {
    let path = rounded_rect_path(rect.x, rect.y, rect.w, rect.h, 15.0);
    if selected {
        pm.fill_path(&path, &paint(GOLD, 255), FillRule::Winding, Transform::identity(), None);
    } else {
        pm.fill_path(&path, &paint(FIELD_BG, 255), FillRule::Winding, Transform::identity(), None);
        let mut st = Stroke::default();
        st.width = 1.0;
        pm.stroke_path(&path, &paint(FIELD_LINE, 255), &st, Transform::identity(), None);
    }
    let glyphs = display_order(label);
    let w = fonts().measure(&glyphs, 12.5, true);
    fonts().draw(
        pm,
        &glyphs,
        rect.x + (rect.w - w) / 2.0,
        rect.y + rect.h / 2.0 + 4.5,
        12.5,
        true,
        col(if selected { SEG_INK } else { CREAM_DIM }, 255),
    );
    let _ = rtl;
}

pub fn render_panel(state: &PanelState, preview: Option<&Pixmap>) -> Pixmap {
    let spec = &state.spec;
    let layout = &state.layout;
    let (w, h) = (layout.width, layout.height);
    let mut pm = Pixmap::new(w, h).expect("panel pixmap");
    let wf = w as f32;
    let rtl = spec.rtl;

    // Card.
    let card = rounded_rect_path(1.0, 1.0, wf - 2.0, h as f32 - 2.0, 14.0);
    pm.fill_path(&card, &paint(LAPIS_900, 252), FillRule::Winding, Transform::identity(), None);
    let mut st = Stroke::default();
    st.width = 1.5;
    pm.stroke_path(&card, &paint(GOLD, 200), &st, Transform::identity(), None);

    // Title + mark.
    let title_x = if rtl { wf - PAD } else { PAD };
    draw_text(&mut pm, &spec.title, title_x, 27.0, 15.0, true, CREAM, rtl);
    star(&mut pm, if rtl { 24.0 } else { wf - 24.0 }, 21.0, 10.0);

    // Fields.
    for (i, field) in spec.fields.iter().enumerate() {
        let rect = layout.field_rects[i];
        match field {
            PanelField::Text { label, .. } => {
                let lx = if rtl { wf - PAD } else { PAD };
                draw_text(&mut pm, label, lx, rect.y - 4.0, 12.0, false, CREAM_DIM, rtl);
                let box_path = rounded_rect_path(rect.x, rect.y, rect.w, rect.h, 8.0);
                pm.fill_path(&box_path, &paint(FIELD_BG, 255), FillRule::Winding, Transform::identity(), None);
                let focused = state.focus == Some(FocusTarget::Field(i));
                let mut bst = Stroke::default();
                bst.width = if focused { 1.8 } else { 1.0 };
                pm.stroke_path(
                    &box_path,
                    &paint(if focused { GOLD } else { FIELD_LINE }, 255),
                    &bst,
                    Transform::identity(),
                    None,
                );
                let value = &state.texts[i];
                let glyphs = display_order(value);
                let tw = fonts().measure(&glyphs, 13.5, false);
                let tx = if rtl { rect.right() - 10.0 - tw } else { rect.x + 10.0 };
                fonts().draw(&mut pm, &glyphs, tx, rect.y + 22.0, 13.5, false, col(CREAM, 255));
                if focused {
                    let caret_x = if rtl { tx - 3.0 } else { tx + tw + 2.0 };
                    let caret = rounded_rect_path(caret_x, rect.y + 8.0, 1.6, rect.h - 16.0, 0.8);
                    pm.fill_path(&caret, &paint(GOLD_BRIGHT, 255), FillRule::Winding, Transform::identity(), None);
                }
            }
            PanelField::Chips { label, options, .. } => {
                let lx = if rtl { wf - PAD } else { PAD };
                draw_text(&mut pm, label, lx, rect.y - 4.0, 12.0, false, CREAM_DIM, rtl);
                for (ci, pill) in layout.chip_rects[i].iter().enumerate() {
                    if let Some(o) = options.get(ci) {
                        draw_pill(&mut pm, *pill, o, state.chips[i] == ci, rtl);
                    }
                }
            }
            PanelField::Toggle { label, .. } => {
                let lx = if rtl { wf - PAD } else { PAD };
                draw_text(&mut pm, label, lx, rect.y + 19.0, 12.5, false, CREAM, rtl);
                // Switch at the trailing edge.
                let sw_w = 42.0;
                let sw_x = if rtl { PAD } else { rect.right() - sw_w };
                let sw = rounded_rect_path(sw_x, rect.y + 3.0, sw_w, 22.0, 11.0);
                let on = state.toggles[i];
                pm.fill_path(
                    &sw,
                    &paint(if on { GOLD } else { FIELD_BG }, 255),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
                let knob_x = if on { sw_x + sw_w - 19.0 } else { sw_x + 3.0 };
                let knob = rounded_rect_path(knob_x, rect.y + 6.0, 16.0, 16.0, 8.0);
                pm.fill_path(
                    &knob,
                    &paint(if on { LAPIS_950 } else { CREAM_DIM }, 255),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            PanelField::Info { .. } => {
                for (li, line) in layout.info_lines[i].iter().enumerate() {
                    let lx = if rtl { wf - PAD } else { PAD };
                    draw_text(
                        &mut pm,
                        line,
                        lx,
                        rect.y + 14.0 + li as f32 * 18.0,
                        12.5,
                        false,
                        CREAM,
                        rtl,
                    );
                }
            }
        }
    }

    // Canvas.
    if let (Some(rect), Some(cv)) = (layout.canvas_rect, &spec.canvas) {
        let bg = rounded_rect_path(rect.x, rect.y, rect.w, rect.h, 8.0);
        pm.fill_path(&bg, &paint((5, 11, 24), 255), FillRule::Winding, Transform::identity(), None);
        if let Some(pv) = preview {
            let sx = rect.w / pv.width() as f32;
            let sy = rect.h / pv.height() as f32;
            pm.draw_pixmap(
                rect.x as i32,
                rect.y as i32,
                pv.as_ref(),
                &PixmapPaint::default(),
                Transform::from_scale(sx, sy),
                None,
            );
        }
        let mut bst = Stroke::default();
        bst.width = 1.2;
        pm.stroke_path(&bg, &paint(FIELD_LINE, 255), &bst, Transform::identity(), None);

        let to_panel = |cx: f32, cy: f32| {
            (
                rect.x + cx / cv.coord_w.max(1.0) * rect.w,
                rect.y + cy / cv.coord_h.max(1.0) * rect.h,
            )
        };
        let draw_shape = |pm: &mut Pixmap, shape: &CanvasShape, ghost: bool| {
            let alpha = if ghost { 150 } else { 255 };
            match shape {
                CanvasShape::Rect { x, y, w: sw, h: sh } => {
                    let (x1, y1) = to_panel(*x, *y);
                    let (x2, y2) = to_panel(x + sw, y + sh);
                    let r = rounded_rect_path(x1, y1, (x2 - x1).abs(), (y2 - y1).abs(), 1.0);
                    if cv.mode == CanvasMode::Rects {
                        pm.fill_path(&r, &paint((0, 0, 0), 255), FillRule::Winding, Transform::identity(), None);
                        let mut s2 = Stroke::default();
                        s2.width = 2.0;
                        pm.stroke_path(&r, &paint(DANGER, alpha), &s2, Transform::identity(), None);
                    } else {
                        let mut s2 = Stroke::default();
                        s2.width = 3.0;
                        pm.stroke_path(&r, &paint(GOLD, alpha), &s2, Transform::identity(), None);
                    }
                }
                CanvasShape::Arrow { x1, y1, x2, y2 } => {
                    let (ax, ay) = to_panel(*x1, *y1);
                    let (bx, by) = to_panel(*x2, *y2);
                    let mut b = PathBuilder::new();
                    b.move_to(ax, ay);
                    b.line_to(bx, by);
                    let ang = (by - ay).atan2(bx - ax);
                    for da in [2.72f32, -2.72] {
                        b.move_to(bx, by);
                        b.line_to(bx + 14.0 * (ang + da).cos(), by + 14.0 * (ang + da).sin());
                    }
                    if let Some(path) = b.finish() {
                        let mut s2 = Stroke::default();
                        s2.width = 3.0;
                        pm.stroke_path(&path, &paint(GOLD, alpha), &s2, Transform::identity(), None);
                    }
                }
                CanvasShape::Text { x, y, text } => {
                    let (tx, ty) = to_panel(*x, *y);
                    draw_text(pm, text, tx, ty, 15.0, true, GOLD_BRIGHT, false);
                }
            }
        };
        if let Some(shapes) = state.shapes.get(&state.page) {
            let shapes = shapes.clone();
            for shape in &shapes {
                draw_shape(&mut pm, shape, false);
            }
        }
        // In-progress stroke.
        if let (Some(from), Some(now)) = (state.stroke_from, state.stroke_now) {
            let ghost = if cv.mode == CanvasMode::Annotate && state.tool == 1 {
                CanvasShape::Arrow { x1: from.0, y1: from.1, x2: now.0, y2: now.1 }
            } else {
                CanvasShape::Rect {
                    x: from.0.min(now.0),
                    y: from.1.min(now.1),
                    w: (now.0 - from.0).abs(),
                    h: (now.1 - from.1).abs(),
                }
            };
            draw_shape(&mut pm, &ghost, true);
        }

        // Pager.
        if cv.pages > 1 {
            let label = format!("{} / {}", state.page, state.pages);
            let glyphs = display_order(&label);
            let tw = fonts().measure(&glyphs, 12.5, true);
            fonts().draw(
                &mut pm,
                &glyphs,
                (wf - tw) / 2.0,
                layout.prev_rect.map(|r| r.y + 18.0).unwrap_or(rect.bottom() + 22.0),
                12.5,
                true,
                col(CREAM_DIM, 255),
            );
            for (r, flip) in [(layout.prev_rect, false), (layout.next_rect, true)] {
                if let Some(r) = r {
                    let box_path = rounded_rect_path(r.x, r.y, r.w, r.h, 7.0);
                    pm.fill_path(&box_path, &paint(FIELD_BG, 255), FillRule::Winding, Transform::identity(), None);
                    // Chevron.
                    let cxm = r.x + r.w / 2.0;
                    let cym = r.y + r.h / 2.0;
                    let dir = if flip { 1.0 } else { -1.0 };
                    let mut b = PathBuilder::new();
                    b.move_to(cxm - dir * 4.0, cym - 6.0);
                    b.line_to(cxm + dir * 4.0, cym);
                    b.line_to(cxm - dir * 4.0, cym + 6.0);
                    if let Some(path) = b.finish() {
                        let mut s2 = Stroke::default();
                        s2.width = 2.0;
                        pm.stroke_path(&path, &paint(CREAM, 255), &s2, Transform::identity(), None);
                    }
                }
            }
        }

        // Annotate tool row + label box.
        if cv.mode == CanvasMode::Annotate {
            let labels = if cv.tool_labels.len() == 3 {
                cv.tool_labels.clone()
            } else {
                vec!["Rect".into(), "Arrow".into(), "Text".into()]
            };
            for (ti, r) in layout.tool_rects.iter().enumerate() {
                if let Some(l) = labels.get(ti) {
                    draw_pill(&mut pm, *r, l, state.tool == ti, rtl);
                }
            }
            if let Some(lr) = layout.label_rect {
                let lx = if rtl { wf - PAD } else { PAD };
                draw_text(&mut pm, &cv.label_caption, lx, lr.y - 4.0, 12.0, false, CREAM_DIM, rtl);
                let box_path = rounded_rect_path(lr.x, lr.y, lr.w, lr.h, 8.0);
                pm.fill_path(&box_path, &paint(FIELD_BG, 255), FillRule::Winding, Transform::identity(), None);
                let focused = state.focus == Some(FocusTarget::LabelBox);
                let mut bst = Stroke::default();
                bst.width = if focused { 1.8 } else { 1.0 };
                pm.stroke_path(
                    &box_path,
                    &paint(if focused { GOLD } else { FIELD_LINE }, 255),
                    &bst,
                    Transform::identity(),
                    None,
                );
                let glyphs = display_order(&state.label_text);
                let tw = fonts().measure(&glyphs, 13.5, false);
                let tx = if rtl { lr.right() - 10.0 - tw } else { lr.x + 10.0 };
                fonts().draw(&mut pm, &glyphs, tx, lr.y + 22.0, 13.5, false, col(CREAM, 255));
            }
        }
    }

    // Buttons.
    let cancel = layout.cancel_rect;
    let cancel_path = rounded_rect_path(cancel.x, cancel.y, cancel.w, cancel.h, 9.0);
    pm.fill_path(&cancel_path, &paint(FIELD_BG, 255), FillRule::Winding, Transform::identity(), None);
    let mut bst = Stroke::default();
    bst.width = 1.0;
    pm.stroke_path(&cancel_path, &paint(FIELD_LINE, 255), &bst, Transform::identity(), None);
    let glyphs = display_order(&spec.cancel_label);
    let tw = fonts().measure(&glyphs, 13.5, true);
    fonts().draw(
        &mut pm,
        &glyphs,
        cancel.x + (cancel.w - tw) / 2.0,
        cancel.y + 23.0,
        13.5,
        true,
        col(CREAM, 255),
    );
    if let Some(apply) = layout.apply_rect {
        let path = rounded_rect_path(apply.x, apply.y, apply.w, apply.h, 9.0);
        pm.fill_path(&path, &paint(GOLD, 255), FillRule::Winding, Transform::identity(), None);
        let glyphs = display_order(&spec.apply_label);
        let tw = fonts().measure(&glyphs, 13.5, true);
        fonts().draw(
            &mut pm,
            &glyphs,
            apply.x + (apply.w - tw) / 2.0,
            apply.y + 23.0,
            13.5,
            true,
            col(SEG_INK, 255),
        );
    }
    pm
}

/// Decode a PNG file's bytes into a preview pixmap (premultiplied, as
/// tiny-skia stores). Panels always preview PNGs the shell rendered.
pub fn decode_png(bytes: &[u8]) -> Option<Pixmap> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let mut rgba = Vec::with_capacity(buf.len());
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(&buf),
        png::ColorType::Rgb => {
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for px in buf.chunks_exact(1) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in buf.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        png::ColorType::Indexed => return None,
    }
    // Premultiply (tiny-skia's storage format).
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = (px[0] as u32 * a / 255) as u8;
        px[1] = (px[1] as u32 * a / 255) as u8;
        px[2] = (px[2] as u32 * a / 255) as u8;
    }
    Pixmap::from_vec(rgba, IntSize::from_wh(w, h)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CanvasSpec, PanelField, PanelSpec};

    fn form_spec() -> PanelSpec {
        PanelSpec {
            id: "t".into(),
            title: "Crop".into(),
            anchor: (0, 0),
            fields: vec![
                PanelField::Text { key: "w".into(), label: "Width".into(), value: "100".into() },
                PanelField::Chips {
                    key: "ratio".into(),
                    label: "Ratio".into(),
                    options: vec!["Free".into(), "1:1".into()],
                    selected: 0,
                },
                PanelField::Toggle { key: "flip".into(), label: "Flip".into(), on: false },
            ],
            canvas: None,
            apply_label: "Apply".into(),
            cancel_label: "Cancel".into(),
            rtl: false,
        }
    }

    #[test]
    fn form_layout_and_typing() {
        let mut st = PanelState::new(form_spec());
        assert!(st.layout.height > 150 && st.layout.width == FORM_W);
        // Focus starts on the first text field; typing edits it.
        for c in "23".chars() {
            assert_eq!(st.handle(PanelEvent::Key(PanelKey::Char(c))), PanelAction::Redraw);
        }
        assert_eq!(st.texts[0], "10023");
        st.handle(PanelEvent::Key(PanelKey::Backspace));
        assert_eq!(st.texts[0], "1002");
        // Chip click selects.
        let pill = st.layout.chip_rects[1][1];
        st.handle(PanelEvent::Down { x: pill.x + 2.0, y: pill.y + 2.0 });
        assert_eq!(st.chips[1], 1);
        // Toggle row flips.
        let row = st.layout.field_rects[2];
        st.handle(PanelEvent::Down { x: row.x + 4.0, y: row.y + 4.0 });
        assert!(st.toggles[2]);
        // Outcome carries typed/selected/flipped values.
        match st.outcome() {
            PanelOutcome::Applied { values, pages } => {
                assert_eq!(values.get("w").map(|s| s.as_str()), Some("1002"));
                assert_eq!(values.get("ratio").map(|s| s.as_str()), Some("1:1"));
                assert_eq!(values.get("flip").map(|s| s.as_str()), Some("true"));
                assert!(pages.is_empty());
            }
            _ => panic!("expected Applied"),
        }
        // Esc cancels; Enter applies.
        assert_eq!(st.handle(PanelEvent::Key(PanelKey::Escape)), PanelAction::Cancel);
        assert_eq!(st.handle(PanelEvent::Key(PanelKey::Enter)), PanelAction::Apply);
        // Render must not panic.
        let pm = render_panel(&st, None);
        assert_eq!(pm.width(), st.layout.width);
    }

    #[test]
    fn canvas_rects_and_paging() {
        let spec = PanelSpec {
            id: "r".into(),
            title: "Redact".into(),
            anchor: (0, 0),
            fields: vec![],
            canvas: Some(CanvasSpec {
                image_png: "x.png".into(),
                coord_w: 612.0,
                coord_h: 792.0,
                mode: CanvasMode::Rects,
                page: 1,
                pages: 3,
                tool_labels: vec![],
                label_caption: String::new(),
            }),
            apply_label: "Redact".into(),
            cancel_label: "Cancel".into(),
            rtl: false,
        };
        let mut st = PanelState::new(spec);
        let rect = st.layout.canvas_rect.expect("canvas rect");
        // Drag a rect across the middle: coords map into page points.
        st.handle(PanelEvent::Down { x: rect.x + rect.w * 0.25, y: rect.y + rect.h * 0.25 });
        st.handle(PanelEvent::Move { x: rect.x + rect.w * 0.75, y: rect.y + rect.h * 0.5 });
        st.handle(PanelEvent::Up { x: rect.x + rect.w * 0.75, y: rect.y + rect.h * 0.5 });
        let shapes = st.shapes.get(&1).expect("page 1 shapes");
        assert_eq!(shapes.len(), 1);
        match &shapes[0] {
            CanvasShape::Rect { x, y, w, h } => {
                assert!((x - 153.0).abs() < 2.0, "x={x}");
                assert!((w - 306.0).abs() < 2.0, "w={w}");
                assert!((y - 198.0).abs() < 2.0, "y={y}");
                assert!((h - 198.0).abs() < 2.0, "h={h}");
            }
            other => panic!("expected Rect, got {other:?}"),
        }
        // Next-page button asks the host for page 2.
        let next = st.layout.next_rect.expect("next rect");
        assert_eq!(
            st.handle(PanelEvent::Down { x: next.x + 3.0, y: next.y + 3.0 }),
            PanelAction::NeedPage(2)
        );
        st.set_canvas_page(2, 3);
        assert_eq!(st.page, 2);
        // Backspace with no focus pops the current page's shape only.
        st.handle(PanelEvent::Down { x: rect.x + 10.0, y: rect.y + 10.0 });
        st.handle(PanelEvent::Up { x: rect.x + 60.0, y: rect.y + 60.0 });
        assert_eq!(st.shapes.get(&2).map(|v| v.len()), Some(1));
        st.focus = None;
        st.handle(PanelEvent::Key(PanelKey::Backspace));
        assert_eq!(st.shapes.get(&2).map(|v| v.len()), Some(0));
        assert_eq!(st.shapes.get(&1).map(|v| v.len()), Some(1), "page 1 shape untouched");
        // Outcome reports both pages' shapes (page 2 now empty → omitted).
        match st.outcome() {
            PanelOutcome::Applied { pages, .. } => {
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].page, 1);
            }
            _ => panic!("expected Applied"),
        }
        let pm = render_panel(&st, None);
        assert_eq!(pm.width(), CANVAS_W);
    }
}
