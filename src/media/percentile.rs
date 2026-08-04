//! The per-tail percentile histogram scan, shared by whole-image auto-contrast
//! (`render.rs`) and region-driven tone (`stats.rs`). A "full frame" percentile
//! is just the rectangle covering the whole image, so both are the same scan
//! over a pixel rectangle — only the pixel set and the degenerate-case fallback
//! differ, and those are parameters.

use rayon::prelude::*;

use super::{merge_hist, scan_band, FrameData, PAR_MIN_SCAN_PX};

/// Float histogram resolution for the arithmetic (non-integer) percentile scan.
const FLOAT_BINS: usize = 4096;

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
        let (hist, total) = self.scan_rows(x0, y0, x1, y1, empty, take);
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
        let (hist, total) = self.scan_rows(x0, y0, x1, y1, empty, take);
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
}
