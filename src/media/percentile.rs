//! The per-tail percentile histogram scan, shared by whole-image auto-contrast
//! (`render.rs`) and region-driven tone (`stats.rs`). A "full frame" percentile
//! is just the rectangle covering the whole image, so both are the same scan
//! over a pixel rectangle — only the pixel set and the degenerate-case fallback
//! differ, and those are parameters.

use rayon::prelude::*;

use super::{merge_hist, scan_band, FrameData, PAR_MIN_SCAN_PX};

/// Float histogram resolution for the arithmetic (non-integer) percentile scan.
pub(super) const FLOAT_BINS: usize = 4096;

/// A whole-image histogram is memoized only when it is at most this fraction of
/// the frame's own sample bytes.
///
/// The table is fixed-size (64 Ki `u32` = 256 KiB for 16-bit data, 1 KiB for
/// 8-bit), so its *relative* cost is entirely a question of how big the image
/// is — and so is the benefit, since the scan it replaces is O(pixels) while the
/// walk is O(bins). Both point the same way: cache it for the large frames where
/// the scan is expensive and the table is noise, skip it for the small ones
/// where the scan is already cheap and the table would be a meaningful addition
/// to a frame the cache budget is accounting for. At 1/16 the worst case is a
/// 6.25% overshoot of `cache_budget_mb`, and only for frames whose bounds have
/// actually been asked for at a non-default percentile.
const HIST_CACHE_RATIO: usize = 16;

impl FrameData {
    /// Fold `take` over the rows of `[x0,x1) × [y0,y1)`, into a `(histogram,
    /// total)` accumulator built by `empty` — serially, or past
    /// [`PAR_MIN_SCAN_PX`] one band of rows per thread with the per-band
    /// histograms summed at the end.
    ///
    /// The counts are integers, so the merge is exact and order-independent: a
    /// parallel scan produces the same histogram — and hence the same
    /// percentile bounds — as the serial one. Both percentile scans below share
    /// this, so the two can't drift apart in how they split.
    ///
    /// A whole-image auto-contrast pass is what makes this worth parallelising;
    /// a small region (a right-drag selection) falls under the threshold and
    /// stays serial.
    fn scan_rows(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        empty: impl Fn() -> (Vec<u32>, u32) + Sync + Send,
        take: impl Fn((Vec<u32>, u32), usize) -> (Vec<u32>, u32) + Sync + Send,
    ) -> (Vec<u32>, u32) {
        let rows = y1.saturating_sub(y0);
        if rows * x1.saturating_sub(x0) < PAR_MIN_SCAN_PX {
            return (y0..y1).fold(empty(), take);
        }
        (y0..y1)
            .into_par_iter()
            .with_min_len(scan_band(rows))
            .fold(&empty, take)
            .reduce(&empty, |(mut a, ta), (b, tb)| {
                merge_hist(&mut a, &b);
                (a, ta + tb)
            })
    }

    /// Integer percentile bounds over the pixel rectangle `[x0,x1) × [y0,y1)`:
    /// the values at the `p`% and `(100-p)`% percentiles via a per-value
    /// histogram. `fallback` is returned for an empty rectangle or when the two
    /// percentiles collapse (whole-image passes the nominal range, a region its
    /// own extent).
    pub(super) fn percentile_rect_int(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        p: f32,
        fallback: (f32, f32),
    ) -> (f32, f32) {
        let nb = self.max_possible() as usize + 1;
        // A whole-image rectangle can reuse the memoized histogram; a region
        // (right-drag stats selection) bins its own pixels, and is small.
        let cached = self
            .covers_frame(x0, y0, x1, y1)
            .then(|| self.full_hist_int());
        let owned;
        let (hist, total) = match &cached {
            Some(Some((h, t))) => (&h[..], *t),
            _ => {
                owned = self.bin_rect_int(x0, y0, x1, y1, nb);
                (&owned.0[..], owned.1)
            }
        };
        if total == 0 {
            return fallback;
        }
        let lo_t = (total as f32 * p / 100.0) as u32;
        let hi_t = (total as f32 * (1.0 - p / 100.0)) as u32;

        let mut cum = 0u32;
        let mut lo = 0usize;
        while lo + 1 < nb {
            cum += hist[lo];
            if cum > lo_t {
                break;
            }
            lo += 1;
        }
        let mut cum = 0u32;
        let mut hi = 0usize;
        while hi + 1 < nb {
            cum += hist[hi];
            if cum >= hi_t {
                break;
            }
            hi += 1;
        }
        if hi <= lo {
            fallback
        } else {
            (lo as f32, hi as f32)
        }
    }

    /// Whether `[x0,x1) × [y0,y1)` is the whole image — the case whose histogram
    /// is memoized, since it is the one recomputed on every repaint.
    fn covers_frame(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> bool {
        x0 == 0 && y0 == 0 && x1 == self.size[0] && y1 == self.size[1]
    }

    /// The per-value histogram of the rectangle's colour samples, and their
    /// count. Split out of [`percentile_rect_int`](Self::percentile_rect_int) so
    /// the *binning* (O(pixels)) can be memoized independently of the *walk*
    /// (O(bins)), which is what the percentile actually varies.
    fn bin_rect_int(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        nb: usize,
    ) -> (Vec<u32>, u32) {
        let cc = self.color_channels();
        let w = self.size[0];
        let empty = || (vec![0u32; nb], 0u32);
        let take = |(mut hist, mut total): (Vec<u32>, u32), y: usize| {
            for x in x0..x1 {
                let base = (y * w + x) * self.channels;
                for c in 0..cc {
                    hist[self.sample(base + c) as usize] += 1;
                    total += 1;
                }
            }
            (hist, total)
        };
        self.scan_rows(x0, y0, x1, y1, empty, take)
    }

    /// The memoized whole-image integer histogram, or `None` for a frame too
    /// small to be worth caching one for (see [`HIST_CACHE_RATIO`]).
    ///
    /// This is the fix for the clip-percentile slider: `bounds_clip` memoizes
    /// only the 0.01% default, so every other value re-binned the entire image
    /// on the UI thread, once per update, while the user dragged. The histogram
    /// is a function of the frame alone, so it is computed once and every
    /// percentile after that is a walk over 64 Ki bins instead of a scan over
    /// however many million samples.
    fn full_hist_int(&self) -> &Option<(Vec<u32>, u32)> {
        self.hist_int.get_or_init(|| {
            let nb = self.max_possible() as usize + 1;
            let [w, h] = self.size;
            (nb * std::mem::size_of::<u32>() <= self.byte_len() / HIST_CACHE_RATIO)
                .then(|| self.bin_rect_int(0, 0, w, h, nb))
        })
    }

    /// The float counterpart of [`bin_rect_int`](Self::bin_rect_int): bin the
    /// rectangle's colour samples across `[min, min + span]`, skipping NaN.
    fn bin_rect_float(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        min: f32,
        span: f32,
    ) -> (Vec<u32>, u32) {
        let last = (FLOAT_BINS - 1) as f32;
        let cc = self.color_channels();
        let w = self.size[0];
        let empty = || (vec![0u32; FLOAT_BINS], 0u32);
        let take = |(mut hist, mut total): (Vec<u32>, u32), y: usize| {
            for x in x0..x1 {
                let base = (y * w + x) * self.channels;
                for c in 0..cc {
                    let s = self.sample_f(base + c);
                    if s.is_nan() {
                        continue;
                    }
                    let b = (((s - min) / span) * last) as usize;
                    hist[b.min(FLOAT_BINS - 1)] += 1;
                    total += 1;
                }
            }
            (hist, total)
        };
        self.scan_rows(x0, y0, x1, y1, empty, take)
    }

    /// The memoized whole-image float histogram, over the frame's own value
    /// extent. Unconditionally worth keeping — [`FLOAT_BINS`] fixes it at 16 KiB
    /// however large the image — so the only `None` here is the degenerate
    /// (empty-extent) frame, which has no bins to walk.
    fn full_hist_float(&self) -> &Option<(Vec<u32>, u32)> {
        self.hist_float.get_or_init(|| {
            let (min, max) = self.value_extent();
            if max <= min {
                return None;
            }
            let [w, h] = self.size;
            Some(self.bin_rect_float(0, 0, w, h, min, max - min))
        })
    }

    /// Float percentile bounds over the pixel rectangle, binned across `extent`
    /// (the rectangle's own min/max — floats can't index a per-value histogram
    /// like integers do). `extent` is returned when it is degenerate or the
    /// percentiles collapse.
    pub(super) fn percentile_rect_float(
        &self,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        p: f32,
        extent: (f32, f32),
    ) -> (f32, f32) {
        let (min, max) = extent;
        if max <= min {
            return (min, max);
        }
        let span = max - min;
        let last = (FLOAT_BINS - 1) as f32;
        // As in the integer path: a whole-image rectangle reuses the memoized
        // histogram, a region bins its own pixels. Binning depends on `extent`
        // as well as the pixels, and the memoized one is built over the frame's
        // own extent — so it is only reusable when this call uses that too.
        let cached = (self.covers_frame(x0, y0, x1, y1) && extent == self.value_extent())
            .then(|| self.full_hist_float());
        let owned;
        let (hist, total) = match &cached {
            Some(Some((h, t))) => (&h[..], *t),
            _ => {
                owned = self.bin_rect_float(x0, y0, x1, y1, min, span);
                (&owned.0[..], owned.1)
            }
        };
        if total == 0 {
            return (min, max);
        }
        let lo_t = (total as f32 * p / 100.0) as u32;
        let hi_t = (total as f32 * (1.0 - p / 100.0)) as u32;

        let bin_val = |b: usize| min + (b as f32 / last) * span;
        let mut cum = 0u32;
        let mut lo = 0usize;
        while lo + 1 < FLOAT_BINS {
            cum += hist[lo];
            if cum > lo_t {
                break;
            }
            lo += 1;
        }
        let mut cum = 0u32;
        let mut hi = 0usize;
        while hi + 1 < FLOAT_BINS {
            cum += hist[hi];
            if cum >= hi_t {
                break;
            }
            hi += 1;
        }
        if hi <= lo {
            (min, max)
        } else {
            (bin_val(lo), bin_val(hi))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Samples;

    /// Big enough that `scan_rows` takes the parallel path (`PAR_MIN_SCAN_PX`).
    const DIM: usize = 512;

    /// Run `f` inside a rayon pool of exactly `threads` threads, so the scan is
    /// forced to split a chosen number of ways.
    fn with_threads<T: Send>(threads: usize, f: impl Fn() -> T + Sync + Send) -> T {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("build pool")
            .install(f)
    }

    /// A whole-image percentile scan must not depend on how many ways rayon
    /// splits it: one thread is the serial fold, and every wider split has to
    /// agree with it. This is the invariant that lets the scan be parallel at
    /// all — the bounds it returns drive auto-contrast, so a split-dependent
    /// answer would make a pane's tone vary run to run.
    #[test]
    fn percentile_scan_is_independent_of_the_split() {
        // A spread of values with distinct tails, so the percentiles are not
        // degenerate and a dropped/double-counted band would move them.
        let ints: Vec<u16> = (0..DIM * DIM)
            .map(|i| ((i as u64 * 7919) % 65536) as u16)
            .collect();
        let floats: Vec<f32> = ints.iter().map(|&v| v as f32 * 0.001).collect();
        let fi = FrameData::new([DIM, DIM], 1, Samples::U16(ints));
        let ff = FrameData::new([DIM, DIM], 1, Samples::F32(floats));

        let int_at = |t| {
            with_threads(t, || {
                fi.percentile_rect_int(0, 0, DIM, DIM, 1.0, (0.0, 1.0))
            })
        };
        let float_at = |t| {
            with_threads(t, || {
                ff.percentile_rect_float(0, 0, DIM, DIM, 1.0, (0.0, 65.535))
            })
        };

        let (serial_int, serial_float) = (int_at(1), float_at(1));
        // A real percentile, not a collapsed fallback — otherwise the equality
        // below would hold trivially.
        assert_ne!(serial_int, (0.0, 1.0));
        assert_ne!(serial_float, (0.0, 65.535));
        for threads in [2, 3, 4, 8] {
            assert_eq!(int_at(threads), serial_int, "int, {threads} threads");
            assert_eq!(float_at(threads), serial_float, "float, {threads} threads");
        }
    }

    /// The parallel scan bins a **region** the same way too — the rows of a
    /// rectangle are a sub-range of the image, so the per-band offsets have to
    /// account for the crop rather than the full width.
    #[test]
    fn percentile_scan_of_a_region_ignores_pixels_outside_it() {
        // Left half all 0, right half all 1000: a scan of the right half only
        // must see 1000s, whatever the split.
        let v: Vec<u16> = (0..DIM * DIM)
            .map(|i| if i % DIM < DIM / 2 { 0 } else { 1000 })
            .collect();
        let f = FrameData::new([DIM, DIM], 1, Samples::U16(v));
        for threads in [1, 2, 4, 8] {
            let got = with_threads(threads, || {
                f.percentile_rect_int(DIM / 2, 0, DIM, DIM, 1.0, (7.0, 7.0))
            });
            // Every sampled value is 1000, so the two percentiles collapse and
            // the fallback comes back — proof the scan saw only the right half.
            assert_eq!(got, (7.0, 7.0), "{threads} threads");
        }
    }

    /// Memoizing the histogram must not change a single bound. The cache stores
    /// the *binning*, which is a function of the frame alone; the percentile
    /// only ever varied the walk. So a frame asked for many percentiles in a row
    /// (a slider drag — the case this exists for) has to give exactly what a
    /// freshly built frame gives for each one, and the first call must not be
    /// privileged over the rest either.
    #[test]
    fn the_memoized_histogram_changes_no_bounds() {
        // 256x256 u8: 64 KiB of samples against a 1 KiB table, so it is over
        // `HIST_CACHE_RATIO` and really is cached (asserted below).
        let vals: Vec<u8> = (0..256 * 256).map(|i| (i * 7 % 251) as u8).collect();
        let reused = FrameData::new([256, 256], 1, Samples::U8(vals.clone()));
        assert!(
            reused.full_hist_int().is_some(),
            "this frame is meant to exercise the cached path"
        );
        for p in [0.0, 0.005, 0.01, 0.1, 0.5, 1.0, 5.0, 12.5, 49.9] {
            // A frame that has never been asked anything: the uncached answer.
            let fresh = FrameData::new([256, 256], 1, Samples::U8(vals.clone()));
            assert_eq!(
                reused.percentile_bounds(p),
                fresh.percentile_bounds(p),
                "p={p}"
            );
        }

        // Same for floats, where the table is a fixed 16 KiB and always kept.
        let fvals: Vec<f32> = (0..64 * 64).map(|i| i as f32 * 0.25 - 300.0).collect();
        let reused = FrameData::new([64, 64], 1, Samples::F32(fvals.clone()));
        assert!(reused.full_hist_float().is_some());
        for p in [0.0, 0.01, 0.2, 2.0, 20.0, 49.9] {
            let fresh = FrameData::new([64, 64], 1, Samples::F32(fvals.clone()));
            assert_eq!(
                reused.percentile_bounds_float(p),
                fresh.percentile_bounds_float(p),
                "p={p}"
            );
        }
    }

    /// The cache is for the **whole image** only. A region percentile bins its
    /// own pixels, so pinning one must not leak into the other in either
    /// direction — which is the bug a "just memoize the histogram" version of
    /// this would have: the region's answer served from the frame's table.
    #[test]
    fn a_region_percentile_ignores_the_whole_image_cache() {
        // Left half 0, right half 1000, as in the region test above.
        let v: Vec<u16> = (0..DIM * DIM)
            .map(|i| if i % DIM < DIM / 2 { 0 } else { 1000 })
            .collect();
        let f = FrameData::new([DIM, DIM], 1, Samples::U16(v));
        // Warm the whole-image path first, so a leak would have something to leak.
        let whole = f.percentile_rect_int(0, 0, DIM, DIM, 1.0, (7.0, 7.0));
        let right = f.percentile_rect_int(DIM / 2, 0, DIM, DIM, 1.0, (7.0, 7.0));
        // The right half is all 1000 -> collapses to the fallback; the whole
        // image spans both values and does not.
        assert_eq!(right, (7.0, 7.0));
        assert_ne!(whole, right);
    }

    /// A frame small enough that the table would be a real share of its
    /// footprint keeps no table — the scan it would save is cheap there anyway,
    /// and the frame cache's byte accounting stays honest. 16x16 u16 is 512 B of
    /// samples against a 256 KiB table.
    #[test]
    fn a_small_frame_caches_no_histogram() {
        let f = FrameData::new([16, 16], 1, Samples::U16(vec![3; 16 * 16]));
        assert!(f.full_hist_int().is_none());
        // And still answers correctly, through the uncached path.
        assert_eq!(f.percentile_bounds(1.0), (0.0, f.max_possible() as f32));
    }
}
