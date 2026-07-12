//! The per-tail percentile histogram scan, shared by whole-image auto-contrast
//! (`render.rs`) and region-driven tone (`stats.rs`). A "full frame" percentile
//! is just the rectangle covering the whole image, so both are the same scan
//! over a pixel rectangle — only the pixel set and the degenerate-case fallback
//! differ, and those are parameters.

use super::FrameData;

/// Float histogram resolution for the arithmetic (non-integer) percentile scan.
const FLOAT_BINS: usize = 4096;

impl FrameData {
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
        let mut hist = vec![0u32; nb];
        let cc = self.color_channels();
        let w = self.size[0];
        let mut total = 0u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let base = (y * w + x) * self.channels;
                for c in 0..cc {
                    hist[self.sample(base + c) as usize] += 1;
                    total += 1;
                }
            }
        }
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
        let mut hist = vec![0u32; FLOAT_BINS];
        let cc = self.color_channels();
        let w = self.size[0];
        let mut total = 0u32;
        for y in y0..y1 {
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
        }
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
