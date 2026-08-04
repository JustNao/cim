//! `FrameData` statistics — histograms, region stats, and the Compute-pane
//! reductions. Purely analytic: nothing here touches decoding, caching or
//! texture rendering.

use std::sync::Arc;

use rayon::prelude::*;

use super::{merge_hist, scan_band, FrameData, Samples, PAR_MIN_SCAN_PX};

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
/// source (see [`reduce_frames`]); `Add`/`Sub` are binary per-pixel operations
/// on two sources' current frames (see [`combine_frames`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reduce {
    Mean,
    Std,
    Add,
    Sub,
}

impl Reduce {
    /// Whether this is a binary (two-source) operation rather than a reduction
    /// over one source's stack of frames.
    pub fn is_binary(self) -> bool {
        matches!(self, Reduce::Add | Reduce::Sub)
    }

    /// The operator sign shown in a binary result's media name (`A + B` /
    /// `A − B`); meaningless for the reductions, which never use it.
    pub fn sign(self) -> &'static str {
        match self {
            Reduce::Sub => "−",
            _ => "+",
        }
    }
    /// Human label for the Compute pickers, in the current UI language. The
    /// [`Self::token`] below is the *stable* form — it round-trips through view
    /// commands and must never be translated.
    pub fn label(self) -> String {
        rust_i18n::t!(format!("compute.reduce_{}", self.token())).into_owned()
    }

    /// Lowercase token used to round-trip the mode through the view command
    /// (`@compute:<token>:…`). Paired with [`Reduce::from_token`].
    pub fn token(self) -> &'static str {
        match self {
            Reduce::Mean => "mean",
            Reduce::Std => "std",
            Reduce::Add => "add",
            Reduce::Sub => "sub",
        }
    }

    /// Parse a [`Reduce::token`] (case-insensitive); `None` if unrecognised.
    pub fn from_token(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mean" => Some(Reduce::Mean),
            "std" => Some(Reduce::Std),
            "add" => Some(Reduce::Add),
            // `diff` is the old name of the (signed) subtraction, kept so an
            // older view command still replays.
            "sub" | "diff" => Some(Reduce::Sub),
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
    // Frames of a different shape contribute nothing, so drop them up front:
    // the accumulation below is then a plain sweep over a uniform stack.
    let stack: Vec<&Arc<FrameData>> = frames
        .iter()
        .filter(|f| f.size == size && f.channels == ch)
        .collect();
    let count = stack.len();
    if count == 0 {
        return None;
    }
    let inv = 1.0 / count as f64;

    // Split by **sample index**, not by frame: each output sample then sums its
    // stack in the same order the serial version did, so the f64 accumulation —
    // which is not associative — reproduces bit for bit. Splitting across frames
    // instead would reorder the adds and could shift the last ulp.
    let sample = |i: usize| {
        let (mut sum, mut sumsq) = (0f64, 0f64);
        for f in &stack {
            let v = f.sample_f(i) as f64;
            sum += v;
            if need_sq {
                sumsq += v * v;
            }
        }
        let m = sum * inv;
        match kind {
            Reduce::Std => ((sumsq * inv - m * m).max(0.0)).sqrt() as f32,
            // Add/Sub are binary ops (see `combine_frames`), never stack
            // reductions; Mean is the sensible fallback.
            _ => m as f32,
        }
    };
    let out: Vec<f32> = if n >= PAR_MIN_SCAN_PX {
        (0..n).into_par_iter().map(sample).collect()
    } else {
        (0..n).map(sample).collect()
    };
    Some(FrameData::new(size, ch, Samples::F32(out)))
}

/// Per-pixel `a + b` / `a − b` of two same-shape frames, as a float frame so
/// negatives and sub-integer results survive. Returns `None` if the frames
/// differ in size or channel count, or for a non-binary `kind`.
pub fn combine_frames(a: &FrameData, b: &FrameData, kind: Reduce) -> Option<FrameData> {
    if a.size != b.size || a.channels != b.channels || !kind.is_binary() {
        return None;
    }
    let n = a.size[0] * a.size[1] * a.channels;
    // Each output sample is an independent function of the two inputs at the
    // same index, so this splits by index with nothing to merge.
    let add = matches!(kind, Reduce::Add);
    let sample = |i: usize| {
        let (x, y) = (a.sample_f(i), b.sample_f(i));
        if add {
            x + y
        } else {
            x - y
        }
    };
    let out: Vec<f32> = if n >= PAR_MIN_SCAN_PX {
        (0..n).into_par_iter().map(sample).collect()
    } else {
        (0..n).map(sample).collect()
    };
    Some(FrameData::new(a.size, a.channels, Samples::F32(out)))
}

impl FrameData {
    /// Per-channel histogram binned across the true [min, max] extent.
    pub fn histogram_display(&self, nbins: usize) -> HistData {
        let cc = self.color_channels();
        let [w, h] = self.size;
        let px = w * h;

        let (min, max) = self.value_extent();
        let span = (max - min).max(f32::MIN_POSITIVE);
        let last = (nbins - 1) as f32;

        // Past `PAR_MIN_SCAN_PX` each band of rows bins into its own accumulator
        // and the counts are summed at the end (`merge_hist`) — exact integer
        // addition, so the result is identical to the serial scan bin for bin.
        let empty = || vec![vec![0u32; nbins]; cc];
        let take = |mut bins: Vec<Vec<u32>>, row: usize| {
            for i in row * w..((row + 1) * w).min(px) {
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
            bins
        };
        let merge = |mut a: Vec<Vec<u32>>, b: Vec<Vec<u32>>| {
            for (ca, cb) in a.iter_mut().zip(&b) {
                merge_hist(ca, cb);
            }
            a
        };
        let bins = if px >= PAR_MIN_SCAN_PX {
            (0..h)
                .into_par_iter()
                .with_min_len(scan_band(h))
                .fold(empty, take)
                .reduce(empty, merge)
        } else {
            (0..h).fold(empty(), take)
        };
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

        // Per-pixel add / signed subtract, as a float frame (negatives survive).
        let da = FrameData::new([2, 1], 1, Samples::U8(vec![0, 10]));
        let db = FrameData::new([2, 1], 1, Samples::U8(vec![4, 20]));
        assert_eq!(
            combine_frames(&da, &db, Reduce::Sub)
                .expect("sub")
                .color_f32()
                .1,
            vec![-4.0, -10.0]
        );
        assert_eq!(
            combine_frames(&da, &db, Reduce::Add)
                .expect("add")
                .color_f32()
                .1,
            vec![4.0, 30.0]
        );
        // Mismatched shapes — and a non-binary kind — don't combine.
        let wide = FrameData::new([3, 1], 1, Samples::U8(vec![0, 0, 0]));
        assert!(combine_frames(&da, &wide, Reduce::Sub).is_none());
        assert!(combine_frames(&da, &db, Reduce::Mean).is_none());
        // `diff` is the old token for the subtraction, still parsed.
        assert_eq!(Reduce::from_token("diff"), Some(Reduce::Sub));

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

    /// Big enough to take the parallel path (`PAR_MIN_SCAN_PX`).
    const DIM: usize = 512;

    /// Run `f` inside a rayon pool of exactly `threads` threads.
    fn with_threads<T: Send>(threads: usize, f: impl Fn() -> T + Sync + Send) -> T {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("build pool")
            .install(f)
    }

    /// A deterministic RGB frame with a different distribution per channel, so a
    /// merge that crossed channels would show up.
    fn rgb_frame() -> FrameData {
        let v: Vec<u16> = (0..DIM * DIM * 3)
            .map(|i| ((i as u64 * 7919 + (i as u64 % 3) * 13) % 65536) as u16)
            .collect();
        FrameData::new([DIM, DIM], 3, Samples::U16(v))
    }

    /// The whole-image histogram — and the value extent it is binned across —
    /// must come out identical however many ways rayon splits the scan. One
    /// thread is the serial fold; wider splits merge per-band accumulators, and
    /// integer counts make that merge exact.
    #[test]
    fn histogram_is_independent_of_the_split() {
        let f = rgb_frame();
        let at = |t| {
            let h = with_threads(t, || f.histogram_display(256));
            (h.bins, h.min, h.max)
        };
        let serial = at(1);
        // Every sample counted exactly once, per channel.
        for chan in &serial.0 {
            assert_eq!(chan.iter().sum::<u32>(), (DIM * DIM) as u32);
        }
        for threads in [2, 3, 4, 8] {
            assert_eq!(at(threads), serial, "{threads} threads");
        }
    }

    /// The stack reduction splits by **sample index**, so each output sample
    /// still sums its frames in the original order. `f64` addition is not
    /// associative, so that ordering is what makes the parallel result equal the
    /// serial one *exactly* — this asserts bit equality, not an epsilon.
    #[test]
    fn stack_reduction_is_independent_of_the_split() {
        // Values whose f64 sums are order-sensitive: a large one alongside small
        // ones, so reordering the adds would actually lose low bits.
        let frames: Vec<Arc<FrameData>> = (0..7)
            .map(|k: usize| {
                let v: Vec<f32> = (0..DIM * DIM)
                    .map(|i| {
                        if (i + k).is_multiple_of(5) {
                            1e9
                        } else {
                            (i % 97) as f32 * 1e-3
                        }
                    })
                    .collect();
                Arc::new(FrameData::new([DIM, DIM], 1, Samples::F32(v)))
            })
            .collect();

        for kind in [Reduce::Mean, Reduce::Std] {
            let at = |t| match with_threads(t, || reduce_frames(&frames, kind).unwrap()).samples {
                Samples::F32(v) => v,
                _ => panic!("reduction is always float"),
            };
            let serial = at(1);
            for threads in [2, 3, 4, 8] {
                assert_eq!(at(threads), serial, "{kind:?}, {threads} threads");
            }
        }
    }

    /// The binary ops are per-sample and independent, so the split can't change
    /// them either — and a mismatched pair is still rejected.
    #[test]
    fn binary_combine_is_independent_of_the_split() {
        let a = FrameData::new(
            [DIM, DIM],
            1,
            Samples::U16((0..DIM * DIM).map(|i| (i % 65536) as u16).collect()),
        );
        let b = FrameData::new(
            [DIM, DIM],
            1,
            Samples::U16((0..DIM * DIM).map(|i| ((i * 3) % 65536) as u16).collect()),
        );

        for kind in [Reduce::Add, Reduce::Sub] {
            let at = |t| match with_threads(t, || combine_frames(&a, &b, kind).unwrap()).samples {
                Samples::F32(v) => v,
                _ => panic!("combine is always float"),
            };
            let serial = at(1);
            // Spot-check against the definition, so this isn't just self-consistent.
            let (x, y) = (a.sample_f(1234), b.sample_f(1234));
            let want = if kind == Reduce::Add { x + y } else { x - y };
            assert_eq!(serial[1234], want);
            for threads in [2, 3, 4, 8] {
                assert_eq!(at(threads), serial, "{kind:?}, {threads} threads");
            }
        }
    }
}
