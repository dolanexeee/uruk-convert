//! Wheel geometry — one source of truth, matching the webview wheel
//! (`src/components/Wheel.tsx`) so the native overlay and the in-window
//! wheel are the same object: a 480-unit disc, segments between radii
//! 80 and 222, a 2.2° gap between neighbours, angles measured clockwise
//! from 12 o'clock.

pub const WHEEL_SIZE: f32 = 480.0;
pub const CENTER: f32 = WHEEL_SIZE / 2.0;
pub const R_OUT: f32 = 222.0;
pub const R_IN: f32 = 80.0;
pub const HALO_R: f32 = R_OUT + 6.0;
pub const GAP_DEG: f32 = 2.2;

/// Point at radius `r` and angle `deg` (0 = up, clockwise), centre-relative
/// in a coordinate system where +y is DOWN (screen coordinates).
pub fn polar(r: f32, deg: f32) -> (f32, f32) {
    let rad = deg.to_radians();
    (r * rad.sin(), -r * rad.cos())
}

/// Point at radius `r`, angle `deg`, absolute in wheel coordinates.
pub fn polar_abs(r: f32, deg: f32) -> (f32, f32) {
    let (dx, dy) = polar(r, deg);
    (CENTER + dx, CENTER + dy)
}

/// Angular span of one segment when there are `n`.
pub fn span(n: usize) -> f32 {
    if n == 0 {
        360.0
    } else {
        360.0 / n as f32
    }
}

/// (start, end) angles of segment `i` of `n`, gap applied.
pub fn sector_angles(n: usize, i: usize) -> (f32, f32) {
    let s = span(n);
    let gap = if n > 1 { GAP_DEG } else { 0.0 };
    (i as f32 * s + gap / 2.0, (i + 1) as f32 * s - gap / 2.0)
}

/// Centre point of segment `i`'s label (mid radius, mid angle).
pub fn label_pos(n: usize, i: usize) -> (f32, f32) {
    let (a0, a1) = sector_angles(n, i);
    polar_abs((R_OUT + R_IN) / 2.0, (a0 + a1) / 2.0)
}

/// Which segment (if any) contains the point at centre-relative
/// `(dx, dy)`? `None` over the hub, beyond the rim, or in a gap.
pub fn segment_at(n: usize, dx: f32, dy: f32) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let r = (dx * dx + dy * dy).sqrt();
    if r < R_IN || r > HALO_R {
        return None;
    }
    // Angle clockwise from 12 o'clock, 0..360.
    let mut deg = dx.atan2(-dy).to_degrees();
    if deg < 0.0 {
        deg += 360.0;
    }
    let s = span(n);
    let idx = (deg / s) as usize;
    if idx >= n {
        return None;
    }
    let (a0, a1) = sector_angles(n, idx);
    if deg >= a0 && deg <= a1 {
        Some(idx)
    } else {
        None // in the gap between segments
    }
}

/// Points of the 8-pointed Shamash star centred at `(cx, cy)` —
/// same proportions as the product mark (outer 24, inner 10 at hub scale).
pub fn star_points(cx: f32, cy: f32, r_outer: f32, r_inner: f32) -> Vec<(f32, f32)> {
    let mut pts = Vec::with_capacity(16);
    for i in 0..16 {
        let r = if i % 2 == 0 { r_outer } else { r_inner };
        let (dx, dy) = polar(r, i as f32 * 22.5);
        pts.push((cx + dx, cy + dy));
    }
    pts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_zero_is_top_right_of_centre() {
        // 8 segments: segment 0 spans ~1.1°..43.9° (upper-right quadrant).
        let (dx, dy) = polar(150.0, 22.5);
        assert_eq!(segment_at(8, dx, dy), Some(0));
    }

    #[test]
    fn hub_and_outside_have_no_segment() {
        assert_eq!(segment_at(8, 0.0, 0.0), None);
        assert_eq!(segment_at(8, 10.0, -10.0), None);
        assert_eq!(segment_at(8, 400.0, 0.0), None);
    }

    #[test]
    fn gap_is_dead() {
        // Exactly on the boundary between segments 0 and 1 (45°) is a gap.
        let (dx, dy) = polar(150.0, 45.0);
        assert_eq!(segment_at(8, dx, dy), None);
    }

    #[test]
    fn seven_targets_like_a_png() {
        // PNG input → targets [jpg webp heic tiff avif bmp pdf]; jpg is
        // segment 0 at mid-angle 360/14 ≈ 25.7°.
        let (dx, dy) = polar(151.0, 360.0 / 14.0);
        assert_eq!(segment_at(7, dx, dy), Some(0));
        // Last segment (pdf) sits just counterclockwise of the top.
        let (dx, dy) = polar(151.0, 360.0 - 360.0 / 14.0);
        assert_eq!(segment_at(7, dx, dy), Some(6));
    }

    #[test]
    fn label_positions_are_inside_the_wheel() {
        for n in 1..=9 {
            for i in 0..n {
                let (x, y) = label_pos(n, i);
                assert!(x >= 0.0 && x <= WHEEL_SIZE && y >= 0.0 && y <= WHEEL_SIZE);
            }
        }
    }
}
