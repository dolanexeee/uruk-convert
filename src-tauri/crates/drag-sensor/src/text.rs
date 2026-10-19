//! Glyph rendering for the native surfaces. tiny-skia draws the shapes;
//! glyphs come from real TTF faces discovered on the system (fontconfig
//! directories), rasterized via ttf-parser outlines. Arabic text must be
//! shaped BEFORE it reaches this module — see [`crate::arabic`], which
//! produces visual-order [`ShapedGlyph`] sequences; this module is a
//! dumb, correct glyph pump (per-glyph face fallback included, so an
//! Arabic presentation form and a Latin digit can share one line).

use std::sync::OnceLock;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};
use ttf_parser::{Face, GlyphId, OutlineBuilder};

use crate::arabic::ShapedGlyph;

struct LoadedFace {
    face: Face<'static>,
    bold: bool,
    arabic: bool,
}

pub struct FontSet {
    faces: Vec<LoadedFace>,
}

impl FontSet {
    fn load() -> FontSet {
        let mut faces = Vec::new();
        for (path, bold, arabic) in discover_fonts() {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                    if let Ok(face) = Face::parse(leaked, 0) {
                        faces.push(LoadedFace { face, bold, arabic });
                    }
                }
                Err(_) => continue,
            }
        }
        FontSet { faces }
    }

    pub fn available(&self) -> bool {
        !self.faces.is_empty()
    }

    fn pick(&self, cp: char, bold: bool) -> Option<&LoadedFace> {
        let want_arabic = matches!(cp, '\u{0600}'..='\u{06FF}' | '\u{FB50}'..='\u{FEFF}');
        // Order: (style match, script match) best-first.
        let mut best: Option<(&LoadedFace, u8)> = None;
        for f in &self.faces {
            if f.face.glyph_index(cp).is_none() {
                continue;
            }
            let mut score = 0u8;
            if f.bold == bold {
                score += 2;
            }
            if f.arabic == want_arabic {
                score += 1;
            }
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((f, score));
            }
        }
        best.map(|(f, _)| f)
    }

    fn advance(&self, f: &LoadedFace, cp: char, size: f32) -> f32 {
        let upem = f.face.units_per_em() as f32;
        match f.face.glyph_index(cp) {
            Some(gid) => f.face.glyph_hor_advance(gid).unwrap_or(0) as f32 * size / upem,
            None => size * 0.5,
        }
    }

    /// Total advance width of a shaped glyph run.
    pub fn measure(&self, glyphs: &[ShapedGlyph], size: f32, bold: bool) -> f32 {
        glyphs
            .iter()
            .map(|g| {
                if g.zero_advance {
                    0.0
                } else {
                    self.pick(g.ch, bold)
                        .map(|f| self.advance(f, g.ch, size))
                        .unwrap_or(size * 0.5)
                }
            })
            .sum()
    }

    /// Draw a shaped glyph run with the pen starting at `(x, baseline)`.
    /// Returns the advance consumed. Colour is a flat fill.
    pub fn draw(
        &self,
        pixmap: &mut Pixmap,
        glyphs: &[ShapedGlyph],
        x: f32,
        baseline: f32,
        size: f32,
        bold: bool,
        color: Color,
    ) -> f32 {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let mut pen = x;
        for g in glyphs {
            let Some(f) = self.pick(g.ch, bold) else {
                continue;
            };
            let Some(gid) = f.face.glyph_index(g.ch) else {
                continue;
            };
            let upem = f.face.units_per_em() as f32;
            let s = size / upem;
            let mut builder = OutlineSink {
                path: PathBuilder::new(),
                ox: pen,
                oy: baseline,
                s,
            };
            if f.face.outline_glyph(gid, &mut builder).is_some() {
                if let Some(path) = builder.path.finish() {
                    pixmap.fill_path(
                        &path,
                        &paint,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
            if !g.zero_advance {
                pen += self.advance(f, g.ch, size);
            }
        }
        pen - x
    }
}

/// ttf-parser outline sink writing device-space points straight into a
/// tiny-skia path (y axis flipped: font space is y-up, pixmap is y-down).
struct OutlineSink {
    path: PathBuilder,
    ox: f32,
    oy: f32,
    s: f32,
}

impl OutlineSink {
    fn px(&self, x: f32) -> f32 {
        self.ox + x * self.s
    }
    fn py(&self, y: f32) -> f32 {
        self.oy - y * self.s
    }
}

impl OutlineBuilder for OutlineSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to(self.px(x), self.py(y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(self.px(x), self.py(y));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.path
            .quad_to(self.px(x1), self.py(y1), self.px(x), self.py(y));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.path.cubic_to(
            self.px(x1),
            self.py(y1),
            self.px(x2),
            self.py(y2),
            self.px(x),
            self.py(y),
        );
    }
    fn close(&mut self) {
        self.path.close();
    }
}

pub fn fonts() -> &'static FontSet {
    static FONTS: OnceLock<FontSet> = OnceLock::new();
    FONTS.get_or_init(FontSet::load)
}

/// Font discovery: (path, is_bold, is_arabic_script_face), best first.
/// Pragmatic filename matching over the standard font directories —
/// enough for the short strings the native surfaces render.
fn discover_fonts() -> Vec<(std::path::PathBuf, bool, bool)> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    #[cfg(target_os = "linux")]
    {
        dirs.push("/usr/share/fonts".into());
        dirs.push("/usr/local/share/fonts".into());
        if let Some(h) = std::env::var_os("HOME") {
            let home = std::path::PathBuf::from(h);
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push("/System/Library/Fonts".into());
        dirs.push("/Library/Fonts".into());
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(std::path::PathBuf::from(h).join("Library/Fonts"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        dirs.push("C:\\Windows\\Fonts".into());
    }

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for d in &dirs {
        collect_fonts(d, 0, &mut files);
    }
    let name_of = |p: &std::path::Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    // (needles in priority order, bold, arabic)
    let wanted: &[(&[&str], bool, bool)] = &[
        (
            &[
                "notonaskharabic-bold",
                "notosansarabic-bold",
                "amiri-bold",
                "geeza",
            ],
            true,
            true,
        ),
        (
            &["notonaskharabic", "notosansarabic", "amiri", "naskh"],
            false,
            true,
        ),
        (
            &[
                "dejavusans-bold",
                "notosans-bold",
                "liberationsans-bold",
                "segoeuib",
                "arialbd",
                "tahomabd",
            ],
            true,
            false,
        ),
        (
            &[
                "dejavusans",
                "notosans",
                "liberationsans",
                "segoeui",
                "arial",
                "tahoma",
            ],
            false,
            false,
        ),
    ];
    let mut out = Vec::new();
    for (needles, bold, arabic) in wanted {
        for n in *needles {
            if let Some(p) = files.iter().find(|p| name_of(p).contains(n)) {
                out.push((p.clone(), *bold, *arabic));
                break;
            }
        }
    }
    out
}

fn collect_fonts(dir: &std::path::Path, depth: u8, out: &mut Vec<std::path::PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_fonts(&p, depth + 1, out);
        } else if p
            .extension()
            .map(|x| x.eq_ignore_ascii_case("ttf") || x.eq_ignore_ascii_case("otf"))
            .unwrap_or(false)
        {
            out.push(p);
        }
    }
}

/// Keep the compiler honest about GlyphId use in this module.
#[allow(dead_code)]
fn _unused(_: GlyphId) {}
