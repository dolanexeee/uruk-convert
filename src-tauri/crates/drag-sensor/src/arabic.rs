//! Minimal Arabic shaping for the native surfaces (wheel hub, toasts,
//! orb). The native renderer draws glyph-by-glyph from TTF cmaps, which
//! gives it NO shaping: Arabic letters would appear isolated and in
//! logical order. This module produces a *visual-order* glyph sequence
//! using the standard presentation-forms algorithm:
//!
//! 1. Each Arabic letter is mapped to its isolated / final / initial /
//!    medial presentation form by its neighbours' joining classes.
//!    Lam-alef pairs merge into their mandatory ligatures. Diacritics
//!    (harakat) stay glued to their base letter as zero-advance glyphs.
//! 2. A simplified bidi pass orders the result for left-to-right
//!    drawing: base direction from the first strong character; R-runs
//!    reversed, L-runs kept. This covers the strings this app renders —
//!    file names and short status lines — not arbitrary documents.
//!
//! Non-Arabic text passes through unchanged (cluster order preserved).

/// One glyph to draw, in visual (left-to-right) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapedGlyph {
    pub ch: char,
    /// Zero-advance marks (Arabic diacritics) draw on their base letter.
    pub zero_advance: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Join {
    /// Connects to both neighbours (ب ت س ل ك …).
    Dual,
    /// Connects only to the previous letter (ا د ر و ة ى …).
    Right,
    /// Never connects (ء, and everything non-Arabic).
    None,
    /// Diacritics: invisible to joining decisions.
    Transparent,
}

fn join_class(c: char) -> Join {
    match c {
        '\u{064B}'..='\u{0652}' | '\u{0670}' | '\u{0640}' => {
            if c == '\u{0640}' {
                Join::Dual // tatweel stretches between two joined letters
            } else {
                Join::Transparent
            }
        }
        // NOTE: U+0629 (ة) is right-joining and deliberately NOT in the
        // ب range below.
        '\u{0626}'
        | '\u{0628}'
        | '\u{062A}'..='\u{062E}'
        | '\u{0633}'..='\u{063A}'
        | '\u{0641}'..='\u{0647}'
        | '\u{0649}' => {
            // ى (0649) is right-joining; it sits in the range above by
            // codepoint order, so handle it before the range in practice:
            if c == '\u{0649}' {
                Join::Right
            } else {
                Join::Dual
            }
        }
        '\u{0622}'..='\u{0625}'
        | '\u{0627}'
        | '\u{0629}'
        | '\u{062F}'..='\u{0632}'
        | '\u{0648}' => Join::Right,
        _ => Join::None,
    }
}

fn is_arabic_char(c: char) -> bool {
    matches!(c, '\u{0600}'..='\u{06FF}')
}

/// Presentation forms: (isolated, final, initial, medial); initial/medial
/// are `None` for right-joining letters.
fn forms(c: char) -> Option<(char, char, Option<char>, Option<char>)> {
    let m = |a: u32| char::from_u32(a);
    let t = |i: u32, f: u32, n: u32, d: u32| Some((m(i)?, m(f)?, Some(m(n)?), Some(m(d)?)));
    let r = |i: u32, f: u32| Some((m(i)?, m(f)?, None, None));
    match c {
        '\u{0621}' => Some((m(0xFE80)?, m(0xFE80)?, None, None)),
        '\u{0622}' => r(0xFE81, 0xFE82),
        '\u{0623}' => r(0xFE83, 0xFE84),
        '\u{0624}' => r(0xFE85, 0xFE86),
        '\u{0625}' => r(0xFE87, 0xFE88),
        '\u{0626}' => t(0xFE89, 0xFE8A, 0xFE8B, 0xFE8C),
        '\u{0627}' => r(0xFE8D, 0xFE8E),
        '\u{0628}' => t(0xFE8F, 0xFE90, 0xFE91, 0xFE92),
        '\u{0629}' => r(0xFE93, 0xFE94),
        '\u{062A}' => t(0xFE95, 0xFE96, 0xFE97, 0xFE98),
        '\u{062B}' => t(0xFE99, 0xFE9A, 0xFE9B, 0xFE9C),
        '\u{062C}' => t(0xFE9D, 0xFE9E, 0xFE9F, 0xFEA0),
        '\u{062D}' => t(0xFEA1, 0xFEA2, 0xFEA3, 0xFEA4),
        '\u{062E}' => t(0xFEA5, 0xFEA6, 0xFEA7, 0xFEA8),
        '\u{062F}' => r(0xFEA9, 0xFEAA),
        '\u{0630}' => r(0xFEAB, 0xFEAC),
        '\u{0631}' => r(0xFEAD, 0xFEAE),
        '\u{0632}' => r(0xFEAF, 0xFEB0),
        '\u{0633}' => t(0xFEB1, 0xFEB2, 0xFEB3, 0xFEB4),
        '\u{0634}' => t(0xFEB5, 0xFEB6, 0xFEB7, 0xFEB8),
        '\u{0635}' => t(0xFEB9, 0xFEBA, 0xFEBB, 0xFEBC),
        '\u{0636}' => t(0xFEBD, 0xFEBE, 0xFEBF, 0xFEC0),
        '\u{0637}' => t(0xFEC1, 0xFEC2, 0xFEC3, 0xFEC4),
        '\u{0638}' => t(0xFEC5, 0xFEC6, 0xFEC7, 0xFEC8),
        '\u{0639}' => t(0xFEC9, 0xFECA, 0xFECB, 0xFECC),
        '\u{063A}' => t(0xFECD, 0xFECE, 0xFECF, 0xFED0),
        '\u{0641}' => t(0xFED1, 0xFED2, 0xFED3, 0xFED4),
        '\u{0642}' => t(0xFED5, 0xFED6, 0xFED7, 0xFED8),
        '\u{0643}' => t(0xFED9, 0xFEDA, 0xFEDB, 0xFEDC),
        '\u{0644}' => t(0xFEDD, 0xFEDE, 0xFEDF, 0xFEE0),
        '\u{0645}' => t(0xFEE1, 0xFEE2, 0xFEE3, 0xFEE4),
        '\u{0646}' => t(0xFEE5, 0xFEE6, 0xFEE7, 0xFEE8),
        '\u{0647}' => t(0xFEE9, 0xFEEA, 0xFEEB, 0xFEEC),
        '\u{0648}' => r(0xFEED, 0xFEEE),
        '\u{0649}' => r(0xFEEF, 0xFEF0),
        '\u{064A}' => t(0xFEF1, 0xFEF2, 0xFEF3, 0xFEF4),
        '\u{0640}' => Some((c, c, Some(c), Some(c))),
        _ => None,
    }
}

/// Lam-alef ligature (isolated, final) for ل + alef-variant.
fn lam_alef(alef: char) -> Option<(char, char)> {
    let m = |a: u32| char::from_u32(a);
    match alef {
        '\u{0627}' => Some((m(0xFEFB)?, m(0xFEFC)?)),
        '\u{0623}' => Some((m(0xFEF7)?, m(0xFEF8)?)),
        '\u{0625}' => Some((m(0xFEF9)?, m(0xFEFA)?)),
        '\u{0622}' => Some((m(0xFEF5)?, m(0xFEF6)?)),
        _ => None,
    }
}

/// A shaped cluster: one base glyph plus its trailing diacritics.
#[derive(Clone, Debug)]
struct Cluster {
    glyphs: Vec<ShapedGlyph>,
    arabic: bool,
}

fn shape_clusters(input: &str) -> Vec<Cluster> {
    let chars: Vec<char> = input.chars().collect();
    // Previous / next non-transparent character, for join decisions.
    let prev_solid = |i: usize| -> Option<char> {
        (0..i)
            .rev()
            .find(|&j| join_class(chars[j]) != Join::Transparent)
            .map(|j| chars[j])
    };
    let next_solid = |i: usize| -> Option<char> {
        (i + 1..chars.len())
            .find(|&j| join_class(chars[j]) != Join::Transparent)
            .map(|j| chars[j])
    };
    let joins_forward = |c: char| join_class(c) == Join::Dual;
    let accepts_join = |c: char| matches!(join_class(c), Join::Dual | Join::Right);

    let mut clusters: Vec<Cluster> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if join_class(c) == Join::Transparent {
            // Diacritic: glue onto the previous cluster; if there is no
            // base yet, emit as its own zero-advance cluster.
            match clusters.last_mut() {
                Some(cl) => cl.glyphs.push(ShapedGlyph {
                    ch: c,
                    zero_advance: true,
                }),
                None => clusters.push(Cluster {
                    glyphs: vec![ShapedGlyph {
                        ch: c,
                        zero_advance: true,
                    }],
                    arabic: is_arabic_char(c),
                }),
            }
            i += 1;
            continue;
        }
        // Lam-alef ligature.
        if c == '\u{0644}' {
            if let Some(next) = chars.get(i + 1) {
                if let Some((iso, fin)) = lam_alef(*next) {
                    let joins_prev = prev_solid(i).map(joins_forward).unwrap_or(false);
                    let ch = if joins_prev { fin } else { iso };
                    clusters.push(Cluster {
                        glyphs: vec![ShapedGlyph {
                            ch,
                            zero_advance: false,
                        }],
                        arabic: true,
                    });
                    i += 2;
                    continue;
                }
            }
        }
        let shaped = match forms(c) {
            Some((iso, fin, ini, med)) => {
                let joins_prev =
                    prev_solid(i).map(joins_forward).unwrap_or(false) && accepts_join(c);
                let joins_next =
                    joins_forward(c) && next_solid(i).map(accepts_join).unwrap_or(false);
                match (joins_prev, joins_next, ini, med) {
                    (true, true, _, Some(med)) => med,
                    (true, false, _, _) => fin,
                    (false, true, Some(ini), _) => ini,
                    _ => iso,
                }
            }
            None => c,
        };
        clusters.push(Cluster {
            glyphs: vec![ShapedGlyph {
                ch: shaped,
                zero_advance: false,
            }],
            arabic: is_arabic_char(c),
        });
        i += 1;
    }
    clusters
}

/// Shape + order `input` for left-to-right glyph drawing.
pub fn display_order(input: &str) -> Vec<ShapedGlyph> {
    let clusters = shape_clusters(input);
    if !clusters.iter().any(|c| c.arabic) {
        return clusters.into_iter().flat_map(|c| c.glyphs).collect();
    }
    // Base direction: first strong (Arabic or alphabetic) cluster.
    let base_rtl = clusters
        .iter()
        .find(|c| c.arabic || c.glyphs[0].ch.is_alphabetic())
        .map(|c| c.arabic)
        .unwrap_or(true);

    let mut ordered: Vec<Cluster> = Vec::with_capacity(clusters.len());
    if base_rtl {
        // Reverse everything, then restore L-runs to reading order.
        let mut rev: Vec<Cluster> = clusters.into_iter().rev().collect();
        let mut j = 0;
        while j < rev.len() {
            if rev[j].arabic {
                j += 1;
                continue;
            }
            let mut k = j;
            while k < rev.len() && !rev[k].arabic {
                k += 1;
            }
            rev[j..k].reverse();
            j = k;
        }
        ordered.append(&mut rev);
    } else {
        // LTR base: reverse each Arabic run in place.
        let mut j = 0;
        let mut all = clusters;
        while j < all.len() {
            if !all[j].arabic {
                j += 1;
                continue;
            }
            let mut k = j;
            while k < all.len() && all[k].arabic {
                k += 1;
            }
            all[j..k].reverse();
            j = k;
        }
        ordered.append(&mut all);
    }
    ordered.into_iter().flat_map(|c| c.glyphs).collect()
}

/// Convenience: the visual-order string (advance info dropped).
pub fn display_string(input: &str) -> String {
    display_order(input).into_iter().map(|g| g.ch).collect()
}

/// Does this string need shaping/RTL handling at all?
pub fn contains_arabic(input: &str) -> bool {
    input.chars().any(is_arabic_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(s: &str) -> Vec<u32> {
        display_order(s).into_iter().map(|g| g.ch as u32).collect()
    }

    #[test]
    fn latin_passes_through() {
        assert_eq!(display_string("photo 2024.png"), "photo 2024.png");
    }

    #[test]
    fn single_word_shapes_and_reverses() {
        // سلام = seen-lam-alef-meem: lam+alef ligature (final form, since
        // seen joins forward); meem follows an alef, which never joins
        // forward, so meem is ISOLATED. Visual: meem, ligature, seen-initial.
        let v = codes("سلام");
        assert_eq!(v, vec![0xFEE1, 0xFEFC, 0xFEB3]);
    }

    #[test]
    fn word_with_right_joining_letters() {
        // صورة = sad-waw-reh-teh-marbuta. و and ر and ة join only backward.
        // Logical forms: sad initial FEBB, waw final FEEE, reh isolated FEAD,
        // teh-marbuta isolated FE93. Visual = reverse.
        let v = codes("صورة");
        assert_eq!(v, vec![0xFE93, 0xFEAD, 0xFEEE, 0xFEBB]);
    }

    #[test]
    fn filename_mixed_direction() {
        // "صورة 2024.png": visual order should place the Latin/number run
        // on the left, the Arabic word on the right, internally ordered.
        let v = display_string("صورة 2024.png");
        let arabic_part: String = v.chars().filter(|c| *c as u32 >= 0xFE80).collect();
        assert_eq!(arabic_part.chars().count(), 4);
        assert!(v.trim_start().starts_with("2024.png"), "got: {v}");
    }

    #[test]
    fn diacritic_stays_with_base() {
        // ب + fatha: fatha must follow the beh glyph in visual order too.
        let v = display_order("ب\u{064E}");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ch as u32, 0xFE8F);
        assert!(v[1].zero_advance);
    }
}
