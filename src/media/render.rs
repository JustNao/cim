//! `FrameData` display rendering — the tone-mapping half of the frame type.
//!
//! Everything here is a pure function of `(samples, lo, hi)`: display-bounds /
//! percentile computation, the LUT render (`render_into` and its decimated /
//! gray16 variants), and the mask / intensity overlay tints. No media,
//! caching or decoding concerns — those stay in the parent module.

use super::{FrameData, Samples};

impl FrameData {
    /// Display range [lo, hi] mapped to [0, 255], memoized per mapping. With
    /// `clip`, a fixed 0.01% percentile stretch (robust auto-contrast);
    /// otherwise the full range.
    pub fn display_bounds(&self, clip: bool) -> (f32, f32) {
        let cell = if clip {
            &self.bounds_clip
        } else {
            &self.bounds_full
        };
        *cell.get_or_init(|| self.compute_display_bounds(clip))
    }

    /// Clip bounds at an arbitrary per-tail `percent`. The default (`0.01`) uses
    /// the memoized `display_bounds(true)`; any other percentile is computed
    /// fresh (only when the texture is re-rendered, so it's not per-repaint).
    pub fn clip_bounds(&self, percent: f32) -> (f32, f32) {
        if (percent - 0.01).abs() < 1e-6 {
            self.display_bounds(true)
        } else {
            self.percentile_bounds(percent)
        }
    }

    fn compute_display_bounds(&self, clip: bool) -> (f32, f32) {
        if clip {
            self.percentile_bounds(0.01)
        } else if self.is_float() {
            // Floats have no canonical ceiling; map the actual data extent.
            self.value_extent()
        } else {
            (0.0, self.max_possible() as f32)
        }
    }

    /// Values at the `p`% and `(100 - p)`% percentiles of the colour samples.
    pub(super) fn percentile_bounds(&self, p: f32) -> (f32, f32) {
        if self.is_float() {
            return self.percentile_bounds_float(p);
        }
        let nb = self.max_possible() as usize + 1;
        let mut hist = vec![0u32; nb];
        let cc = self.color_channels();
        let px = self.size[0] * self.size[1];
        let mut total = 0u32;
        for i in 0..px {
            let base = i * self.channels;
            for c in 0..cc {
                hist[self.sample(base + c) as usize] += 1;
                total += 1;
            }
        }
        let full = (0.0, self.max_possible() as f32);
        if total == 0 {
            return full;
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
            full
        } else {
            (lo as f32, hi as f32)
        }
    }

    /// Percentile stretch for float frames: bin across the true value extent
    /// (floats can't index a per-value histogram like integers do).
    pub(super) fn percentile_bounds_float(&self, p: f32) -> (f32, f32) {
        const NB: usize = 4096;
        // value_extent yields finite, ordered bounds (min ≤ max), so a plain
        // comparison is unambiguous here.
        let (min, max) = self.value_extent();
        if max <= min {
            return (min, max);
        }
        let span = max - min;
        let last = (NB - 1) as f32;
        let mut hist = vec![0u32; NB];
        let cc = self.color_channels();
        let px = self.size[0] * self.size[1];
        let mut total = 0u32;
        for i in 0..px {
            let base = i * self.channels;
            for c in 0..cc {
                let s = self.sample_f(base + c);
                if s.is_nan() {
                    continue;
                }
                let b = (((s - min) / span) * last) as usize;
                hist[b.min(NB - 1)] += 1;
                total += 1;
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
        while lo + 1 < NB {
            cum += hist[lo];
            if cum > lo_t {
                break;
            }
            lo += 1;
        }
        let mut cum = 0u32;
        let mut hi = 0usize;
        while hi + 1 < NB {
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

    /// Build the 8-bit RGBA buffer egui uploads as a texture (fresh allocation).
    pub fn render_rgba(&self, clip: bool) -> Vec<u8> {
        let (lo, hi) = self.display_bounds(clip);
        let mut out = Vec::new();
        self.render_into(lo, hi, &mut out);
        out
    }


    /// Render the 8-bit RGBA display buffer into `out` (resized to fit), mapping
    /// native samples through `[lo, hi] → [0, 255]`.
    ///
    /// Integer sources map through a small lookup table keyed by sample value —
    /// one table build (≤ 64 Ki entries) instead of a float multiply-and-clamp
    /// at every pixel — which is the bulk of the per-frame CPU on a large image.
    /// Passing a reusable `out` also avoids re-allocating the buffer each frame.
    pub fn render_into(&self, lo: f32, hi: f32, out: &mut Vec<u8>) {
        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        let cc = self.color_channels();
        out.clear();
        out.resize(px * 4, 255); // alpha stays 255; rgb overwritten below

        // A boolean mask ignores the tone window: false → black, true → white.
        if self.mask {
            match &self.samples {
                Samples::U8(v) => fill_rgba(out, v, ch, cc, px, |s| if s != 0 { 255 } else { 0 }),
                Samples::U16(v) => fill_rgba(out, v, ch, cc, px, |s| if s != 0 { 255 } else { 0 }),
                Samples::F32(v) => {
                    fill_rgba(out, v, ch, cc, px, |s| if s != 0.0 { 255 } else { 0 })
                }
            }
            return;
        }

        let denom = hi - lo;
        let scale = if denom > 0.0 { 255.0 / denom } else { 0.0 };
        let map_f = |s: f32| -> u8 { (((s - lo) * scale).clamp(0.0, 255.0)) as u8 };

        match &self.samples {
            Samples::U8(v) => {
                let lut: Vec<u8> = (0..=u8::MAX).map(|s| map_f(s as f32)).collect();
                fill_rgba(out, v, ch, cc, px, |s| lut[s as usize]);
            }
            Samples::U16(v) => {
                let lut: Vec<u8> = (0..=u16::MAX).map(|s| map_f(s as f32)).collect();
                fill_rgba(out, v, ch, cc, px, |s| lut[s as usize]);
            }
            // Floats have no bounded domain to tabulate; map arithmetically.
            Samples::F32(v) => fill_rgba(out, v, ch, cc, px, map_f),
        }
    }

    /// Render the display RGBA at a **nearest-decimated** resolution — every
    /// `step`-th source pixel along each axis — into `out` (resized to fit),
    /// returning the decimated pixel size `[w', h']`. `step <= 1` is the
    /// full-size render, identical to (and delegating to) [`render_into`].
    ///
    /// A minified pane (physical scale < 1 screen pixel per source pixel) can't
    /// show every source pixel anyway, so building, copying and uploading the
    /// full-resolution texture is wasted work — worst over VNC / software GL,
    /// where the upload is a CPU memcpy and every extra pane multiplies the cost.
    /// Decimation only **drops** whole samples and never blends, so each texel is
    /// still a true source value (the pixel-accuracy invariant holds); the texture
    /// is drawn stretched to the same on-screen rect with NEAREST filtering, as
    /// before. The caller chooses `step` from the pane's zoom (see
    /// `CimApp::stage_step`).
    pub fn render_into_scaled(&self, lo: f32, hi: f32, step: usize, out: &mut Vec<u8>) -> [usize; 2] {
        if step <= 1 {
            self.render_into(lo, hi, out);
            return self.size;
        }
        let [w, h] = self.size;
        let ow = w.div_ceil(step); // ceil: cover the whole image
        let oh = h.div_ceil(step);
        let ch = self.channels;
        let cc = self.color_channels();
        out.clear();
        out.resize(ow * oh * 4, 255); // alpha stays 255; rgb overwritten below

        // A boolean mask ignores the tone window: false → black, true → white.
        if self.mask {
            match &self.samples {
                Samples::U8(v) => fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| if s != 0 { 255 } else { 0 }),
                Samples::U16(v) => fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| if s != 0 { 255 } else { 0 }),
                Samples::F32(v) => fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| if s != 0.0 { 255 } else { 0 }),
            }
            return [ow, oh];
        }

        let denom = hi - lo;
        let scale = if denom > 0.0 { 255.0 / denom } else { 0.0 };
        let map_f = |s: f32| -> u8 { (((s - lo) * scale).clamp(0.0, 255.0)) as u8 };
        match &self.samples {
            Samples::U8(v) => {
                let lut: Vec<u8> = (0..=u8::MAX).map(|s| map_f(s as f32)).collect();
                fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| lut[s as usize]);
            }
            Samples::U16(v) => {
                let lut: Vec<u8> = (0..=u16::MAX).map(|s| map_f(s as f32)).collect();
                fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| lut[s as usize]);
            }
            Samples::F32(v) => fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, map_f),
        }
        [ow, oh]
    }

    /// Render a **single-channel 16-bit** buffer into `out` (resized to
    /// `width*height`), mapping native samples through `[lo, hi] → [0, 65535]`.
    /// This is the input the proprietary operators receive (`crate::imageproc`):
    /// one 16-bit sample per pixel, at genuine 16-bit precision, expanded back to
    /// RGBA (and downscaled to 8 bits) for the texture only after the operators
    /// have run. Only called for single-channel frames (see [`is_op_input`]);
    /// the first channel is taken for any wider source. Mirrors [`render_into`].
    pub fn render_into_gray_u16(&self, lo: f32, hi: f32, out: &mut Vec<u16>) {
        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        out.clear();
        out.resize(px, u16::MAX);

        if self.mask {
            match &self.samples {
                Samples::U8(v) => fill_gray(out, v, ch, px, |s| if s != 0 { u16::MAX } else { 0 }),
                Samples::U16(v) => fill_gray(out, v, ch, px, |s| if s != 0 { u16::MAX } else { 0 }),
                Samples::F32(v) => fill_gray(out, v, ch, px, |s| if s != 0.0 { u16::MAX } else { 0 }),
            }
            return;
        }

        let denom = hi - lo;
        let scale = if denom > 0.0 { 65535.0 / denom } else { 0.0 };
        let map_f = |s: f32| -> u16 { (((s - lo) * scale).clamp(0.0, 65535.0)) as u16 };

        match &self.samples {
            Samples::U8(v) => {
                let lut: Vec<u16> = (0..=u8::MAX).map(|s| map_f(s as f32)).collect();
                fill_gray(out, v, ch, px, |s| lut[s as usize]);
            }
            Samples::U16(v) => {
                let lut: Vec<u16> = (0..=u16::MAX).map(|s| map_f(s as f32)).collect();
                fill_gray(out, v, ch, px, |s| lut[s as usize]);
            }
            Samples::F32(v) => fill_gray(out, v, ch, px, map_f),
        }
    }

    /// Build an RGBA overlay from this mask: true pixels take `rgb` at `alpha`,
    /// false pixels are fully transparent. Used to tint a boolean mask over
    /// another pane. `out` is resized to `w*h*4`.
    pub fn render_mask_rgba(&self, rgb: [u8; 3], alpha: u8, out: &mut Vec<u8>) {
        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        out.clear();
        out.resize(px * 4, 0); // transparent by default
        for i in 0..px {
            if self.sample(i * ch) != 0 {
                let o = i * 4;
                out[o] = rgb[0];
                out[o + 1] = rgb[1];
                out[o + 2] = rgb[2];
                out[o + 3] = alpha;
            }
        }
    }

    /// Build an RGBA overlay from this **single-channel grayscale** frame: every
    /// pixel takes the tint `rgb`, with a per-pixel alpha proportional to its
    /// normalised intensity (through the frame's full display range) scaled by
    /// `alpha`. This generalises [`render_mask_rgba`] to non-mask images — a
    /// boolean mask is just the two-value special case — so any single-channel
    /// image or sequence can tint another pane. `out` is resized to `w*h*4`.
    pub fn render_intensity_rgba(&self, rgb: [u8; 3], alpha: u8, out: &mut Vec<u8>) {
        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        let (lo, hi) = self.display_bounds(false);
        let span = (hi - lo).max(f32::MIN_POSITIVE);
        out.clear();
        out.resize(px * 4, 0); // transparent by default
        for i in 0..px {
            let t = ((self.sample_f(i * ch) - lo) / span).clamp(0.0, 1.0);
            let a = (t * alpha as f32).round() as u8;
            if a != 0 {
                let o = i * 4;
                out[o] = rgb[0];
                out[o + 1] = rgb[1];
                out[o + 2] = rgb[2];
                out[o + 3] = a;
            }
        }
    }
}

/// Write interleaved samples into an RGBA buffer through `map`. Mono sources
/// (1 colour channel) replicate the grey value across R/G/B; alpha is left at
/// whatever `out` already holds (255). `out` must already be `px * 4` long.
fn fill_rgba<T: Copy, U: Copy>(
    out: &mut [U],
    v: &[T],
    ch: usize,
    cc: usize,
    px: usize,
    map: impl Fn(T) -> U,
) {
    for i in 0..px {
        let base = i * ch;
        let o = i * 4;
        if cc == 1 {
            let g = map(v[base]);
            out[o] = g;
            out[o + 1] = g;
            out[o + 2] = g;
        } else {
            out[o] = map(v[base]);
            out[o + 1] = map(v[base + 1]);
            out[o + 2] = map(v[base + 2]);
        }
    }
}

/// Like [`fill_rgba`] but samples every `step`-th source pixel per axis from a
/// `w`-wide source into an `ow × oh` output (both `ceil(dim / step)`). Used by
/// [`FrameData::render_into_scaled`] to build a minified pane's texture at
/// ~display resolution instead of full resolution.
#[allow(clippy::too_many_arguments)]
fn fill_rgba_decimated<T: Copy, U: Copy>(
    out: &mut [U],
    v: &[T],
    w: usize,
    ch: usize,
    cc: usize,
    ow: usize,
    oh: usize,
    step: usize,
    map: impl Fn(T) -> U,
) {
    for oy in 0..oh {
        let row = oy * step * w; // source row of this output row
        for ox in 0..ow {
            let base = (row + ox * step) * ch;
            let o = (oy * ow + ox) * 4;
            if cc == 1 {
                let g = map(v[base]);
                out[o] = g;
                out[o + 1] = g;
                out[o + 2] = g;
            } else {
                out[o] = map(v[base]);
                out[o + 1] = map(v[base + 1]);
                out[o + 2] = map(v[base + 2]);
            }
        }
    }
}

/// Write the first channel of each interleaved pixel into a single-channel
/// buffer through `map`. `out` must already be `px` long.
fn fill_gray<T: Copy, U: Copy>(out: &mut [U], v: &[T], ch: usize, px: usize, map: impl Fn(T) -> U) {
    for i in 0..px {
        out[i] = map(v[i * ch]);
    }
}

#[cfg(test)]
mod tests {
    use crate::media::{FrameData, Samples};


    /// A boolean mask renders as pure black/white regardless of the tone
    /// window, and its overlay buffer tints true pixels while leaving false
    /// pixels transparent.
    #[test]
    fn mask_renders_black_white_and_overlay() {
        // 2x1 mask: [false, true].
        let m = FrameData::new_mask([2, 1], 1, Samples::U8(vec![0, 1]));
        assert!(m.is_mask());

        // Render ignores lo/hi: 0 → black, nonzero → white (alpha 255).
        let mut got = Vec::new();
        m.render_into(1000.0, 2000.0, &mut got);
        assert_eq!(got, vec![0, 0, 0, 255, 255, 255, 255, 255]);

        // Overlay: false → fully transparent; true → rgb at the given alpha.
        let mut ov = Vec::new();
        m.render_mask_rgba([10, 20, 30], 128, &mut ov);
        assert_eq!(ov, vec![0, 0, 0, 0, 10, 20, 30, 128]);
    }

    /// A grayscale single-channel frame overlays by intensity: alpha scales with
    /// the pixel's value across the full display range, times the given alpha.
    #[test]
    fn grayscale_overlay_tints_by_intensity() {
        // 3x1 8-bit gray: min, mid, max → display range [0, 255].
        let f = FrameData::new([3, 1], 1, Samples::U8(vec![0, 128, 255]));
        assert!(!f.is_mask());

        let mut ov = Vec::new();
        f.render_intensity_rgba([10, 20, 30], 200, &mut ov);
        // 0 → transparent; 128/255*200 ≈ 100; 255 → full 200. Tint constant.
        assert_eq!(
            ov,
            vec![0, 0, 0, 0, 10, 20, 30, 100, 10, 20, 30, 200]
        );
    }


    /// The LUT render path must produce exactly what the straightforward
    /// per-pixel float mapping would, for both integer widths and both
    /// mono/RGB layouts, at arbitrary bounds.
    #[test]
    fn lut_render_matches_float_reference() {
        // Reference mapping identical to the pre-LUT implementation.
        fn reference(frame: &FrameData, lo: f32, hi: f32) -> Vec<u8> {
            let denom = hi - lo;
            let scale = if denom > 0.0 { 255.0 / denom } else { 0.0 };
            let map = |s: f32| (((s - lo) * scale).clamp(0.0, 255.0)) as u8;
            let px = frame.size[0] * frame.size[1];
            let cc = if frame.channels >= 3 { 3 } else { 1 };
            let mut out = vec![255u8; px * 4];
            for i in 0..px {
                let base = i * frame.channels;
                for c in 0..3 {
                    let s = frame.sample_f(base + if cc == 1 { 0 } else { c });
                    out[i * 4 + c] = map(s);
                }
            }
            out
        }

        // mono u8, mono u16, and rgb u16, with a non-trivial clip window.
        let mono_u8 = FrameData::new([16, 1], 1, Samples::U8((0..16).cycle().take(16).collect()));
        let mono_u16 = FrameData::new([4, 1], 1, Samples::U16(vec![0, 1000, 30000, 65535]));
        let rgb_u16 = FrameData::new(
            [2, 1],
            3,
            Samples::U16(vec![10, 20000, 60000, 500, 40000, 65535]),
        );

        for (frame, lo, hi) in [
            (&mono_u8, 0.0, 255.0),
            (&mono_u16, 1000.0, 60000.0),
            (&rgb_u16, 400.0, 61000.0),
        ] {
            let mut got = Vec::new();
            frame.render_into(lo, hi, &mut got);
            assert_eq!(got, reference(frame, lo, hi));
        }
    }

    /// Decimated staging: `step == 1` matches the full render exactly, and
    /// `step >= 2` yields a `ceil(dim/step)`-sized buffer whose every texel is a
    /// true source sample (every `step`-th pixel), never a blend of neighbours.
    #[test]
    fn scaled_render_decimates_to_true_samples() {
        // 4x2 mono ramp; display range [0, 255] so a sample maps to itself.
        //   row 0: 0 10 20 30
        //   row 1: 40 50 60 70
        let f = FrameData::new([4, 2], 1, Samples::U8(vec![0, 10, 20, 30, 40, 50, 60, 70]));
        let (lo, hi) = (0.0, 255.0);

        // step 1 is identical to render_into (same size, same bytes).
        let mut full = Vec::new();
        f.render_into(lo, hi, &mut full);
        let mut one = Vec::new();
        assert_eq!(f.render_into_scaled(lo, hi, 1, &mut one), [4, 2]);
        assert_eq!(one, full);

        // step 2 -> ceil(4/2) x ceil(2/2) = 2x1, sampling (0,0) and (2,0): 0, 20.
        let mut half = Vec::new();
        assert_eq!(f.render_into_scaled(lo, hi, 2, &mut half), [2, 1]);
        assert_eq!(half.len(), 2 * 1 * 4);
        assert_eq!([half[0], half[4]], [0, 20]); // grey channels = the source values
        assert_eq!([half[3], half[7]], [255, 255]); // alpha preserved

        // step 3 -> ceil(4/3) x ceil(2/3) = 2x1, sampling (0,0) and (3,0): 0, 30.
        let mut third = Vec::new();
        assert_eq!(f.render_into_scaled(lo, hi, 3, &mut third), [2, 1]);
        assert_eq!([third[0], third[4]], [0, 30]);
    }
}
