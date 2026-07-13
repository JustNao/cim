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

    /// Values at the `p`% and `(100 - p)`% percentiles of the colour samples
    /// over the whole image (auto-contrast) — the full-frame case of the shared
    /// percentile scan. Integer sources fall back to the nominal range.
    pub(super) fn percentile_bounds(&self, p: f32) -> (f32, f32) {
        if self.is_float() {
            return self.percentile_bounds_float(p);
        }
        let [w, h] = self.size;
        self.percentile_rect_int(0, 0, w, h, p, (0.0, self.max_possible() as f32))
    }

    /// The float-frame case of [`FrameData::percentile_bounds`]: bin across the
    /// whole image's value extent.
    pub(super) fn percentile_bounds_float(&self, p: f32) -> (f32, f32) {
        let [w, h] = self.size;
        self.percentile_rect_float(0, 0, w, h, p, self.value_extent())
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
    /// Convenience wrapper over [`render_into_lut`](Self::render_into_lut) with a
    /// throwaway table — use that directly (passing a reused [`ToneLut`]) on any
    /// per-frame path so a fixed-tone run doesn't rebuild the ≤ 64 Ki-entry LUT
    /// every frame (the bulk of per-frame CPU on a large image).
    pub fn render_into(&self, lo: f32, hi: f32, out: &mut Vec<u8>) {
        self.render_into_lut(lo, hi, &mut ToneLut::default(), out);
    }

    /// Render the 8-bit RGBA display buffer into `out`, reusing `lut` for the
    /// value→display table. Integer sources map through the table (256 or 64 Ki
    /// entries), rebuilt only when `(lo, hi, mask)` change — so a run of frames at
    /// a fixed tone reuses one table instead of rebuilding it each frame. Float
    /// sources have no bounded domain to tabulate and map arithmetically (the
    /// `lut` is left untouched).
    pub fn render_into_lut(&self, lo: f32, hi: f32, lut: &mut ToneLut, out: &mut Vec<u8>) {
        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        let cc = self.color_channels();
        out.clear();
        out.resize(px * 4, 255); // alpha stays 255; rgb overwritten below

        match &self.samples {
            // The table folds in the mask rule (0 → black, else white) and the
            // linear map, so both integer paths are one table look-up per pixel.
            Samples::U8(v) => {
                let tab = lut.map8(lo, hi, self.mask, 256);
                fill_rgba(out, v, ch, cc, px, |s| tab[s as usize]);
            }
            Samples::U16(v) => {
                let tab = lut.map8(lo, hi, self.mask, 1 << 16);
                fill_rgba(out, v, ch, cc, px, |s| tab[s as usize]);
            }
            Samples::F32(v) if self.mask => {
                fill_rgba(out, v, ch, cc, px, |s| if s != 0.0 { 255 } else { 0 })
            }
            Samples::F32(v) => {
                let map_f = map_u8(lo, hi);
                fill_rgba(out, v, ch, cc, px, map_f)
            }
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
    /// `CimApp::stage_step`) and passes a reused `lut` (see
    /// [`render_into_lut`](Self::render_into_lut)); a small decimated output skips
    /// the table entirely and maps arithmetically, which is cheaper than building
    /// a 64 Ki-entry LUT for a few thousand pixels.
    pub fn render_into_scaled_lut(
        &self,
        lo: f32,
        hi: f32,
        step: usize,
        lut: &mut ToneLut,
        out: &mut Vec<u8>,
    ) -> [usize; 2] {
        if step <= 1 {
            self.render_into_lut(lo, hi, lut, out);
            return self.size;
        }
        let [w, h] = self.size;
        let ow = w.div_ceil(step); // ceil: cover the whole image
        let oh = h.div_ceil(step);
        let ch = self.channels;
        let cc = self.color_channels();
        out.clear();
        out.resize(ow * oh * 4, 255); // alpha stays 255; rgb overwritten below
        let opx = ow * oh;

        match &self.samples {
            // 256-entry table is always cheaper than a per-pixel map.
            Samples::U8(v) => {
                let tab = lut.map8(lo, hi, self.mask, 256);
                fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| tab[s as usize]);
            }
            // For a small decimated output, building a 64 Ki table costs more than
            // mapping each output pixel arithmetically — so skip the table then.
            Samples::U16(v) if opx < (1 << 16) => {
                if self.mask {
                    fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| if s != 0 { 255 } else { 0 });
                } else {
                    let map_f = map_u8(lo, hi);
                    fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| map_f(s as f32));
                }
            }
            Samples::U16(v) => {
                let tab = lut.map8(lo, hi, self.mask, 1 << 16);
                fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| tab[s as usize]);
            }
            Samples::F32(v) if self.mask => {
                fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, |s| if s != 0.0 { 255 } else { 0 });
            }
            Samples::F32(v) => {
                let map_f = map_u8(lo, hi);
                fill_rgba_decimated(out, v, w, ch, cc, ow, oh, step, map_f);
            }
        }
        [ow, oh]
    }

    /// Render the display RGBA of a **mono** frame through a colour `palette`
    /// (the Colormap tone): each source sample is toned to an 8-bit index via
    /// `[lo, hi]` then looked up in the 256-entry palette. Nearest-decimated at
    /// `step` like [`render_into_scaled_lut`](Self::render_into_scaled_lut) — each
    /// output texel is still a single true source sample, only its *colour* comes
    /// from the palette, so pixel-accuracy holds (the readout still reads native
    /// values). The caller ensures the frame is single-channel and non-mask.
    #[allow(clippy::too_many_arguments)]
    pub fn render_into_scaled_cmap(
        &self,
        lo: f32,
        hi: f32,
        step: usize,
        palette: &[[u8; 3]; 256],
        palette_id: u8,
        lut: &mut ToneLut,
        out: &mut Vec<u8>,
    ) -> [usize; 2] {
        let [w, h] = self.size;
        let ch = self.channels;
        let (ow, oh) = if step <= 1 {
            (w, h)
        } else {
            (w.div_ceil(step), h.div_ceil(step))
        };
        out.clear();
        out.resize(ow * oh * 4, 255);
        let decim = step > 1;
        let opx = ow * oh;

        // `rgb` yields the colour for one source sample; `write` fans it out over
        // the full or decimated grid (the fill helpers below).
        match &self.samples {
            Samples::U8(v) => {
                let tab = lut.map_rgb(lo, hi, palette, palette_id, 256);
                let rgb = |s: u8| tab[s as usize];
                if decim {
                    fill_rgb_decimated(out, v, w, ch, ow, oh, step, rgb);
                } else {
                    fill_rgb(out, v, ch, w * h, rgb);
                }
            }
            // Small decimated output maps arithmetically (skip the 64 Ki table).
            Samples::U16(v) if decim && opx < (1 << 16) => {
                let map_f = map_u8(lo, hi);
                fill_rgb_decimated(out, v, w, ch, ow, oh, step, |s| palette[map_f(s as f32) as usize]);
            }
            Samples::U16(v) => {
                let tab = lut.map_rgb(lo, hi, palette, palette_id, 1 << 16);
                let rgb = |s: u16| tab[s as usize];
                if decim {
                    fill_rgb_decimated(out, v, w, ch, ow, oh, step, rgb);
                } else {
                    fill_rgb(out, v, ch, w * h, rgb);
                }
            }
            Samples::F32(v) => {
                let map_f = map_u8(lo, hi);
                let rgb = |s: f32| palette[map_f(s) as usize];
                if decim {
                    fill_rgb_decimated(out, v, w, ch, ow, oh, step, rgb);
                } else {
                    fill_rgb(out, v, ch, w * h, rgb);
                }
            }
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
    pub fn render_into_gray_u16_lut(&self, lo: f32, hi: f32, lut: &mut ToneLut, out: &mut Vec<u16>) {
        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        out.clear();
        out.resize(px, u16::MAX);

        match &self.samples {
            Samples::U8(v) => {
                let tab = lut.map16(lo, hi, self.mask, 256);
                fill_gray(out, v, ch, px, |s| tab[s as usize]);
            }
            Samples::U16(v) => {
                let tab = lut.map16(lo, hi, self.mask, 1 << 16);
                fill_gray(out, v, ch, px, |s| tab[s as usize]);
            }
            Samples::F32(v) if self.mask => {
                fill_gray(out, v, ch, px, |s| if s != 0.0 { u16::MAX } else { 0 })
            }
            Samples::F32(v) => {
                let map_f = map_u16(lo, hi);
                fill_gray(out, v, ch, px, map_f)
            }
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

/// The arithmetic `[lo, hi] → [0, 255]` map, shared by the float path and by
/// [`ToneLut`]'s table build so a tabulated and a per-pixel render map identically.
#[inline]
fn map_u8(lo: f32, hi: f32) -> impl Fn(f32) -> u8 {
    let denom = hi - lo;
    let scale = if denom > 0.0 { 255.0 / denom } else { 0.0 };
    move |s: f32| (((s - lo) * scale).clamp(0.0, 255.0)) as u8
}

/// The arithmetic `[lo, hi] → [0, 65535]` map (16-bit operator input path).
#[inline]
fn map_u16(lo: f32, hi: f32) -> impl Fn(f32) -> u16 {
    let denom = hi - lo;
    let scale = if denom > 0.0 { 65535.0 / denom } else { 0.0 };
    move |s: f32| (((s - lo) * scale).clamp(0.0, 65535.0)) as u16
}

/// A cached value→display lookup table, rebuilt only when its key
/// `(lo, hi, mask, entries)` changes. A long run of frames at a fixed tone reuses
/// one table instead of rebuilding a 64 Ki-entry LUT each frame — the dominant
/// per-frame CPU cost on a large integer image. Owned per pane by the render path
/// (`stage` for cheap panes, `renderer::Worker` for heavy ones); float sources
/// don't tabulate and leave it untouched. Holds an 8-bit table (RGBA render) and a
/// 16-bit table (operator input) independently, each self-keyed so switching paths
/// only rebuilds the one in use.
#[derive(Default)]
pub struct ToneLut {
    key8: Option<(u32, u32, bool, usize)>,
    tab8: Vec<u8>,
    key16: Option<(u32, u32, bool, usize)>,
    tab16: Vec<u16>,
    key_rgb: Option<(u32, u32, u8, usize)>,
    tab_rgb: Vec<[u8; 3]>,
}

impl ToneLut {
    /// The 8-bit table over `[0, entries)` sample values: `mask` folds in the
    /// black/white rule (0 → 0, else 255), otherwise the linear `[lo,hi]` map.
    fn map8(&mut self, lo: f32, hi: f32, mask: bool, entries: usize) -> &[u8] {
        let key = (lo.to_bits(), hi.to_bits(), mask, entries);
        if self.key8 != Some(key) {
            self.tab8.clear();
            self.tab8.reserve(entries);
            if mask {
                self.tab8
                    .extend((0..entries).map(|s| if s != 0 { 255u8 } else { 0 }));
            } else {
                let map_f = map_u8(lo, hi);
                self.tab8.extend((0..entries).map(|s| map_f(s as f32)));
            }
            self.key8 = Some(key);
        }
        &self.tab8
    }

    /// The per-value RGB table for the Colormap tone: each sample value is toned
    /// to an 8-bit index (`map_u8`) and looked up in `palette`. Keyed on
    /// `(lo, hi, palette_id, entries)` so a fixed palette/window reuses it.
    fn map_rgb(
        &mut self,
        lo: f32,
        hi: f32,
        palette: &[[u8; 3]; 256],
        palette_id: u8,
        entries: usize,
    ) -> &[[u8; 3]] {
        let key = (lo.to_bits(), hi.to_bits(), palette_id, entries);
        if self.key_rgb != Some(key) {
            let map_f = map_u8(lo, hi);
            self.tab_rgb.clear();
            self.tab_rgb.reserve(entries);
            self.tab_rgb
                .extend((0..entries).map(|s| palette[map_f(s as f32) as usize]));
            self.key_rgb = Some(key);
        }
        &self.tab_rgb
    }

    /// The 16-bit counterpart of [`map8`](Self::map8) (operator input range).
    fn map16(&mut self, lo: f32, hi: f32, mask: bool, entries: usize) -> &[u16] {
        let key = (lo.to_bits(), hi.to_bits(), mask, entries);
        if self.key16 != Some(key) {
            self.tab16.clear();
            self.tab16.reserve(entries);
            if mask {
                self.tab16
                    .extend((0..entries).map(|s| if s != 0 { u16::MAX } else { 0 }));
            } else {
                let map_f = map_u16(lo, hi);
                self.tab16.extend((0..entries).map(|s| map_f(s as f32)));
            }
            self.key16 = Some(key);
        }
        &self.tab16
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

/// Write a per-pixel RGB triple (from the first channel of each interleaved
/// pixel) into an RGBA buffer; alpha is left at whatever `out` holds (255). Used
/// by the Colormap tone. `out` must already be `px * 4` long.
fn fill_rgb<T: Copy>(out: &mut [u8], v: &[T], ch: usize, px: usize, rgb: impl Fn(T) -> [u8; 3]) {
    for i in 0..px {
        let c = rgb(v[i * ch]);
        let o = i * 4;
        out[o] = c[0];
        out[o + 1] = c[1];
        out[o + 2] = c[2];
    }
}

/// Decimated [`fill_rgb`]: samples every `step`-th source pixel per axis from a
/// `w`-wide source into an `ow × oh` output.
#[allow(clippy::too_many_arguments)]
fn fill_rgb_decimated<T: Copy>(
    out: &mut [u8],
    v: &[T],
    w: usize,
    ch: usize,
    ow: usize,
    oh: usize,
    step: usize,
    rgb: impl Fn(T) -> [u8; 3],
) {
    for oy in 0..oh {
        let row = oy * step * w;
        for ox in 0..ow {
            let c = rgb(v[(row + ox * step) * ch]);
            let o = (oy * ow + ox) * 4;
            out[o] = c[0];
            out[o + 1] = c[1];
            out[o + 2] = c[2];
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
        let mut lut = super::ToneLut::default();

        // step 1 is identical to render_into (same size, same bytes).
        let mut full = Vec::new();
        f.render_into(lo, hi, &mut full);
        let mut one = Vec::new();
        assert_eq!(f.render_into_scaled_lut(lo, hi, 1, &mut lut, &mut one), [4, 2]);
        assert_eq!(one, full);

        // step 2 -> ceil(4/2) x ceil(2/2) = 2x1, sampling (0,0) and (2,0): 0, 20.
        let mut half = Vec::new();
        assert_eq!(f.render_into_scaled_lut(lo, hi, 2, &mut lut, &mut half), [2, 1]);
        assert_eq!(half.len(), 2 * 1 * 4);
        assert_eq!([half[0], half[4]], [0, 20]); // grey channels = the source values
        assert_eq!([half[3], half[7]], [255, 255]); // alpha preserved

        // step 3 -> ceil(4/3) x ceil(2/3) = 2x1, sampling (0,0) and (3,0): 0, 30.
        let mut third = Vec::new();
        assert_eq!(f.render_into_scaled_lut(lo, hi, 3, &mut lut, &mut third), [2, 1]);
        assert_eq!([third[0], third[4]], [0, 30]);
    }

    /// A reused `ToneLut` renders bit-identically to a throwaway one, reuses the
    /// table across frames at a fixed `(lo,hi)`, and rebuilds when it changes.
    #[test]
    fn tone_lut_caches_and_matches_plain_render() {
        use crate::media::ToneLut;
        // Two "frames" at a fixed tone: bytes must match the uncached render, and
        // the shared table must be reused (same key) across both.
        let a = FrameData::new([4, 1], 1, Samples::U16(vec![0, 1000, 30000, 65535]));
        let b = FrameData::new([4, 1], 1, Samples::U16(vec![65535, 30000, 1000, 0]));
        let (lo, hi) = (1000.0, 60000.0);

        let mut lut = ToneLut::default();
        for f in [&a, &b] {
            let mut plain = Vec::new();
            f.render_into(lo, hi, &mut plain);
            let mut cached = Vec::new();
            f.render_into_lut(lo, hi, &mut lut, &mut cached);
            assert_eq!(cached, plain, "cached render must equal the plain render");
        }
        // The table was built once and reused (key unchanged across both frames).
        assert_eq!(lut.tab8.len(), 1 << 16);
        assert_eq!(lut.key8, Some((lo.to_bits(), hi.to_bits(), false, 1 << 16)));

        // Changing the window rebuilds the table (new key).
        let mut cached = Vec::new();
        a.render_into_lut(500.0, 40000.0, &mut lut, &mut cached);
        assert_eq!(lut.key8, Some((500f32.to_bits(), 40000f32.to_bits(), false, 1 << 16)));
        let mut plain = Vec::new();
        a.render_into(500.0, 40000.0, &mut plain);
        assert_eq!(cached, plain);
    }

    /// The decimated small-output micro-win (arithmetic map instead of a 64 Ki
    /// table) is bit-identical to the tabulated full render at `step 1`.
    #[test]
    fn scaled_lut_small_output_matches_table() {
        use crate::media::ToneLut;
        // 8x8 u16 ramp; a large step yields a tiny output (< 65536 px) that takes
        // the arithmetic path, which must still equal the table-based mapping.
        let data: Vec<u16> = (0..64).map(|i| (i * 1000) as u16).collect();
        let f = FrameData::new([8, 8], 1, Samples::U16(data));
        let (lo, hi) = (0.0, 63000.0);

        let mut lut = ToneLut::default();
        let mut scaled = Vec::new();
        let size = f.render_into_scaled_lut(lo, hi, 4, &mut lut, &mut scaled);
        assert_eq!(size, [2, 2]); // 4 px < 65536 → arithmetic path

        // Reference: same decimation, but forced through the table via render_into.
        let mut reference = ToneLut::default();
        let full_tab = reference.map8(lo, hi, false, 1 << 16).to_vec();
        // Output texels sample source pixels (0,0),(4,0),(0,4),(4,4).
        for (ox, oy, x, y) in [(0, 0, 0, 0), (1, 0, 4, 0), (0, 1, 0, 4), (1, 1, 4, 4)] {
            let value = ((y * 8 + x) * 1000) as usize;
            let o = (oy * 2 + ox) * 4;
            assert_eq!(scaled[o], full_tab[value]);
        }
    }

    /// The Colormap render maps each sample through its toned index into the
    /// palette (RGB, not grey), and a flat window yields a constant colour.
    #[test]
    fn colormap_render_maps_through_palette() {
        use crate::media::ToneLut;
        use crate::palette::Palette;
        let pal = Palette::Viridis;
        let tab = pal.table();

        // A 3-pixel mono ramp min/mid/max over the window [0, 255].
        let f = FrameData::new([3, 1], 1, Samples::U8(vec![0, 128, 255]));
        let mut lut = ToneLut::default();
        let mut out = Vec::new();
        let size = f.render_into_scaled_cmap(0.0, 255.0, 1, tab, pal.id(), &mut lut, &mut out);
        assert_eq!(size, [3, 1]);
        // Each pixel's RGB is palette[value], with alpha preserved (255).
        for (i, &v) in [0u8, 128, 255].iter().enumerate() {
            let o = i * 4;
            assert_eq!([out[o], out[o + 1], out[o + 2]], tab[v as usize]);
            assert_eq!(out[o + 3], 255);
        }
        // Endpoints are the palette ends and differ (it's a real colour ramp).
        assert_eq!([out[0], out[1], out[2]], tab[0]);
        assert_ne!([out[0], out[1], out[2]], [out[8], out[9], out[10]]);

        // A flat window (lo == hi) collapses every sample to one palette colour.
        let mut flat = Vec::new();
        f.render_into_scaled_cmap(5.0, 5.0, 1, tab, pal.id(), &mut lut, &mut flat);
        assert_eq!([flat[0], flat[1], flat[2]], [flat[4], flat[5], flat[6]]);
        assert_eq!([flat[4], flat[5], flat[6]], [flat[8], flat[9], flat[10]]);
    }
}
