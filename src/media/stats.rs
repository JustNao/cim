//! `FrameData` statistics — histograms, region stats, and the Compute-pane
//! reductions. Purely analytic: nothing here touches decoding, caching or
//! texture rendering.

use std::sync::Arc;

use super::{FrameData, Samples};

/// Per-channel histogram plus the true value extent, for the Visualise panel.
pub struct HistData {
    pub bins: Vec<Vec<u32>>, // 1 curve if mono, else R,G,B
    pub min: f32,
    pub max: f32,
    pub mono: bool,
}

/// Statistics over a rectangular region of a frame, for the region stats panel
/// shown under a right-drag selection. The histogram mirrors the Visualise
/// panel; `mean`/`std` carry one entry per colour channel (1 mono, 3 RGB).
pub struct RegionStats {
    pub hist: HistData,
    pub mean: Vec<f32>,
    pub std: Vec<f32>,
    pub count: usize,
}

/// A Compute-panel operation. `Mean`/`Std` reduce a stack of frames from one
/// source (see [`reduce_frames`]); `Diff` is a binary per-pixel difference of
/// two sources' current frames (see [`diff_frames`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reduce {
    Mean,
    Std,
    Diff,
}

impl Reduce {
    pub fn label(self) -> &'static str {
        match self {
            Reduce::Mean => "Mean",
            Reduce::Std => "Std",
            Reduce::Diff => "Diff",
        }
    }

    /// Lowercase token used to round-trip the mode through the view command
    /// (`@compute:<token>:…`). Paired with [`Reduce::from_token`].
    pub fn token(self) -> &'static str {
        match self {
            Reduce::Mean => "mean",
            Reduce::Std => "std",
            Reduce::Diff => "diff",
        }
    }

    /// Parse a [`Reduce::token`] (case-insensitive); `None` if unrecognised.
    pub fn from_token(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mean" => Some(Reduce::Mean),
            "std" => Some(Reduce::Std),
            "diff" => Some(Reduce::Diff),
            _ => None,
        }
    }
}

/// Reduce a stack of same-shape frames to a single frame, per pixel and per
/// channel: the arithmetic **mean** or population **standard deviation**. Frames
/// whose size / channel count differ from the first are skipped. Returns `None`
/// if nothing usable was supplied. The result is always float, so fractional
/// means and small deviations aren't quantised.
pub fn reduce_frames(frames: &[Arc<FrameData>], kind: Reduce) -> Option<FrameData> {
    let first = frames.first()?;
    let size = first.size;
    let ch = first.channels;
    let n = size[0] * size[1] * ch;

    let need_sq = matches!(kind, Reduce::Std);
    let mut sum = vec![0f64; n];
    let mut sumsq = if need_sq { vec![0f64; n] } else { Vec::new() };
    let mut count = 0usize;
    for f in frames {
        if f.size != size || f.channels != ch {
            continue;
        }
        for i in 0..n {
            let v = f.sample_f(i) as f64;
            sum[i] += v;
            if need_sq {
                sumsq[i] += v * v;
            }
        }
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let inv = 1.0 / count as f64;
    let out: Vec<f32> = (0..n)
        .map(|i| {
            let m = sum[i] * inv;
            match kind {
                Reduce::Mean => m as f32,
                Reduce::Std => ((sumsq[i] * inv - m * m).max(0.0)).sqrt() as f32,
                // Diff is a binary op (see `diff_frames`), never a stack reduction.
                Reduce::Diff => m as f32,
            }
        })
        .collect();
    Some(FrameData::new(size, ch, Samples::F32(out)))
}

/// Per-pixel signed difference `a - b` of two same-shape frames, as a float
/// frame so negatives and sub-integer deltas survive. Returns `None` if the
/// frames differ in size or channel count.
pub fn diff_frames(a: &FrameData, b: &FrameData) -> Option<FrameData> {
    if a.size != b.size || a.channels != b.channels {
        return None;
    }
    let n = a.size[0] * a.size[1] * a.channels;
    let out: Vec<f32> = (0..n).map(|i| a.sample_f(i) - b.sample_f(i)).collect();
    Some(FrameData::new(a.size, a.channels, Samples::F32(out)))
}

impl FrameData {
    /// Per-channel histogram binned across the true [min, max] extent.
    pub fn histogram_display(&self, nbins: usize) -> HistData {
        let cc = self.color_channels();
        let px = self.size[0] * self.size[1];

        let (min, max) = self.value_extent();
        let span = (max - min).max(f32::MIN_POSITIVE);
        let last = (nbins - 1) as f32;

        let mut bins = vec![vec![0u32; nbins]; cc];
        for i in 0..px {
            let base = i * self.channels;
            for (c, chan) in bins.iter_mut().enumerate() {
                let s = self.sample_f(base + c);
                if s.is_nan() {
                    continue;
                }
                let bin = (((s - min) / span) * last) as usize;
                chan[bin.min(nbins - 1)] += 1;
            }
        }
        HistData {
            bins,
            min,
            max,
            mono: cc == 1,
        }
    }

    /// Min/max of the colour samples within the pixel rectangle
    /// `[x0, x1) × [y0, y1)` (NaN-skipping). Falls back to the nominal range for
    /// an empty / all-NaN region. Bounds are assumed already clamped to size.
    fn region_extent(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> (f32, f32) {
        let cc = self.color_channels();
        let w = self.size[0];
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for y in y0..y1 {
            for x in x0..x1 {
                let base = (y * w + x) * self.channels;
                for c in 0..cc {
                    let s = self.sample_f(base + c);
                    if s < min {
                        min = s;
                    }
                    if s > max {
                        max = s;
                    }
                }
            }
        }
        if min > max {
            (0.0, self.max_possible() as f32)
        } else {
            (min, max)
        }
    }

    /// Histogram + mean/std over the pixel rectangle `[x0, x1) × [y0, y1)`, for
    /// the region stats panel. The histogram is binned across the region's own
    /// value extent so the tails stay legible.
    pub fn region_stats(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        nbins: usize,
    ) -> RegionStats {
        let cc = self.color_channels();
        let w = self.size[0];
        let (min, max) = self.region_extent(x0, y0, x1, y1);
        let span = (max - min).max(f32::MIN_POSITIVE);
        let last = (nbins - 1) as f32;

        let mut bins = vec![vec![0u32; nbins]; cc];
        let mut sum = vec![0f64; cc];
        let mut sumsq = vec![0f64; cc];
        let mut count = 0usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let base = (y * w + x) * self.channels;
                for c in 0..cc {
                    let s = self.sample_f(base + c);
                    if s.is_nan() {
                        continue;
                    }
                    let bin = (((s - min) / span) * last) as usize;
                    bins[c][bin.min(nbins - 1)] += 1;
                    sum[c] += s as f64;
                    sumsq[c] += (s as f64) * (s as f64);
                }
                count += 1;
            }
        }
        let n = count.max(1) as f64;
        let mean: Vec<f32> = (0..cc).map(|c| (sum[c] / n) as f32).collect();
        let std: Vec<f32> = (0..cc)
            .map(|c| {
                let m = sum[c] / n;
                ((sumsq[c] / n - m * m).max(0.0)).sqrt() as f32
            })
            .collect();
        RegionStats {
            hist: HistData {
                bins,
                min,
                max,
                mono: cc == 1,
            },
            mean,
            std,
            count,
        }
    }

    /// Display bounds derived from a region instead of the whole image: the
    /// region's min/max, or its `percent`% per-tail percentile stretch with
    /// `clip`. Used when a pane's tone is pinned to a right-drag selection.
    /// Values elsewhere in the image that fall outside these bounds are clamped
    /// by the render (that is the whole point — the region drives the contrast,
    /// extremes outside it saturate to black/white).
    pub fn region_display_bounds(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        clip: bool,
        percent: f32,
    ) -> (f32, f32) {
        if clip {
            self.region_percentile_bounds(x0, y0, x1, y1, percent)
        } else {
            self.region_extent(x0, y0, x1, y1)
        }
    }

    /// Region variant of [`FrameData::percentile_bounds`]: the `p`% and
    /// `(100 - p)`% percentile values within the pixel rectangle.
    fn region_percentile_bounds(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        p: f32,
    ) -> (f32, f32) {
        if self.is_float() {
            return self.region_percentile_float(x0, y0, x1, y1, p);
        }
        let full = self.region_extent(x0, y0, x1, y1);
        self.percentile_rect_int(x0, y0, x1, y1, p, full)
    }

    /// Region percentile stretch for float frames (bins across the region's
    /// value extent, mirroring [`FrameData::percentile_bounds_float`]).
    fn region_percentile_float(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        p: f32,
    ) -> (f32, f32) {
        self.percentile_rect_float(x0, y0, x1, y1, p, self.region_extent(x0, y0, x1, y1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{load, save_frame};

    /// Region statistics cover only the selected pixels: mean/std/min/max and
    /// the region-derived tone bounds ignore extremes elsewhere in the image.
    #[test]
    fn region_stats_and_bounds_cover_only_the_region() {
        // 3x1 mono row: a bright outlier, then two mid values.
        //   x=0 -> 255 (outside the region), x=1 -> 10, x=2 -> 20.
        let f = FrameData::new([3, 1], 1, Samples::U8(vec![255, 10, 20]));

        // Region = the last two pixels [1,3) x [0,1).
        let s = f.region_stats(1, 0, 3, 1, 256);
        assert_eq!(s.count, 2);
        assert!(s.hist.mono);
        assert_eq!(s.hist.min, 10.0);
        assert_eq!(s.hist.max, 20.0);
        assert_eq!(s.mean[0], 15.0);
        assert_eq!(s.std[0], 5.0); // population std of {10,20}

        // Linear (no clip) region bounds are the region's own min/max — the
        // bright pixel at x=0 is excluded, so it will clamp to white on render.
        assert_eq!(
            f.region_display_bounds(1, 0, 3, 1, false, 0.01),
            (10.0, 20.0)
        );

        // Whole-image full-range bounds still span the outlier.
        assert_eq!(f.display_bounds(false), (0.0, 255.0));
    }

    /// Reducing a stack of frames yields the per-pixel mean / std, and the
    /// result round-trips through a float TIFF and an 8-bit PNG.
    #[test]
    fn reduce_frames_and_save_roundtrip() {
        // Two 2x1 mono frames: [0,10] and [4,20].
        let a = Arc::new(FrameData::new([2, 1], 1, Samples::U8(vec![0, 10])));
        let b = Arc::new(FrameData::new([2, 1], 1, Samples::U8(vec![4, 20])));

        let mean = reduce_frames(&[a.clone(), b.clone()], Reduce::Mean).expect("mean");
        assert_eq!(mean.color_f32().1, vec![2.0, 15.0]);

        let std = reduce_frames(&[a, b], Reduce::Std).expect("std");
        let sv = std.color_f32().1; // population std of {0,4}=2, {10,20}=5
        assert!((sv[0] - 2.0).abs() < 1e-4 && (sv[1] - 5.0).abs() < 1e-4);

        // Empty input reduces to nothing.
        assert!(reduce_frames(&[], Reduce::Mean).is_none());

        // Per-pixel signed difference, as a float frame (negatives survive).
        let da = FrameData::new([2, 1], 1, Samples::U8(vec![0, 10]));
        let db = FrameData::new([2, 1], 1, Samples::U8(vec![4, 20]));
        assert_eq!(
            diff_frames(&da, &db).expect("diff").color_f32().1,
            vec![-4.0, -10.0]
        );
        // Mismatched shapes don't diff.
        let wide = FrameData::new([3, 1], 1, Samples::U8(vec![0, 0, 0]));
        assert!(diff_frames(&da, &wide).is_none());

        let dir = std::env::temp_dir().join("cim_compute_test");
        let _ = std::fs::create_dir_all(&dir);

        // Float TIFF preserves the fractional values (re-openable, right size).
        let tif = dir.join("mean.tif");
        save_frame(&mean, &tif).expect("save tif");
        assert_eq!(load(&tif).expect("reload tif").size(), [2, 1]);

        // PNG writes the 8-bit view.
        let png = dir.join("mean.png");
        save_frame(&mean, &png).expect("save png");
        assert!(png.exists());

        // Unsupported extension is rejected.
        assert!(save_frame(&mean, &dir.join("mean.gif")).is_err());
    }

    /// The region percentile over the FULL frame must equal the whole-image
    /// percentile — the invariant the planned percentile unification relies
    /// on — plus fixed golden values so a rewrite can't silently drift.
    #[test]
    fn full_frame_region_percentile_matches_whole_image() {
        // Integer path: u8 ramp with outliers at both ends.
        let mut v: Vec<u8> = (0..200).map(|i| 50 + (i % 100) as u8).collect();
        v[0] = 0;
        v[199] = 255;
        let f = FrameData::new([20, 10], 1, Samples::U8(v));
        for p in [0.01f32, 0.5, 2.0, 25.0] {
            assert_eq!(
                f.region_percentile_bounds(0, 0, 20, 10, p),
                f.percentile_bounds(p),
                "u8 p={p}"
            );
        }
        // Golden: 25% per tail of [0, 10, 20, 30] cuts to (10, 20).
        let g = FrameData::new([4, 1], 1, Samples::U8(vec![0, 10, 20, 30]));
        assert_eq!(g.percentile_bounds(25.0), (10.0, 20.0));
        assert_eq!(g.region_percentile_bounds(0, 0, 4, 1, 25.0), (10.0, 20.0));

        // Float path (separate binned implementation).
        let vf: Vec<f32> = (0..200).map(|i| -5.0 + (i % 100) as f32 * 0.25).collect();
        let ff = FrameData::new([20, 10], 1, Samples::F32(vf));
        for p in [0.01f32, 0.5, 2.0, 25.0] {
            assert_eq!(
                ff.region_percentile_float(0, 0, 20, 10, p),
                ff.percentile_bounds_float(p),
                "f32 p={p}"
            );
        }
        // Golden: 25% per tail of [0, 10, 20, 30] cuts to ~(10, 20) (binned).
        let gf = FrameData::new([4, 1], 1, Samples::F32(vec![0.0, 10.0, 20.0, 30.0]));
        let (lo, hi) = gf.percentile_bounds_float(25.0);
        assert!(
            (lo - 10.0).abs() < 0.01 && (hi - 20.0).abs() < 0.01,
            "({lo}, {hi})"
        );
    }
}
