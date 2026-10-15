//! The tone **curve**: how a native sample value becomes a display level.
//!
//! A curve is a short list of points `(value, level)` — `value` in the frame's
//! native units (what the histogram's x axis shows), `level` a display level in
//! `0..=255` — joined by straight segments and held flat past either end. The
//! everyday Linear tone is the two-point curve `(lo, 0) → (hi, 255)`
//! ([`Curve::linear`]), so the clip percentile, Share clip, global statistics and
//! a region tone all still produce `[lo, hi]` and simply wrap it; only a
//! **manual** curve (`ToneOptions::curve`, edited on the Transformations
//! histogram) has more points or levels other than 0 and 255.
//!
//! The capacity is fixed ([`MAX_POINTS`]) so a curve is `Copy` and rides the
//! `Copy` tone options and render jobs by value, like the palette does.

use std::hash::{Hash, Hasher};

/// Most points a curve holds. Plenty for a hand-drawn tone curve, and small
/// enough that a per-pixel segment search (the float path) stays cheap.
pub const MAX_POINTS: usize = 16;

/// The top display level.
pub const LEVEL_MAX: f32 = 255.0;

/// A piecewise-linear `value → level` map; see the module docs.
///
/// Invariants: at least two points, sorted by value (equal values allowed),
/// levels within `0..=255` — except an inverted [`linear`](Self::linear)
/// window, kept as given so it maps (everything to 0) as it always did.
#[derive(Clone, Copy, Debug)]
pub struct Curve {
    len: u8,
    pts: [[f32; 2]; MAX_POINTS],
}

impl Curve {
    /// The plain linear window `[lo, hi] → [0, 255]`.
    pub fn linear(lo: f32, hi: f32) -> Self {
        let mut pts = [[0.0; 2]; MAX_POINTS];
        pts[0] = [lo, 0.0];
        pts[1] = [hi, LEVEL_MAX];
        Self { len: 2, pts }
    }

    /// A curve through `pts`, sorted by value, levels clamped to `0..=255` and
    /// truncated to [`MAX_POINTS`]. A single point (or none) is padded into a
    /// flat curve so the invariants hold.
    pub fn from_points(pts: &[[f32; 2]]) -> Self {
        let mut c = Self {
            len: 0,
            pts: [[0.0; 2]; MAX_POINTS],
        };
        for &[x, y] in pts.iter().take(MAX_POINTS) {
            c.pts[c.len as usize] = [x, y.clamp(0.0, LEVEL_MAX)];
            c.len += 1;
        }
        c.pts[..c.len as usize].sort_by(|a, b| a[0].total_cmp(&b[0]));
        while c.len < 2 {
            let p = if c.len == 0 { [0.0, 0.0] } else { c.pts[0] };
            c.pts[c.len as usize] = p;
            c.len += 1;
        }
        c
    }

    pub fn points(&self) -> &[[f32; 2]] {
        &self.pts[..self.len as usize]
    }

    /// The value of the first point: below it every sample maps to its level.
    pub fn lo(&self) -> f32 {
        self.pts[0][0]
    }

    /// The value of the last point: above it every sample maps to its level.
    pub fn hi(&self) -> f32 {
        self.pts[self.len as usize - 1][0]
    }

    /// Whether this is a plain [`linear`](Self::linear) window — the shape the
    /// GPU's float kernel (which maps arithmetically from `[lo, hi]`) can draw.
    pub fn is_linear(&self) -> bool {
        self.len == 2 && self.pts[0][1] == 0.0 && self.pts[1][1] == LEVEL_MAX
    }

    /// `s` on a `0..=255 * k` scale, unrounded. For a linear curve this is
    /// exactly the historical `(s - lo) * (255k / (hi - lo))` clamped, empty
    /// window (`hi <= lo`) included — so the default tone renders the very bytes
    /// it always did. A NaN sample stays NaN (and casts to 0 like it used to).
    #[inline]
    fn eval(&self, s: f32, k: f32) -> f32 {
        let p = self.points();
        // The segment `s` falls in: the last one starting at or before it,
        // or the end segments for samples past either end (the clamp below
        // then holds them at that end's level).
        let mut i = 0;
        while i + 2 < p.len() && s >= p[i + 1][0] {
            i += 1;
        }
        let ([x0, y0], [x1, y1]) = (p[i], p[i + 1]);
        let dx = x1 - x0;
        let scale = if dx > 0.0 { (y1 - y0) * k / dx } else { 0.0 };
        (y0 * k + (s - x0) * scale).clamp(y0.min(y1) * k, y0.max(y1) * k)
    }

    /// The display level `s` maps to, unrounded (`0.0..=255.0`).
    pub fn level(&self, s: f32) -> f32 {
        self.eval(s, 1.0)
    }

    /// The `value → [0, 255]` map, shared by the float path and the 8-bit
    /// table build so a tabulated and a per-pixel render map identically.
    #[inline]
    pub fn map_u8(self) -> impl Fn(f32) -> u8 {
        move |s| self.eval(s, 1.0) as u8
    }

    /// The `value → [0, 65535]` map (16-bit operator input path): the same
    /// curve with every level scaled by 257, so 255 lands on 65535.
    #[inline]
    pub fn map_u16(self) -> impl Fn(f32) -> u16 {
        move |s| self.eval(s, 257.0) as u16
    }

    /// Insert `[x, y]` in value order, returning its index, or `None` when the
    /// curve is full.
    pub fn insert(&mut self, [x, y]: [f32; 2]) -> Option<usize> {
        let n = self.len as usize;
        if n >= MAX_POINTS {
            return None;
        }
        let at = self.points().partition_point(|p| p[0] <= x);
        self.pts.copy_within(at..n, at + 1);
        self.pts[at] = [x, y.clamp(0.0, LEVEL_MAX)];
        self.len += 1;
        Some(at)
    }

    /// Remove point `i`, unless only two are left (a curve needs both ends).
    pub fn remove(&mut self, i: usize) -> bool {
        let n = self.len as usize;
        if n <= 2 || i >= n {
            return false;
        }
        self.pts.copy_within(i + 1..n, i);
        self.len -= 1;
        true
    }

    /// Move point `i` to `[x, y]`: the value stays between its neighbours' (so
    /// the order, and every other point, is untouched), the level within
    /// `0..=255`.
    pub fn set(&mut self, i: usize, [x, y]: [f32; 2]) {
        let p = self.points();
        let lo = if i > 0 {
            p[i - 1][0]
        } else {
            f32::NEG_INFINITY
        };
        let hi = p.get(i + 1).map_or(f32::INFINITY, |q| q[0]);
        self.pts[i] = [x.clamp(lo, hi), y.clamp(0.0, LEVEL_MAX)];
    }
}

/// Equality on the exact bits of the used points, so a curve can key the tone
/// table caches (as `lo.to_bits()` / `hi.to_bits()` used to).
impl PartialEq for Curve {
    fn eq(&self, other: &Self) -> bool {
        let bits = |c: &Self| {
            c.points()
                .iter()
                .flatten()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>()
        };
        self.len == other.len && bits(self) == bits(other)
    }
}

impl Eq for Curve {}

impl Hash for Curve {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.len.hash(h);
        for v in self.points().iter().flatten() {
            v.to_bits().hash(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The map Linear used before curves existed, verbatim.
    fn old_u8(lo: f32, hi: f32, s: f32) -> u8 {
        let denom = hi - lo;
        let scale = if denom > 0.0 { 255.0 / denom } else { 0.0 };
        ((s - lo) * scale).clamp(0.0, 255.0) as u8
    }

    fn old_u16(lo: f32, hi: f32, s: f32) -> u16 {
        let denom = hi - lo;
        let scale = if denom > 0.0 { 65535.0 / denom } else { 0.0 };
        ((s - lo) * scale).clamp(0.0, 65535.0) as u16
    }

    /// The default tone must render the very bytes it did before curves: every
    /// 16-bit value, through ordinary, narrow, inverted-empty and float windows.
    #[test]
    fn a_linear_curve_maps_exactly_like_the_old_window() {
        for (lo, hi) in [
            (0.0, 65535.0),
            (1000.0, 60000.0),
            (3.0, 40.0),
            (5.0, 5.0),
            (9.0, 2.0),
            (-0.37, 1.25),
        ] {
            let (m8, m16) = (
                Curve::linear(lo, hi).map_u8(),
                Curve::linear(lo, hi).map_u16(),
            );
            for v in (0..=65535u32).step_by(7).map(|v| v as f32).chain([
                -1.0,
                0.5,
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ]) {
                assert_eq!(m8(v), old_u8(lo, hi, v), "u8 {lo}..{hi} at {v}");
                assert_eq!(m16(v), old_u16(lo, hi, v), "u16 {lo}..{hi} at {v}");
            }
        }
    }

    #[test]
    fn a_curve_interpolates_between_points_and_holds_past_its_ends() {
        let c = Curve::from_points(&[[100.0, 20.0], [200.0, 220.0], [300.0, 120.0]]);
        assert_eq!(c.level(0.0), 20.0);
        assert_eq!(c.level(100.0), 20.0);
        assert_eq!(c.level(150.0), 120.0);
        assert_eq!(c.level(200.0), 220.0);
        assert_eq!(c.level(250.0), 170.0);
        assert_eq!(c.level(300.0), 120.0);
        assert_eq!(c.level(1e9), 120.0);
        assert_eq!(c.map_u16()(200.0), 220 * 257);
    }

    #[test]
    fn edits_keep_the_points_ordered_and_in_range() {
        let mut c = Curve::linear(0.0, 100.0);
        assert_eq!(c.insert([50.0, 300.0]), Some(1));
        assert_eq!(c.points(), &[[0.0, 0.0], [50.0, 255.0], [100.0, 255.0]]);
        // Dragging past a neighbour stops at it.
        c.set(1, [150.0, -4.0]);
        assert_eq!(c.points()[1], [100.0, 0.0]);
        assert!(c.remove(1));
        // Both ends stay.
        assert!(!c.remove(0));
        assert_eq!(c.points().len(), 2);
        let mut full = Curve::linear(0.0, 1.0);
        for i in 0..MAX_POINTS - 2 {
            assert!(full.insert([0.5 + i as f32 * 0.01, 9.0]).is_some());
        }
        assert_eq!(full.insert([0.2, 1.0]), None);
        // Unsorted input is sorted.
        let s = Curve::from_points(&[[9.0, 1.0], [2.0, 3.0]]);
        assert_eq!((s.lo(), s.hi()), (2.0, 9.0));
    }
}
