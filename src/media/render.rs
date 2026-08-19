//! `FrameData` display rendering — the tone-mapping half of the frame type.
//!
//! Everything here is a pure function of `(samples, lo, hi)`: display-bounds /
//! percentile computation, the LUT render (`render_into` and its decimated /
//! gray16 variants), and the mask / intensity overlay tints. No media,
//! caching or decoding concerns — those stay in the parent module.

use rayon::prelude::*;

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

    /// Render the display RGBA of `region` — a nearest-decimated sub-rect of the
    /// frame — into `out` (resized to fit), reusing `lut` for the value→display
    /// table.
    ///
    /// This is the one RGBA render every live path goes through: the whole image
    /// at full resolution ([`Region::whole`] at step 1), a decimated whole image
    /// (a minified pane — see `CimApp::stage_step`), or the adaptive mode's
    /// viewport region (`app::roi`). Integer sources map through the table (256
    /// or 64 Ki entries), rebuilt only when `(lo, hi, mask)` change, so a run of
    /// frames at a fixed tone reuses one table instead of rebuilding it each
    /// frame; float sources have no bounded domain to tabulate and map
    /// arithmetically, leaving `lut` untouched. A small output skips the 64 Ki
    /// table and maps arithmetically too — cheaper than tabulating for a few
    /// thousand pixels, and bit-identical because the table's entries *are*
    /// [`map_u8`] of their index.
    ///
    /// Decimation only **drops** whole samples and never blends, so every output
    /// texel is still a true source value (the pixel-accuracy invariant holds);
    /// the texture is drawn stretched to the same on-screen rect with NEAREST
    /// filtering. A region renders exactly the sub-rect the same whole-image
    /// render would produce, byte for byte (`region_render_matches_full_subrect`
    /// locks it down), so it can be composed over a whole-image texture without a
    /// visible boundary.
    ///
    /// `lo`/`hi` must be the **pane's** display bounds (whole-image or region
    /// statistics chosen by the tone policy), never bounds derived from the
    /// region itself — a region-local window would change contrast as the view
    /// pans. `region` must lie inside the frame (see [`Region::whole`] and
    /// `app::roi`, which clamps).
    pub fn render_lut<S: RgbaSink>(
        &self,
        lo: f32,
        hi: f32,
        region: Region,
        lut: &mut ToneLut,
        out: &mut S,
    ) {
        let grid = region.grid(self, self.color_channels());
        match &self.samples {
            // A 256-entry table is always cheaper than a per-pixel map.
            Samples::U8(v) => {
                let tab = lut.map8(lo, hi, self.mask, 256);
                fill_lut(out, v, grid, |s| tab[s as usize]);
            }
            Samples::U16(v) if region.texels() < (1 << 16) => {
                if self.mask {
                    fill_lut(out, v, grid, |s| if s != 0 { 255 } else { 0 });
                } else {
                    let map_f = map_u8(lo, hi);
                    fill_lut(out, v, grid, |s| map_f(s as f32));
                }
            }
            Samples::U16(v) => {
                let tab = lut.map8(lo, hi, self.mask, 1 << 16);
                fill_lut(out, v, grid, |s| tab[s as usize]);
            }
            Samples::F32(v) if self.mask => {
                fill_lut(out, v, grid, |s| if s != 0.0 { 255 } else { 0 });
            }
            Samples::F32(v) => {
                let map_f = map_u8(lo, hi);
                fill_lut(out, v, grid, map_f);
            }
        }
    }

    /// [`render_lut`](Self::render_lut) through a colour `palette` (the Colormap
    /// tone): each source sample is toned to an 8-bit index via `[lo, hi]` then
    /// looked up in the palette's 256 entries. Each output texel is still a
    /// single true source sample — only its *colour* comes from the palette, so
    /// pixel-accuracy holds and the readout still reports native values. The
    /// caller ensures the frame is single-channel and non-mask.
    pub fn render_cmap<S: RgbaSink>(
        &self,
        lo: f32,
        hi: f32,
        region: Region,
        palette: crate::palette::Palette,
        lut: &mut ToneLut,
        out: &mut S,
    ) {
        // Mono source (the caller guarantees it): only the first channel is read,
        // so the grid's colour-channel count is 1.
        let grid = region.grid(self, 1);
        let (tab_src, id) = (palette.table(), palette.id());
        match &self.samples {
            Samples::U8(v) => {
                let tab = lut.map_rgb(lo, hi, tab_src, id, 256);
                fill_cmap(out, v, grid, |s| tab[s as usize]);
            }
            Samples::U16(v) if region.texels() < (1 << 16) => {
                let map_f = map_u8(lo, hi);
                fill_cmap(out, v, grid, |s| tab_src[map_f(s as f32) as usize]);
            }
            Samples::U16(v) => {
                let tab = lut.map_rgb(lo, hi, tab_src, id, 1 << 16);
                fill_cmap(out, v, grid, |s| tab[s as usize]);
            }
            Samples::F32(v) => {
                let map_f = map_u8(lo, hi);
                fill_cmap(out, v, grid, |s| tab_src[map_f(s) as usize]);
            }
        }
    }

    /// Render a **single-channel 16-bit** buffer for `region` into `out` (cleared
    /// and refilled with `region.texels()` samples), mapping native samples
    /// through `[lo, hi] → [0, 65535]`.
    ///
    /// This is the input the proprietary operators receive (`crate::imageproc`):
    /// one 16-bit sample per pixel, at genuine 16-bit precision, expanded back to
    /// RGBA (and downscaled to 8 bits) for the texture only after the operators
    /// have run. Only called for single-channel frames (see [`is_op_input`]); the
    /// first channel is taken for any wider source. Under adaptive rendering the
    /// region is cropped and decimated **before** the operators run, so their
    /// output reflects the visible region rather than the whole image — by
    /// design (see `app::roi`).
    pub fn render_gray_u16_lut(
        &self,
        lo: f32,
        hi: f32,
        region: Region,
        lut: &mut ToneLut,
        out: &mut Vec<u16>,
    ) {
        let grid = region.grid(self, 1);
        out.clear();
        out.reserve(region.texels());

        // First channel of each sampled pixel, row-wise over the grid.
        fn fill<T: Copy>(out: &mut Vec<u16>, v: &[T], grid: Grid, map: impl Fn(T) -> u16) {
            for oy in 0..grid.oh {
                out.extend(grid.row(v, oy).map(|s| map(s[0])));
            }
        }

        match &self.samples {
            Samples::U8(v) => {
                let tab = lut.map16(lo, hi, self.mask, 256);
                fill(out, v, grid, |s| tab[s as usize]);
            }
            Samples::U16(v) => {
                let tab = lut.map16(lo, hi, self.mask, 1 << 16);
                fill(out, v, grid, |s| tab[s as usize]);
            }
            Samples::F32(v) if self.mask => {
                fill(out, v, grid, |s| if s != 0.0 { u16::MAX } else { 0 });
            }
            Samples::F32(v) => {
                let map_f = map_u16(lo, hi);
                fill(out, v, grid, map_f);
            }
        }
    }

    /// [`render_lut`](Self::render_lut) over the whole image at full resolution,
    /// reusing `lut`. The common case, spelled out so callers that never crop
    /// don't have to build a [`Region`].
    pub fn render_into_lut<S: RgbaSink>(&self, lo: f32, hi: f32, lut: &mut ToneLut, out: &mut S) {
        self.render_lut(lo, hi, Region::whole(self.size, 1), lut, out);
    }

    /// The display table the GPU tone map indexes (`gpu/tone.wgsl`), one opaque
    /// `0xAABBGGRR` word per entry, appended into `out`.
    ///
    /// Built from the **same** [`ToneLut`] tables the CPU render walks, which is
    /// the whole reason the GPU path can claim to be bit-identical rather than
    /// merely close: for an integer source the entry *is* the mapped display
    /// value of that native sample — mask rule and Colormap palette already
    /// folded in — so the shader does no arithmetic at all and has nothing to
    /// drift from. A float source has no bounded domain to tabulate (the same
    /// reason [`render_lut`](Self::render_lut) maps it
    /// per pixel), so its table is indexed by the toned 8-bit level the shader
    /// computes itself, mirroring [`map_u8`], and supplies only the colour.
    ///
    /// Entry count is therefore the sample domain for integers (256 / 64 Ki) and
    /// a flat 256 for floats. `palette` is the Colormap tone's table and id, as
    /// [`render_cmap`](Self::render_cmap) takes them.
    pub fn tone_table_rgba(
        &self,
        lo: f32,
        hi: f32,
        palette: Option<(&[[u8; 3]; 256], u8)>,
        out: &mut Vec<u32>,
    ) {
        /// Opaque RGB in the byte order an `Rgba8Unorm` texel wants.
        fn pack(c: [u8; 3]) -> u32 {
            0xff00_0000 | ((c[2] as u32) << 16) | ((c[1] as u32) << 8) | c[0] as u32
        }
        let entries = self.tone_table_entries();
        out.clear();
        out.reserve(entries);
        // The table is uploaded and then reused across frames by the GPU cache,
        // so this builder runs only when the tone changes — a throwaway `ToneLut`
        // costs nothing here and keeps the CPU render's own cached table free for
        // the CPU render.
        let mut lut = ToneLut::default();
        match (&self.samples, palette) {
            (Samples::F32(_), Some((pal, _))) => out.extend(pal.iter().map(|&c| pack(c))),
            (Samples::F32(_), None) => out.extend((0..256).map(|g| pack([g as u8; 3]))),
            (_, Some((pal, id))) => out.extend(
                lut.map_rgb(lo, hi, pal, id, entries)
                    .iter()
                    .map(|&c| pack(c)),
            ),
            (_, None) => out.extend(
                lut.map8(lo, hi, self.mask, entries)
                    .iter()
                    .map(|&g| pack([g; 3])),
            ),
        }
    }

    /// How many entries [`tone_table_rgba`](Self::tone_table_rgba) yields for
    /// this frame — the native sample domain for an integer source, or the 256
    /// toned levels for a float one.
    pub fn tone_table_entries(&self) -> usize {
        match &self.samples {
            Samples::U8(_) | Samples::F32(_) => 256,
            Samples::U16(_) => 1 << 16,
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

/// Where a render deposits its display pixels, appended one at a time.
///
/// Two sinks exist: a plain `Vec<u8>` of RGBA bytes (export, and the operator
/// tail's 16-bit expansion) and egui's packed [`Color32`] buffer
/// (`renderer::RgbaSink for Vec<Color32>`). Rendering **straight into** the
/// texture's own pixel type is the point: going through bytes first cost a
/// full-buffer conversion copy (`ColorImage::from_rgba_unmultiplied`) on every
/// frame — on a 4096×3000 image that is ~50 MB read + 50 MB written per render,
/// comparable to the tone map itself.
///
/// Appending (rather than writing into a pre-sized buffer) is also what keeps
/// the buffer from being memset first: `begin` only reserves. Every pixel a
/// render produces is opaque, so alpha never needs a separate pass.
pub trait RgbaSink {
    /// Drop any previous contents and reserve room for `px` pixels.
    fn begin(&mut self, px: usize);
    /// Append one opaque pixel.
    fn push_rgb(&mut self, rgb: [u8; 3]);
    /// Append one opaque grey pixel (mono sources replicate across R/G/B).
    #[inline]
    fn push_gray(&mut self, g: u8) {
        self.push_rgb([g, g, g]);
    }

    /// Append a whole run of grey pixels. Worth overriding: pushing one pixel at
    /// a time updates the buffer's length on every iteration, and that
    /// loop-carried dependency stops the mapping loop from vectorising. A sink
    /// whose pixel type matches the iterator's can hand the run straight to
    /// `Vec::extend`, which writes the run without touching the length until the
    /// end (worth ~2× on a full-resolution render).
    #[inline]
    fn extend_gray<I: Iterator<Item = u8>>(&mut self, it: I) {
        for g in it {
            self.push_gray(g);
        }
    }

    /// [`extend_gray`](Self::extend_gray) for coloured pixels (the Colormap tone).
    #[inline]
    fn extend_rgb<I: Iterator<Item = [u8; 3]>>(&mut self, it: I) {
        for c in it {
            self.push_rgb(c);
        }
    }

    /// Fill from a **parallel** run of grey values — one per output pixel, in
    /// output order — returning whether this sink took it. The default is
    /// `false`: the caller then renders the same run serially, so a sink opts
    /// into parallelism rather than every sink having to support it.
    ///
    /// Only a sink holding exactly one element per pixel can implement this,
    /// because rayon splits the output by index and each task must own a
    /// disjoint, statically-known slice of it. That rules out the RGBA-byte sink
    /// (four elements per pixel) and is why this returns a bool instead of being
    /// required. The one implementor is egui's `Vec<Color32>`
    /// (`renderer::RgbaSink for Vec<Color32>`) — the display render, which is the
    /// path with a frame budget.
    ///
    /// Splitting is by pixel index over a contiguous source, so the mapping is
    /// the *same* per-pixel function the serial path applies, in the same order:
    /// the two outputs are identical by construction, which the pixel-accuracy
    /// invariant requires (`par_matches_serial_render` locks it down).
    fn par_gray<I: rayon::iter::IndexedParallelIterator<Item = u8>>(&mut self, _it: I) -> bool {
        false
    }

    /// [`par_gray`](Self::par_gray) for coloured pixels (the Colormap tone).
    fn par_rgb<I: rayon::iter::IndexedParallelIterator<Item = [u8; 3]>>(&mut self, _it: I) -> bool {
        false
    }
}

/// Output pixels below which a render stays serial: rayon's split/join costs a
/// few microseconds, and a small pane's whole render is not much more than that.
/// A full-resolution pane worth parallelising is orders of magnitude past this,
/// and a decimated one never reaches the parallel path at all.
const PAR_MIN_PX: usize = 1 << 20;

impl RgbaSink for Vec<u8> {
    #[inline]
    fn begin(&mut self, px: usize) {
        self.clear();
        self.reserve(px * 4);
    }
    #[inline]
    fn push_rgb(&mut self, rgb: [u8; 3]) {
        self.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
    }
    // A byte buffer holds four elements per pixel, so there's no run to hand to
    // `extend` — the per-pixel default is what this sink wants.
}

/// The sub-rect and sampling rate one render covers: output texel `(ox, oy)` is
/// source pixel `origin + [ox, oy] * step`.
///
/// One descriptor for all three shapes the live paths render — the whole image
/// (`whole(size, 1)`), a decimated whole image for a minified pane
/// (`whole(size, step)`), and the adaptive mode's viewport region (`app::roi`,
/// which owns the geometry that picks `origin`/`out`). Carrying them together
/// is what lets [`FrameData::render_lut`] and friends be a single function
/// instead of a whole-image and a region copy that must be kept identical.
///
/// The last sampled pixel must lie inside the frame; `whole` guarantees it and
/// `app::roi` clamps for regions (`debug_assert`ed in [`Region::grid`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    /// First source pixel sampled — the region's top-left, in source pixels.
    pub origin: [usize; 2],
    /// Output size in texels.
    pub out: [usize; 2],
    /// Source pixels per output texel, on both axes (1 = full resolution).
    pub step: usize,
}

impl Region {
    /// The whole `size` image nearest-decimated at `step`. `div_ceil` so a
    /// partial last step still lands — the same sizing `app::decode`'s
    /// `decimated_size` reports, which the texture identity depends on.
    pub fn whole(size: [usize; 2], step: usize) -> Self {
        let step = step.max(1);
        Self {
            origin: [0, 0],
            out: [size[0].div_ceil(step), size[1].div_ceil(step)],
            step,
        }
    }

    /// Output texels this region produces.
    pub fn texels(&self) -> usize {
        self.out[0] * self.out[1]
    }

    /// The sampling grid over `frame`, reading `cc` colour channels per pixel
    /// (1 for a mono or operator-input render, else the frame's own count).
    fn grid(&self, frame: &FrameData, cc: usize) -> Grid {
        let [ow, oh] = self.out;
        debug_assert!(ow > 0 && oh > 0);
        debug_assert!(self.origin[0] + (ow - 1) * self.step < frame.size[0]);
        debug_assert!(self.origin[1] + (oh - 1) * self.step < frame.size[1]);
        Grid {
            w: frame.size[0],
            ch: frame.channels,
            cc,
            ow,
            oh,
            step: self.step.max(1),
            origin: self.origin,
        }
    }
}

/// The source→output sampling grid shared by the fill helpers: an `ow × oh`
/// output taking every `step`-th pixel per axis from a `w`-wide interleaved
/// source of `ch` channels, of which `cc` carry colour (1 = mono, replicated),
/// starting at source pixel `origin`. `step == 1`, `origin == [0, 0]` with the
/// output spanning the source is the full-resolution render; a nonzero origin
/// (or an output narrower than the source) is a region render.
#[derive(Clone, Copy)]
struct Grid {
    w: usize,
    ch: usize,
    cc: usize,
    ow: usize,
    oh: usize,
    step: usize,
    origin: [usize; 2],
}

impl Grid {
    /// The source samples of output row `oy`, in output order.
    #[inline]
    fn row<'a, T>(&self, v: &'a [T], oy: usize) -> impl Iterator<Item = &'a [T]> {
        v[((self.origin[1] + oy * self.step) * self.w + self.origin[0]) * self.ch..]
            .chunks_exact(self.ch)
            .step_by(self.step)
            .take(self.ow)
    }

    /// Whether output pixel `i` is source pixel `i` — the whole image as one
    /// contiguous run, which is what the vectorised / parallel fast paths in
    /// [`fill_lut`] / [`fill_cmap`] assume. A decimated or region grid must take
    /// the row-wise path instead.
    #[inline]
    fn contiguous(&self) -> bool {
        self.step == 1 && self.origin == [0, 0] && self.ow == self.w
    }
}

/// Map interleaved samples through `map` into `out`, over `grid`.
///
/// The undecimated cases run output pixel `i` off source pixel `i`, so the whole
/// image is one contiguous run: no per-row setup, no strided iterator, and — past
/// [`PAR_MIN_PX`], for a sink that takes it — split across cores by pixel index
/// (see [`RgbaSink::par_gray`]). A decimated grid stays serial and row-wise: it
/// is already 4× or more cheaper, and its rows aren't a single contiguous span.
fn fill_lut<S: RgbaSink, T: Copy + Sync>(
    out: &mut S,
    v: &[T],
    grid: Grid,
    map: impl Fn(T) -> u8 + Sync,
) {
    let px = grid.ow * grid.oh;
    // The hot path: an undecimated single-channel image, samples 1:1 with pixels.
    if grid.contiguous() && grid.ch == 1 {
        let src = &v[..px];
        if px >= PAR_MIN_PX && out.par_gray(src.par_iter().map(|&s| map(s))) {
            return;
        }
        out.begin(px);
        out.extend_gray(src.iter().map(|&s| map(s)));
        return;
    }
    // Undecimated colour: rows are contiguous, so the image is one strided run.
    if grid.contiguous() && grid.cc == 3 {
        let src = &v[..px * grid.ch];
        let rgb = |s: &[T]| [map(s[0]), map(s[1]), map(s[2])];
        if px >= PAR_MIN_PX && out.par_rgb(src.par_chunks_exact(grid.ch).map(rgb)) {
            return;
        }
        out.begin(px);
        out.extend_rgb(src.chunks_exact(grid.ch).map(rgb));
        return;
    }
    out.begin(px);
    for oy in 0..grid.oh {
        if grid.cc == 1 {
            out.extend_gray(grid.row(v, oy).map(|s| map(s[0])));
        } else {
            out.extend_rgb(grid.row(v, oy).map(|s| [map(s[0]), map(s[1]), map(s[2])]));
        }
    }
}

/// [`fill_lut`] for the Colormap tone: the first channel of each pixel picks a
/// colour (not a grey) through `rgb`. Mono sources only (`grid.cc == 1`).
fn fill_cmap<S: RgbaSink, T: Copy + Sync>(
    out: &mut S,
    v: &[T],
    grid: Grid,
    rgb: impl Fn(T) -> [u8; 3] + Sync,
) {
    let px = grid.ow * grid.oh;
    if grid.contiguous() && grid.ch == 1 {
        let src = &v[..px];
        if px >= PAR_MIN_PX && out.par_rgb(src.par_iter().map(|&s| rgb(s))) {
            return;
        }
        out.begin(px);
        out.extend_rgb(src.iter().map(|&s| rgb(s)));
        return;
    }
    out.begin(px);
    for oy in 0..grid.oh {
        out.extend_rgb(grid.row(v, oy).map(|s| rgb(s[0])));
    }
}

#[cfg(test)]
mod tests {
    use crate::media::{FrameData, Region, Samples};

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
        assert_eq!(ov, vec![0, 0, 0, 0, 10, 20, 30, 100, 10, 20, 30, 200]);
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
        let mut one = Vec::<u8>::new();
        let r = Region::whole([4, 2], 1);
        assert_eq!(r.out, [4, 2]);
        f.render_lut(lo, hi, r, &mut lut, &mut one);
        assert_eq!(one, full);

        // step 2 -> ceil(4/2) x ceil(2/2) = 2x1, sampling (0,0) and (2,0): 0, 20.
        let mut half = Vec::<u8>::new();
        let r = Region::whole([4, 2], 2);
        assert_eq!(r.out, [2, 1]);
        f.render_lut(lo, hi, r, &mut lut, &mut half);
        assert_eq!(half.len(), 2 * 4); // 2x1 RGBA
        assert_eq!([half[0], half[4]], [0, 20]); // grey channels = the source values
        assert_eq!([half[3], half[7]], [255, 255]); // alpha preserved

        // step 3 -> ceil(4/3) x ceil(2/3) = 2x1, sampling (0,0) and (3,0): 0, 30.
        let mut third = Vec::<u8>::new();
        let r = Region::whole([4, 2], 3);
        assert_eq!(r.out, [2, 1]);
        f.render_lut(lo, hi, r, &mut lut, &mut third);
        assert_eq!([third[0], third[4]], [0, 30]);
    }

    /// A reused `ToneLut` renders bit-identically to a throwaway one, reuses the
    /// table across frames at a fixed `(lo,hi)`, and rebuilds when it changes.
    #[test]
    fn tone_lut_caches_and_matches_plain_render() {
        use crate::media::ToneLut;
        // Two "frames" at a fixed tone: bytes must match the uncached render, and
        // the shared table must be reused (same key) across both. Sized past the
        // 64 Ki-texel shortcut, since a smaller output deliberately skips the
        // table and maps arithmetically instead (`render_lut`).
        let px: Vec<u16> = (0..256 * 256).map(|i| (i * 7 % 65536) as u16).collect();
        let a = FrameData::new([256, 256], 1, Samples::U16(px.clone()));
        let b = FrameData::new(
            [256, 256],
            1,
            Samples::U16(px.iter().rev().copied().collect()),
        );
        let (lo, hi) = (1000.0, 60000.0);

        let mut lut = ToneLut::default();
        for f in [&a, &b] {
            let mut plain = Vec::new();
            f.render_into(lo, hi, &mut plain);
            let mut cached = Vec::<u8>::new();
            f.render_lut(lo, hi, Region::whole(f.size, 1), &mut lut, &mut cached);
            assert_eq!(cached, plain, "cached render must equal the plain render");
        }
        // The table was built once and reused (key unchanged across both frames).
        assert_eq!(lut.tab8.len(), 1 << 16);
        assert_eq!(lut.key8, Some((lo.to_bits(), hi.to_bits(), false, 1 << 16)));

        // Changing the window rebuilds the table (new key).
        let mut cached = Vec::<u8>::new();
        a.render_lut(
            500.0,
            40000.0,
            Region::whole(a.size, 1),
            &mut lut,
            &mut cached,
        );
        assert_eq!(
            lut.key8,
            Some((500f32.to_bits(), 40000f32.to_bits(), false, 1 << 16))
        );
        let mut plain = Vec::new();
        a.render_into(500.0, 40000.0, &mut plain);
        assert_eq!(cached, plain);
    }

    /// The small-output micro-win (arithmetic map instead of a 64 Ki table) is
    /// bit-identical to the tabulated mapping. Bit-identity is what lets the
    /// shortcut key off the *output* size alone — a region and the whole-image
    /// render it must match can land on opposite sides of the threshold.
    #[test]
    fn small_output_matches_table() {
        use crate::media::ToneLut;
        // 8x8 u16 ramp; a large step yields a tiny output (< 65536 px) that takes
        // the arithmetic path, which must still equal the table-based mapping.
        let data: Vec<u16> = (0..64).map(|i| (i * 1000) as u16).collect();
        let f = FrameData::new([8, 8], 1, Samples::U16(data));
        let (lo, hi) = (0.0, 63000.0);

        let mut lut = ToneLut::default();
        let mut scaled = Vec::<u8>::new();
        let r = Region::whole([8, 8], 4);
        assert_eq!(r.out, [2, 2]); // 4 px < 65536 → arithmetic path
        f.render_lut(lo, hi, r, &mut lut, &mut scaled);

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

    /// A region render is byte-identical to the matching sub-rect of the full
    /// render at the same `step` — the invariant that lets a region texture sit
    /// over the whole-image texture without a visible boundary. Exercised over
    /// every sample type, mono and RGB, steps 1 and 2, and regions touching the
    /// image edges.
    #[test]
    fn region_render_matches_full_subrect() {
        use crate::media::ToneLut;

        // Extract the RGBA bytes of an `[ow, oh]` output-space sub-rect at
        // `(ox0, oy0)` from a full render `full` of output width `fw`.
        fn subrect(
            full: &[u8],
            fw: usize,
            ox0: usize,
            oy0: usize,
            ow: usize,
            oh: usize,
        ) -> Vec<u8> {
            let mut out = Vec::new();
            for oy in 0..oh {
                let start = ((oy0 + oy) * fw + ox0) * 4;
                out.extend_from_slice(&full[start..start + ow * 4]);
            }
            out
        }

        // 7x5 frames of each sample type (odd sizes so step 2 clips at edges).
        let px = 7 * 5;
        let frames = [
            FrameData::new(
                [7, 5],
                1,
                Samples::U8((0..px as u8 * 3).step_by(3).collect()),
            ),
            FrameData::new(
                [7, 5],
                1,
                Samples::U16((0..px).map(|i| (i * 1873) as u16).collect()),
            ),
            FrameData::new(
                [7, 5],
                1,
                Samples::F32((0..px).map(|i| i as f32 * 0.37 - 2.0).collect()),
            ),
            FrameData::new(
                [7, 5],
                3,
                Samples::U16((0..px * 3).map(|i| (i * 613) as u16).collect()),
            ),
        ];
        let (lo, hi) = (3.0, 40.0);

        for f in &frames {
            for step in [1usize, 2] {
                let mut lut = ToneLut::default();
                let mut full = Vec::<u8>::new();
                let [fw, fh] = Region::whole(f.size, step).out;
                f.render_lut(lo, hi, Region::whole(f.size, step), &mut lut, &mut full);

                // Interior region, plus regions pinned to each far edge.
                for (ox0, oy0) in [(0, 0), (1, 1), (fw - 2, fh - 2)] {
                    let (ow, oh) = (2.min(fw - ox0), 2.min(fh - oy0));
                    let mut region = Vec::<u8>::new();
                    let r = Region {
                        origin: [ox0 * step, oy0 * step],
                        out: [ow, oh],
                        step,
                    };
                    f.render_lut(lo, hi, r, &mut lut, &mut region);
                    assert_eq!(
                        region,
                        subrect(&full, fw, ox0, oy0, ow, oh),
                        "step {step} region at ({ox0},{oy0})"
                    );
                }
            }
        }
    }

    /// [`region_render_matches_full_subrect`] for the Colormap tone and the
    /// 16-bit operator-input region render.
    #[test]
    fn region_cmap_and_gray_match_full() {
        use crate::media::ToneLut;
        use crate::palette::Palette;

        let px = 7 * 5;
        let f = FrameData::new(
            [7, 5],
            1,
            Samples::U16((0..px).map(|i| (i * 1873) as u16).collect()),
        );
        let (lo, hi) = (100.0, 60000.0);
        let pal = Palette::Viridis;

        for step in [1usize, 2] {
            let mut lut = ToneLut::default();
            let mut full = Vec::<u8>::new();
            let [fw, fh] = Region::whole(f.size, step).out;
            f.render_cmap(
                lo,
                hi,
                Region::whole(f.size, step),
                pal,
                &mut lut,
                &mut full,
            );
            let mut region = Vec::<u8>::new();
            let (ow, oh) = (fw - 1, fh - 1);
            let r = Region {
                origin: [step, step],
                out: [ow, oh],
                step,
            };
            f.render_cmap(lo, hi, r, pal, &mut lut, &mut region);
            let mut want = Vec::new();
            for oy in 0..oh {
                let start = ((1 + oy) * fw + 1) * 4;
                want.extend_from_slice(&full[start..start + ow * 4]);
            }
            assert_eq!(region, want, "cmap step {step}");
        }

        // Gray u16: the step-1 full render is the reference; a step-2 region
        // must pick every other sample of it.
        let mut lut = ToneLut::default();
        let mut full16 = Vec::new();
        f.render_gray_u16_lut(lo, hi, Region::whole(f.size, 1), &mut lut, &mut full16);
        let mut region16 = Vec::new();
        let r = Region {
            origin: [1, 1],
            out: [3, 2],
            step: 2,
        };
        f.render_gray_u16_lut(lo, hi, r, &mut lut, &mut region16);
        let mut want16 = Vec::new();
        for oy in 0..2 {
            for ox in 0..3 {
                want16.push(full16[(1 + oy * 2) * 7 + 1 + ox * 2]);
            }
        }
        assert_eq!(region16, want16);
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
        let mut out = Vec::<u8>::new();
        let size = Region::whole(f.size, 1).out;
        f.render_cmap(
            0.0,
            255.0,
            Region::whole(f.size, 1),
            pal,
            &mut lut,
            &mut out,
        );
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
        let mut flat = Vec::<u8>::new();
        f.render_cmap(5.0, 5.0, Region::whole(f.size, 1), pal, &mut lut, &mut flat);
        assert_eq!([flat[0], flat[1], flat[2]], [flat[4], flat[5], flat[6]]);
        assert_eq!([flat[4], flat[5], flat[6]], [flat[8], flat[9], flat[10]]);
    }
}
