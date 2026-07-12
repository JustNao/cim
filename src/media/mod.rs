//! Media sources: still images and multi-page TIFF sequences.
//!
//! Frames keep their **original** samples (8- or 16-bit, 1/3/4 channels) so the
//! UI can report true pixel values and histograms at native bit depth. The
//! 8-bit RGBA needed for display is derived on demand in [`FrameData::render_rgba`].
//!
//! Decoding runs on the background pool (see `decoder.rs`), so the pieces that
//! pool needs are exposed here: a stateless [`decode_tiff_page`] plus cache
//! accessors ([`Media::resident`] / [`Media::insert`]).
//!
//! Video (mp4/avi) will slot in later as another `Media` variant behind the
//! same interface.

mod render;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Context, Result};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::encoder::{colortype, TiffEncoder};
use tiff::ColorType;

/// Original interleaved samples, at native bit depth.
pub enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

/// A single decoded frame at native bit depth.
pub struct FrameData {
    pub size: [usize; 2], // [width, height]
    pub channels: usize,  // 1 (gray), 3 (rgb) or 4 (rgba)
    pub samples: Samples,
    /// Display bounds are content-invariant per frame, so memoize them the
    /// first time each mapping is needed (full-range vs 0.01% clip) — the clip
    /// path otherwise re-scans the whole image on every redraw.
    bounds_full: OnceLock<(f32, f32)>,
    bounds_clip: OnceLock<(f32, f32)>,
    /// Decoded from a 1-bit bilevel TIFF: a boolean mask. Rendered as pure
    /// black/white (false/true) rather than through the tone mapping, and
    /// available to tint another pane as an overlay.
    mask: bool,
}

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

/// A small neutral still, used as the initial image of a fresh Compute pane
/// before a source has been chosen / reduced.
pub fn placeholder_frame() -> FrameData {
    FrameData::new([64, 64], 1, Samples::U8(vec![40; 64 * 64]))
}

/// Write a single frame to disk. `.tif`/`.tiff` preserves the native values as a
/// 32-bit float TIFF (mono or RGB); `.png`/`.jpg`/`.jpeg` writes the 8-bit
/// display rendering (native range mapped to `[0, 255]`), dropping any alpha.
pub fn save_frame(frame: &FrameData, path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let [w, h] = frame.size;
    match ext.as_str() {
        "tif" | "tiff" => {
            let mut file =
                File::create(path).with_context(|| format!("create {}", path.display()))?;
            let mut enc = TiffEncoder::new(&mut file)?;
            let (cc, data) = frame.color_f32();
            if cc == 1 {
                enc.write_image::<colortype::Gray32Float>(w as u32, h as u32, &data)?;
            } else {
                enc.write_image::<colortype::RGB32Float>(w as u32, h as u32, &data)?;
            }
            Ok(())
        }
        "png" | "jpg" | "jpeg" => {
            let rgba = frame.render_rgba(false);
            let mut rgb = Vec::with_capacity(w * h * 3);
            for px in rgba.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            image::save_buffer(path, &rgb, w as u32, h as u32, image::ColorType::Rgb8)
                .with_context(|| format!("save {}", path.display()))?;
            Ok(())
        }
        other => Err(anyhow!(
            "unsupported format '.{other}' — use .tif, .png or .jpg"
        )),
    }
}

impl FrameData {
    pub fn new(size: [usize; 2], channels: usize, samples: Samples) -> Self {
        Self {
            size,
            channels,
            samples,
            bounds_full: OnceLock::new(),
            bounds_clip: OnceLock::new(),
            mask: false,
        }
    }

    /// Like [`FrameData::new`] but flagged as a boolean mask (1-bit source).
    pub fn new_mask(size: [usize; 2], channels: usize, samples: Samples) -> Self {
        let mut f = Self::new(size, channels, samples);
        f.mask = true;
        f
    }

    /// Extract the axis-aligned sub-rectangle `[x, y, w, h]` (clamped to the
    /// frame) as a new **independent** frame — same sample type / channel count /
    /// mask flag, but its own memoized bounds. The export uses this to run the
    /// whole tone pipeline (LUT bounds + LUT render + the proprietary operators)
    /// on **only** the cropped region instead of the full image.
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> FrameData {
        let sw = self.size[0];
        let x0 = x.min(sw);
        let y0 = y.min(self.size[1]);
        let x1 = (x0 + w).min(sw);
        let y1 = (y0 + h).min(self.size[1]);
        let (cw, ch) = (x1 - x0, y1 - y0);
        let n = self.channels;
        // Copy each output row as one contiguous slice of the source row.
        macro_rules! rows {
            ($v:expr) => {{
                let mut out = Vec::with_capacity(cw * ch * n);
                for row in y0..y1 {
                    let s = (row * sw + x0) * n;
                    out.extend_from_slice(&$v[s..s + cw * n]);
                }
                out
            }};
        }
        let samples = match &self.samples {
            Samples::U8(v) => Samples::U8(rows!(v)),
            Samples::U16(v) => Samples::U16(rows!(v)),
            Samples::F32(v) => Samples::F32(rows!(v)),
        };
        let mut f = FrameData::new([cw, ch], n, samples);
        f.mask = self.mask;
        f
    }

    /// True when this frame is a boolean mask (decoded from a 1-bit TIFF).
    pub fn is_mask(&self) -> bool {
        self.mask
    }

    /// Bytes held by the sample buffer, for the cache memory budget.
    pub fn byte_len(&self) -> usize {
        let n = self.size[0] * self.size[1] * self.channels;
        n * match self.samples {
            Samples::U8(_) => 1,
            Samples::U16(_) => 2,
            Samples::F32(_) => 4,
        }
    }

    #[inline]
    pub fn sample(&self, idx: usize) -> u32 {
        match &self.samples {
            Samples::U8(v) => v[idx] as u32,
            Samples::U16(v) => v[idx] as u32,
            Samples::F32(v) => v[idx] as u32, // fallback; float paths use sample_f
        }
    }

    /// Native value of one sample as `f32` (float-aware).
    #[inline]
    fn sample_f(&self, idx: usize) -> f32 {
        match &self.samples {
            Samples::U8(v) => v[idx] as f32,
            Samples::U16(v) => v[idx] as f32,
            Samples::F32(v) => v[idx],
        }
    }

    #[inline]
    fn is_float(&self) -> bool {
        matches!(self.samples, Samples::F32(_))
    }

    /// 16-bit unsigned samples — the underlying sample format the proprietary
    /// operators require.
    #[inline]
    pub fn is_u16(&self) -> bool {
        matches!(self.samples, Samples::U16(_))
    }

    /// A **single-channel 16-bit** frame — the only input the proprietary
    /// operators (LUT_ALPHA / DETAILS_ENHANCED) accept. Their availability is
    /// gated on this (plus a loaded library): they receive one 16-bit sample per
    /// pixel, not an interleaved RGBA buffer.
    #[inline]
    pub fn is_op_input(&self) -> bool {
        self.channels == 1 && self.is_u16()
    }

    /// Short native-format label for the footer readout (`uint8` / `uint16` /
    /// `float32`).
    pub fn kind_label(&self) -> &'static str {
        match self.samples {
            Samples::U8(_) => "uint8",
            Samples::U16(_) => "uint16",
            Samples::F32(_) => "float32",
        }
    }

    /// More than 8 bits per sample (16-bit or float) → clip-on-load default.
    pub fn hi_depth(&self) -> bool {
        !matches!(self.samples, Samples::U8(_))
    }

    /// Largest representable value for the sample type (255 or 65535). For
    /// floats there is no fixed maximum, so display code uses the data extent
    /// instead — this returns the nominal scene-linear ceiling of `1.0`.
    pub fn max_possible(&self) -> u32 {
        match self.samples {
            Samples::U8(_) => 255,
            Samples::U16(_) => 65535,
            Samples::F32(_) => 1,
        }
    }

    /// Actual [min, max] of the colour samples, in native units (NaN-skipping).
    /// Falls back to the nominal range for empty / all-NaN frames.
    fn value_extent(&self) -> (f32, f32) {
        let cc = self.color_channels();
        let px = self.size[0] * self.size[1];
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for i in 0..px {
            let base = i * self.channels;
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
        if min > max {
            (0.0, self.max_possible() as f32)
        } else {
            (min, max)
        }
    }

    /// The colour samples as interleaved `f32`, alpha excluded: returns the
    /// colour-channel count (1 or 3) and a `w*h*cc` buffer. Used to write a
    /// computed frame out as a float TIFF.
    pub fn color_f32(&self) -> (usize, Vec<f32>) {
        let cc = self.color_channels();
        let px = self.size[0] * self.size[1];
        let mut out = Vec::with_capacity(px * cc);
        for i in 0..px {
            let base = i * self.channels;
            for c in 0..cc {
                out.push(self.sample_f(base + c));
            }
        }
        (cc, out)
    }

    /// Channels that carry colour (alpha excluded) — used for stats.
    fn color_channels(&self) -> usize {
        if self.channels >= 3 {
            3
        } else {
            1
        }
    }

    /// Native-value readout of one pixel, e.g. `14273` or `R 201 G 198 B 195`.
    pub fn pixel_string(&self, x: usize, y: usize) -> String {
        let base = (y * self.size[0] + x) * self.channels;
        if self.is_float() {
            if self.color_channels() == 1 {
                format!("{:.4}", self.sample_f(base))
            } else {
                format!(
                    "R {:.4} G {:.4} B {:.4}",
                    self.sample_f(base),
                    self.sample_f(base + 1),
                    self.sample_f(base + 2)
                )
            }
        } else if self.color_channels() == 1 {
            format!("{}", self.sample(base))
        } else {
            format!(
                "R {} G {} B {}",
                self.sample(base),
                self.sample(base + 1),
                self.sample(base + 2)
            )
        }
    }

    /// Mean native intensity of one pixel across its colour channels (alpha
    /// excluded) — the scalar value plotted along the line-profile graph. Mono
    /// frames return the single sample; colour frames the average of R/G/B.
    pub fn intensity_at(&self, x: usize, y: usize) -> f32 {
        let base = (y * self.size[0] + x) * self.channels;
        let cc = self.color_channels();
        let mut sum = 0.0;
        for c in 0..cc {
            sum += self.sample_f(base + c);
        }
        sum / cc as f32
    }

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
        let full = self.region_extent(x0, y0, x1, y1);
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
        const NB: usize = 4096;
        let (min, max) = self.region_extent(x0, y0, x1, y1);
        if max <= min {
            return (min, max);
        }
        let span = max - min;
        let last = (NB - 1) as f32;
        let mut hist = vec![0u32; NB];
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
                    hist[b.min(NB - 1)] += 1;
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
}

pub enum Media {
    Still(Still),
    TiffSeq(TiffSeq),
    FileSeq(FileSeq),
    ConcatSeq(ConcatSeq),
}

/// A concatenation's frame files and its discovered `frame → (file, page)` map.
pub type ConcatLayout = (Vec<PathBuf>, Vec<(usize, usize)>);

/// How the background pool should decode one frame of a sequence.
pub enum DecodeReq {
    /// Seek `page` of the multi-page TIFF at `path`, via a reader keyed by
    /// (pane id, `file`). A lone `TiffSeq` uses `file = 0` and `page = frame`;
    /// a `ConcatSeq` uses `file` to pick which TIFF in the run and `page` the
    /// page within it.
    ///
    /// `probe` = a **metadata-only** frontier probe: seek to the page and read
    /// just whether it exists (its IFD), without decoding pixels. Used to
    /// fast-forward lazy length discovery during a seek so the intervening pages
    /// aren't decompressed — only the landed target frame is.
    Tiff {
        file: usize,
        page: usize,
        path: PathBuf,
        probe: bool,
    },
    /// Decode this standalone file — one frame of a numbered still sequence.
    File(PathBuf),
}

pub struct Still {
    name: String,
    frame: Arc<FrameData>,
    hi_depth: bool,
}

/// Frame residency plus LRU / memory-budget bookkeeping, shared by both
/// sequence kinds (multi-page TIFF and numbered image files). Slots may be
/// evicted back to `None` to stay within the cache budget without changing the
/// known length.
struct SeqCache {
    /// One slot per known frame; `None` = not resident (never decoded or evicted).
    cache: Vec<Option<Arc<FrameData>>>,
    /// Recency tick per frame, parallel to `cache`; the budget evicts the
    /// least-recently-used resident frames first.
    last_used: Vec<u64>,
    /// Running total of resident sample bytes (sum of `byte_len` over `Some`s).
    resident_bytes: usize,
}

impl SeqCache {
    /// A cache of `len` not-yet-resident frames.
    fn new(len: usize) -> Self {
        Self {
            cache: vec![None; len],
            last_used: vec![0; len],
            resident_bytes: 0,
        }
    }

    fn len(&self) -> usize {
        self.cache.len()
    }

    fn resident(&self, idx: usize) -> Option<Arc<FrameData>> {
        self.cache.get(idx).and_then(|slot| slot.clone())
    }

    /// Store a decoded frame. Replacing an existing slot re-accounts its bytes;
    /// inserting exactly at the end (`idx == len`) grows the known length by one
    /// (how a TIFF frontier probe discovers the next page). Out-of-range inserts
    /// are ignored.
    fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        if idx < self.cache.len() {
            if let Some(old) = &self.cache[idx] {
                self.resident_bytes -= old.byte_len();
            }
            self.resident_bytes += frame.byte_len();
            self.cache[idx] = Some(frame);
        } else if idx == self.cache.len() {
            self.resident_bytes += frame.byte_len();
            self.cache.push(Some(frame));
            self.last_used.push(0);
        }
    }

    /// Grow the known length by one **empty** (non-resident) slot — how a
    /// metadata-only frontier probe records that a page exists without decoding
    /// it. Only advances at the frontier (`idx == len`); anything else is
    /// ignored (a real decode fills interior slots via `insert`).
    fn note_len(&mut self, idx: usize) {
        if idx == self.cache.len() {
            self.cache.push(None);
            self.last_used.push(0);
        }
    }

    fn touch(&mut self, idx: usize, clock: u64) {
        if let Some(u) = self.last_used.get_mut(idx) {
            *u = clock;
        }
    }

    fn evict(&mut self, idx: usize) {
        if let Some(slot) = self.cache.get_mut(idx) {
            if let Some(old) = slot.take() {
                self.resident_bytes -= old.byte_len();
            }
        }
    }

    fn resident_frames(&self) -> Vec<(usize, u64, usize)> {
        self.cache
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|f| (i, self.last_used[i], f.byte_len())))
            .collect()
    }

    fn resident_count(&self) -> usize {
        self.cache.iter().filter(|f| f.is_some()).count()
    }
}

pub struct TiffSeq {
    name: String,
    path: PathBuf,
    size: [usize; 2],
    hi_depth: bool,
    /// Page 0 is 1-bit bilevel → this is a boolean-mask sequence.
    is_mask: bool,
    /// Frames known so far. Grows lazily as later pages are decoded — we never
    /// walk the whole file to learn its length.
    frames: SeqCache,
    /// Set once a probe past `frames.len()` found no more pages: the real end.
    at_end: bool,
}

/// A sequence whose frames are individual numbered image files (one file per
/// frame) — e.g. `frame_000.png … frame_011.png`, given on the command line as
/// a compact `PREFIX%0Xu SUFFIX,START,END` token. Unlike a TIFF its length is
/// known up front (the file list), so there is no lazy discovery and it is
/// always "at end".
pub struct FileSeq {
    name: String,
    paths: Vec<PathBuf>,
    size: [usize; 2],
    hi_depth: bool,
    frames: SeqCache,
}

/// Several multi-page TIFFs presented as **one** continuous timeline: when
/// `movie_000.tif` runs out of pages the timeline rolls straight into
/// `movie_001.tif`, and so on. Opened from a compact `PREFIX%0Xu.tif,…` token.
///
/// Page counts per file aren't known up front (a TIFF's length is discovered
/// lazily, §4), so the global length grows one page at a time: the frontier
/// probe walks pages within the current file, and when a probe finds no page it
/// rolls over to the next file rather than ending the sequence — only the last
/// file's end is the real end.
pub struct ConcatSeq {
    name: String,
    files: Vec<PathBuf>,
    size: [usize; 2],
    hi_depth: bool,
    frames: SeqCache,
    /// Global frame → (file index, page within that file). `map.len()` always
    /// equals `frames.len()` (the known length).
    map: Vec<(usize, usize)>,
    /// The next (file, page) the frontier probe will try — not yet in `map`.
    disc_file: usize,
    disc_page: usize,
    /// Set once the *last* file has been exhausted: the real end.
    at_end: bool,
}

impl Media {
    /// Wrap an in-memory frame as an always-resident still (e.g. a computed
    /// image). Not backed by a file.
    pub fn still(name: String, frame: FrameData) -> Media {
        let hi_depth = frame.hi_depth();
        Media::Still(Still {
            name,
            frame: Arc::new(frame),
            hi_depth,
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Media::Still(s) => &s.name,
            Media::TiffSeq(t) => &t.name,
            Media::FileSeq(f) => &f.name,
            Media::ConcatSeq(c) => &c.name,
        }
    }

    pub fn frame_count(&self) -> usize {
        match self {
            Media::Still(_) => 1,
            Media::TiffSeq(t) => t.frames.len(),
            Media::FileSeq(f) => f.frames.len(),
            Media::ConcatSeq(c) => c.frames.len(),
        }
    }

    /// Whether this is a multi-frame sequence (not a single still). A multi-page
    /// TIFF counts even before its length is discovered (`frame_count` starts at
    /// 1), since it decodes and plays like a sequence.
    pub fn is_sequence(&self) -> bool {
        !matches!(self, Media::Still(_))
    }

    pub fn size(&self) -> [usize; 2] {
        match self {
            Media::Still(s) => s.frame.size,
            Media::TiffSeq(t) => t.size,
            Media::FileSeq(f) => f.size,
            Media::ConcatSeq(c) => c.size,
        }
    }

    /// More than 8 bits per sample → clip-on-load is a sensible default.
    pub fn hi_depth(&self) -> bool {
        match self {
            Media::Still(s) => s.hi_depth,
            Media::TiffSeq(t) => t.hi_depth,
            Media::FileSeq(f) => f.hi_depth,
            Media::ConcatSeq(c) => c.hi_depth,
        }
    }

    /// A boolean-mask media (1-bit bilevel TIFF): rendered black/white and
    /// available to tint another pane as an overlay. Only TIFFs are masks.
    pub fn is_mask(&self) -> bool {
        matches!(self, Media::TiffSeq(t) if t.is_mask)
    }

    pub fn resident(&self, idx: usize) -> Option<Arc<FrameData>> {
        match self {
            Media::Still(s) => Some(s.frame.clone()),
            Media::TiffSeq(t) => t.frames.resident(idx),
            Media::FileSeq(f) => f.frames.resident(idx),
            Media::ConcatSeq(c) => c.frames.resident(idx),
        }
    }

    /// How the pool should decode frame `idx`, or `None` for an always-resident
    /// still. A TIFF page seeks in the persistent per-id reader; a numbered
    /// still sequence decodes that frame's own file; a concatenation maps the
    /// global frame to (file, page) — or, at the frontier, probes the next page.
    pub fn decode_job(&self, idx: usize) -> Option<DecodeReq> {
        self.job(idx, false)
    }

    /// A **metadata-only** frontier probe for frame `idx` (TIFF-backed media
    /// only): confirms the page exists without decoding its pixels, so a seek
    /// can pass it cheaply. `None` for stills / numbered still runs (a still
    /// run's length is known up front, so it never needs probing).
    pub fn probe_job(&self, idx: usize) -> Option<DecodeReq> {
        self.job(idx, true)
    }

    fn job(&self, idx: usize, probe: bool) -> Option<DecodeReq> {
        match self {
            Media::Still(_) => None,
            Media::TiffSeq(t) => Some(DecodeReq::Tiff {
                file: 0,
                page: idx,
                path: t.path.clone(),
                probe,
            }),
            // A numbered still run knows its length up front, so it is only ever
            // decoded, never probed.
            Media::FileSeq(f) if !probe => f.paths.get(idx).cloned().map(DecodeReq::File),
            Media::FileSeq(_) => None,
            Media::ConcatSeq(c) => c.job(idx, probe),
        }
    }

    pub fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        match self {
            Media::Still(_) => {}
            Media::TiffSeq(t) => t.frames.insert(idx, frame),
            Media::FileSeq(f) => f.frames.insert(idx, frame),
            Media::ConcatSeq(c) => c.insert(idx, frame),
        }
    }

    /// Mark frame `idx` as used at `clock`, so the budget evicts it last.
    pub fn touch(&mut self, idx: usize, clock: u64) {
        match self {
            Media::Still(_) => {}
            Media::TiffSeq(t) => t.frames.touch(idx, clock),
            Media::FileSeq(f) => f.frames.touch(idx, clock),
            Media::ConcatSeq(c) => c.frames.touch(idx, clock),
        }
    }

    /// Drop a resident frame to reclaim memory. The known length is unchanged;
    /// the frame simply re-decodes on demand if shown again.
    pub fn evict(&mut self, idx: usize) {
        match self {
            Media::Still(_) => {}
            Media::TiffSeq(t) => t.frames.evict(idx),
            Media::FileSeq(f) => f.frames.evict(idx),
            Media::ConcatSeq(c) => c.frames.evict(idx),
        }
    }

    /// Sample bytes currently resident, for the memory budget.
    pub fn resident_bytes(&self) -> usize {
        match self {
            Media::Still(s) => s.frame.byte_len(),
            Media::TiffSeq(t) => t.frames.resident_bytes,
            Media::FileSeq(f) => f.frames.resident_bytes,
            Media::ConcatSeq(c) => c.frames.resident_bytes,
        }
    }

    /// Resident frames as `(frame index, last-used tick, byte size)`. Stills
    /// return empty — their single frame is always needed and never evicted.
    pub fn resident_frames(&self) -> Vec<(usize, u64, usize)> {
        match self {
            Media::Still(_) => Vec::new(),
            Media::TiffSeq(t) => t.frames.resident_frames(),
            Media::FileSeq(f) => f.frames.resident_frames(),
            Media::ConcatSeq(c) => c.frames.resident_frames(),
        }
    }

    /// True once we've confirmed there is no page beyond what we already know.
    /// Always true for a still or a numbered still sequence (length is known);
    /// discovered lazily for a TIFF or a concatenation.
    pub fn at_end(&self) -> bool {
        match self {
            Media::Still(_) | Media::FileSeq(_) => true,
            Media::TiffSeq(t) => t.at_end,
            Media::ConcatSeq(c) => c.at_end,
        }
    }

    /// A frontier probe found no page where one was expected. A `TiffSeq` has
    /// reached its real end; a `ConcatSeq` has finished the current file and
    /// rolls over to the next (only the last file's end is the real end).
    pub fn frontier_ended(&mut self) {
        match self {
            Media::TiffSeq(t) => t.at_end = true,
            Media::ConcatSeq(c) => c.roll_to_next_file(),
            _ => {}
        }
    }

    /// A **metadata-only** frontier probe confirmed a page exists at `idx`
    /// without decoding it: grow the known length by one empty (non-resident)
    /// slot so a seek can pass it. The frame decodes on demand when actually
    /// shown. Only advances at the frontier (`idx == len`).
    pub fn note_frontier(&mut self, idx: usize) {
        match self {
            Media::TiffSeq(t) => t.frames.note_len(idx),
            Media::ConcatSeq(c) => c.note_frontier(idx),
            _ => {}
        }
    }

    pub fn resident_count(&self) -> usize {
        match self {
            Media::Still(_) => 1,
            Media::TiffSeq(t) => t.frames.resident_count(),
            Media::FileSeq(f) => f.frames.resident_count(),
            Media::ConcatSeq(c) => c.frames.resident_count(),
        }
    }

    /// For a concatenation, the file list plus the discovered global→(file,page)
    /// map, so an export can snapshot it. `None` for other media.
    pub fn concat_layout(&self) -> Option<ConcatLayout> {
        match self {
            Media::ConcatSeq(c) => Some((c.files.clone(), c.map.clone())),
            _ => None,
        }
    }
}

impl ConcatSeq {
    /// Map global frame `idx` to a decode (or, with `probe`, a metadata-only)
    /// request. Frames already in `map` resolve directly; the frontier
    /// (`idx == map.len()`) probes the next (file, page) to discover.
    fn job(&self, idx: usize, probe: bool) -> Option<DecodeReq> {
        let (file, page) = if idx < self.map.len() {
            self.map[idx]
        } else if idx == self.map.len() {
            (self.disc_file, self.disc_page)
        } else {
            return None;
        };
        self.files.get(file).map(|path| DecodeReq::Tiff {
            file,
            page,
            path: path.clone(),
            probe,
        })
    }

    fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        if idx == self.frames.len() {
            self.advance_map();
        }
        self.frames.insert(idx, frame);
    }

    /// A metadata-only frontier probe confirmed the next global frame exists
    /// (still within the current file): record where it lives and grow the
    /// known length by one empty slot, without decoding its pixels.
    fn note_frontier(&mut self, idx: usize) {
        if idx == self.frames.len() {
            self.advance_map();
            self.frames.note_len(idx);
        }
    }

    /// Frontier confirmed: record where this global frame lives and step the
    /// probe to the next page of the same file.
    fn advance_map(&mut self) {
        self.map.push((self.disc_file, self.disc_page));
        self.disc_page += 1;
    }

    /// The current file has no more pages: continue the timeline at the start of
    /// the next file, or mark the real end if this was the last file.
    fn roll_to_next_file(&mut self) {
        self.disc_file += 1;
        self.disc_page = 0;
        if self.disc_file >= self.files.len() {
            self.at_end = true;
        }
    }
}

/// Open any supported file as a `Media`.
pub fn load(path: &Path) -> Result<Media> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "tif" | "tiff" => open_tiff(path, name),
        _ => open_still(path, name),
    }
}

fn open_still(path: &Path, name: String) -> Result<Media> {
    let frame = decode_still_frame(path)?;
    let hi_depth = frame.hi_depth();
    Ok(Media::Still(Still {
        name,
        frame: Arc::new(frame),
        hi_depth,
    }))
}

fn open_tiff(path: &Path, name: String) -> Result<Media> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut dec = Decoder::new(BufReader::new(file))?;

    let (w, h) = dec.dimensions()?;
    let ct = dec.colortype()?;
    let hi_depth = color_bits(ct) > 8;
    let is_mask = matches!(ct, ColorType::Gray(1));

    // Only page 0 is inspected here. The page count is discovered lazily as
    // later frames are shown — walking every IFD up front would stall opening a
    // long sequence, and pages may even differ in resolution.
    Ok(Media::TiffSeq(TiffSeq {
        name,
        path: path.to_path_buf(),
        size: [w as usize, h as usize],
        hi_depth,
        is_mask,
        frames: SeqCache::new(1),
        at_end: false,
    }))
}

/// Open a numbered file run (a compact `PREFIX%0Xu…,…` token) as a single media.
/// Multi-page TIFFs are **concatenated** into one continuous timeline
/// (`ConcatSeq`); any other extension is a still-per-file sequence (`FileSeq`).
/// `name` is the display label (typically the token itself).
pub fn load_sequence(files: &[PathBuf], name: String) -> Result<Media> {
    let first = files
        .first()
        .ok_or_else(|| anyhow!("empty image sequence"))?;
    let is_tiff = first
        .extension()
        .map(|e| {
            let e = e.to_string_lossy().to_lowercase();
            e == "tif" || e == "tiff"
        })
        .unwrap_or(false);
    if is_tiff {
        load_concat(files, name)
    } else {
        load_file_seq(files, name)
    }
}

/// A numbered still sequence — one image file per frame. The first file is
/// decoded up front for the size / bit depth and kept resident so the pane
/// shows something immediately; the rest decode on demand.
fn load_file_seq(files: &[PathBuf], name: String) -> Result<Media> {
    let f0 = decode_file(&files[0])?;
    let size = f0.size;
    let hi_depth = f0.hi_depth();

    let mut frames = SeqCache::new(files.len());
    frames.insert(0, Arc::new(f0));

    Ok(Media::FileSeq(FileSeq {
        name,
        paths: files.to_vec(),
        size,
        hi_depth,
        frames,
    }))
}

/// Several multi-page TIFFs concatenated into one timeline. Only the first
/// page of the first file is read up front (size / depth + an instant first
/// frame); page counts per file are discovered lazily while browsing, rolling
/// from one file to the next at each file's end.
fn load_concat(files: &[PathBuf], name: String) -> Result<Media> {
    let f0 = decode_file(&files[0])?; // page 0 of the first TIFF
    let size = f0.size;
    let hi_depth = f0.hi_depth();

    let mut frames = SeqCache::new(1);
    frames.insert(0, Arc::new(f0));

    Ok(Media::ConcatSeq(ConcatSeq {
        name,
        files: files.to_vec(),
        size,
        hi_depth,
        frames,
        map: vec![(0, 0)],  // global frame 0 = file 0, page 0
        disc_file: 0,
        disc_page: 1, // next frontier probe: page 1 of file 0
        at_end: false,
    }))
}

/// Decode one standalone file (a still, or one frame of a numbered sequence) at
/// native bit depth. Dispatches by extension like [`load`]: multi-page TIFFs
/// go through the `tiff` crate (page 0), everything else through the `image`
/// crate.
pub fn decode_file(path: &Path) -> Result<FrameData> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "tif" | "tiff" => {
            let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
            let mut dec = Decoder::new(BufReader::new(file))?;
            decode_current(&mut dec)
        }
        _ => decode_still_frame(path),
    }
}

/// Decode a still image file into a `FrameData` via the `image` crate, mapping
/// its colour type to native `Samples`.
fn decode_still_frame(path: &Path) -> Result<FrameData> {
    use image::ColorType as C;
    let dynimg = image::open(path).with_context(|| format!("decode image {}", path.display()))?;
    let color = dynimg.color();
    let (w, h) = (dynimg.width() as usize, dynimg.height() as usize);

    let (samples, channels) = match color {
        C::L8 | C::La8 => (Samples::U8(dynimg.to_luma8().into_raw()), 1),
        C::L16 | C::La16 => (Samples::U16(dynimg.to_luma16().into_raw()), 1),
        C::Rgb16 => (Samples::U16(dynimg.to_rgb16().into_raw()), 3),
        C::Rgba16 => (Samples::U16(dynimg.to_rgba16().into_raw()), 4),
        C::Rgb8 => (Samples::U8(dynimg.to_rgb8().into_raw()), 3),
        C::Rgb32F => (Samples::F32(dynimg.to_rgb32f().into_raw()), 3),
        C::Rgba32F => (Samples::F32(dynimg.to_rgba32f().into_raw()), 4),
        _ => (Samples::U8(dynimg.to_rgba8().into_raw()), 4),
    };
    Ok(FrameData::new([w, h], channels, samples))
}

/// A persistent TIFF reader for one sequence.
///
/// The `tiff` crate caches the byte offset of each IFD it has walked, but only
/// within a single `Decoder`. Keeping one `SeqReader` alive per sequence keeps
/// that cache warm, so seeking to page `k` no longer re-walks the whole IFD
/// chain from the start on every decode (which made a sweep O(N²)).
pub struct SeqReader {
    dec: Decoder<BufReader<File>>,
}

impl SeqReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        Ok(Self {
            dec: Decoder::new(BufReader::new(file))?,
        })
    }

    /// Decode page `idx` at native bit depth, or `Ok(None)` when `idx` is past
    /// the last page (how the caller learns the true length without counting
    /// ahead). Always seeks first — the reader may sit on any page from a prior
    /// call — which is cheap once the offset is cached.
    pub fn decode(&mut self, idx: usize) -> Result<Option<FrameData>> {
        if self.dec.seek_to_image(idx).is_err() {
            return Ok(None);
        }
        decode_current(&mut self.dec).map(Some)
    }

    /// Metadata-only frontier probe: seek to page `idx` and confirm it exists by
    /// reading its IFD, **without** decoding pixels. `Ok(true)` the page is
    /// there, `Ok(false)` it is past the last page. `seek_to_image` walks the
    /// IFD chain (cheap once offsets are cached) but never touches the strip
    /// data, so fast-forwarding a seek this way skips the per-page decompress.
    pub fn probe(&mut self, idx: usize) -> Result<bool> {
        Ok(self.dec.seek_to_image(idx).is_ok())
    }
}

fn decode_current(dec: &mut Decoder<BufReader<File>>) -> Result<FrameData> {
    let (w, h) = dec.dimensions()?;
    let (w, h) = (w as usize, h as usize);
    let color = dec.colortype()?;
    let channels = match color {
        ColorType::Gray(_) => 1,
        ColorType::RGB(_) => 3,
        ColorType::RGBA(_) => 4,
        other => return Err(anyhow!("unsupported TIFF color type: {:?}", other)),
    };

    let samples = match dec.read_image()? {
        DecodingResult::U8(b) => Samples::U8(b),
        DecodingResult::U16(b) => Samples::U16(b),
        DecodingResult::F32(b) => Samples::F32(b),
        // Less common numeric layouts: keep them viewable by widening to f32.
        DecodingResult::F64(b) => Samples::F32(b.into_iter().map(|x| x as f32).collect()),
        DecodingResult::U32(b) => Samples::F32(b.into_iter().map(|x| x as f32).collect()),
        DecodingResult::I8(b) => Samples::F32(b.into_iter().map(|x| x as f32).collect()),
        DecodingResult::I16(b) => Samples::F32(b.into_iter().map(|x| x as f32).collect()),
        DecodingResult::I32(b) => Samples::F32(b.into_iter().map(|x| x as f32).collect()),
        other => {
            return Err(anyhow!(
                "unsupported TIFF sample format: {:?}",
                std::mem::discriminant(&other)
            ))
        }
    };

    // A 1-bit bilevel page comes back with its pixels packed 8-to-a-byte;
    // expand to one 0/1 byte per pixel so the rest of the pipeline is uniform.
    //
    // A boolean mask's "true" is the raw stored sample bit — what the array
    // author actually set (e.g. `numpy` `True` → 1) — not the pixel's black/
    // white *appearance*. Those differ when PhotometricInterpretation is
    // WhiteIsZero (the TIFF baseline default, and what `tifffile` writes for a
    // bool array): there the bit `1` means black, and the `tiff` decoder has
    // already normalised the buffer to intensity (0 = black), flipping the bit.
    // Undo that here so mask-true == the set bit regardless of photometric.
    let is_mask = matches!(color, ColorType::Gray(1));
    let samples = if is_mask {
        let white_is_zero = dec
            .find_tag(tiff::tags::Tag::PhotometricInterpretation)
            .ok()
            .flatten()
            .and_then(|v| v.into_u16().ok())
            == Some(0);
        match samples {
            Samples::U8(packed) => Samples::U8(mask_bits(&packed, w, h, white_is_zero)),
            other => other,
        }
    } else {
        samples
    };

    let expected = w * h * channels;
    let got = match &samples {
        Samples::U8(v) => v.len(),
        Samples::U16(v) => v.len(),
        Samples::F32(v) => v.len(),
    };
    if got < expected {
        return Err(anyhow!("short TIFF buffer: {got} < {expected}"));
    }

    if is_mask {
        Ok(FrameData::new_mask([w, h], channels, samples))
    } else {
        Ok(FrameData::new([w, h], channels, samples))
    }
}

/// Boolean-mask bits from a decoded 1-bit page: unpack the bilevel buffer, then
/// (when the source was WhiteIsZero, so the `tiff` decoder already inverted the
/// stored bits to intensity) flip them back so a set pixel reads as `1`. The
/// result is the array author's original truth value, not the black/white look.
fn mask_bits(packed: &[u8], w: usize, h: usize, white_is_zero: bool) -> Vec<u8> {
    let mut bits = unpack_bilevel(packed, w, h);
    if white_is_zero {
        for b in &mut bits {
            *b ^= 1;
        }
    }
    bits
}

/// Expand a packed 1-bit bilevel buffer — MSB-first, each row padded to a byte
/// boundary (the TIFF layout) — into one `0`/`1` byte per pixel.
fn unpack_bilevel(packed: &[u8], w: usize, h: usize) -> Vec<u8> {
    let stride = w.div_ceil(8);
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        let row = y * stride;
        for x in 0..w {
            let byte = packed.get(row + x / 8).copied().unwrap_or(0);
            out[y * w + x] = (byte >> (7 - (x % 8))) & 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    /// Opening a TIFF must not walk the whole file: the length starts at one
    /// page and pages are discovered by decoding, with `Ok(None)` marking the
    /// end. Pages differ in resolution on purpose (like real captures).
    #[test]
    fn tiff_length_is_discovered_lazily() {
        let dir = fixture_dir("lazy_len");
        let path = dir.join("seq.tif");
        write_multipage_tiff_u16(&path, &[[32, 24], [16, 12], [32, 24]]);
        let m = load(&path).expect("open tiff");
        // Fresh open knows only the first page and hasn't confirmed the end.
        assert_eq!(m.frame_count(), 1);
        assert!(!m.at_end());

        // Walk pages the way the app does until a probe finds nothing.
        let mut reader = SeqReader::open(&path).expect("open");
        let mut pages = 0;
        loop {
            match reader.decode(pages).expect("decode") {
                Some(frame) => {
                    // Per-page native size and values survive the round trip.
                    let [w, h] = frame.size;
                    assert_eq!(frame.sample(0), gray16_page(w, h, pages as u16 * 1000)[0] as u32);
                    pages += 1;
                }
                None => break,
            }
        }
        assert_eq!(pages, 3, "all written pages decode");
        // Probing exactly at the end reports None, not an error.
        assert!(reader.decode(pages).unwrap().is_none());
    }

    /// The metadata-only frontier probe (`SeqReader::probe` + `Media::note_frontier`)
    /// discovers exactly the same length as a decode walk, but **without** making
    /// any frame resident — this is the seek fast-path that skips decompressing
    /// every page it rides past. `probe` reports the page's existence; the real
    /// length lands identically to `tiff_length_is_discovered_lazily`.
    #[test]
    fn probe_discovers_length_without_decoding() {
        let dir = fixture_dir("probe_len");
        let path = dir.join("seq.tif");
        write_multipage_tiff_u16(&path, &[[24, 16], [24, 16], [12, 8], [24, 16]]);
        let mut m = load(&path).expect("open tiff");
        let mut reader = SeqReader::open(&path).expect("open");

        // Walk the frontier via metadata-only probes, exactly as `drive_seek`
        // does, growing the known length one empty slot at a time.
        let mut guard = 0;
        while !m.at_end() {
            guard += 1;
            assert!(guard < 10_000, "probe discovery should terminate");
            let known = m.frame_count();
            if reader.probe(known).expect("probe") {
                m.note_frontier(known);
            } else {
                m.frontier_ended();
            }
        }

        // Compare against the true page count from a decode walk.
        let mut dec = SeqReader::open(&path).expect("open");
        let mut pages = 0;
        while dec.decode(pages).expect("decode").is_some() {
            pages += 1;
        }
        assert_eq!(pages, 4, "decode walk sees every written page");
        assert_eq!(m.frame_count(), pages, "probe length == decode length");
        assert_eq!(
            m.resident_count(),
            0,
            "probing must not decode/keep any frame resident"
        );
        assert_eq!(m.resident_bytes(), 0);
    }

    /// A numbered still run opens as one `FileSeq` media whose length is the
    /// file count (known up front, so it is immediately "at end"), with the
    /// first frame decoded eagerly and later frames decodable per file.
    #[test]
    fn file_sequence_opens_as_one_media() {
        let dir = fixture_dir("file_seq");
        let files = write_png_run(&dir, 12, 20, 10);
        let m = load_sequence(&files, "frame".into()).expect("open sequence");
        assert_eq!(m.frame_count(), 12);
        assert!(m.at_end(), "a file sequence knows its length up front");
        assert!(m.resident(0).is_some(), "first frame decoded eagerly");
        // A later frame decodes standalone from its own file.
        let f7 = decode_file(&files[7]).expect("decode frame 7");
        assert!(f7.size[0] > 0 && f7.size[1] > 0);
    }

    /// A run of multi-page TIFFs opens as one `ConcatSeq` whose length is the
    /// **sum** of the files' page counts, discovered lazily by rolling from one
    /// file to the next. Drives discovery exactly as the app does: probe the
    /// frontier, `insert` a decoded page or `frontier_ended` on a miss.
    #[test]
    fn concat_sequence_concatenates_pages() {
        let dir = fixture_dir("concat");
        let files = vec![dir.join("clip_000.tif"), dir.join("clip_001.tif")];
        write_multipage_tiff_u16(&files[0], &[[16, 12]; 4]); // 4 pages
        write_multipage_tiff_u16(&files[1], &[[16, 12]; 3]); // 3 pages
        let mut m = load_sequence(&files, "clip".into()).expect("open concat");
        assert!(matches!(m, Media::ConcatSeq(_)), "tiff run → ConcatSeq");
        assert_eq!(m.frame_count(), 1); // only the first page known at open

        // Walk the frontier until the real end, opening a fresh reader per probe.
        let mut guard = 0;
        while !m.at_end() {
            guard += 1;
            assert!(guard < 100, "discovery should terminate");
            let known = m.frame_count();
            let Some(DecodeReq::Tiff { page, path, .. }) = m.decode_job(known) else {
                break;
            };
            match SeqReader::open(&path).unwrap().decode(page).unwrap() {
                Some(frame) => m.insert(known, Arc::new(frame)),
                None => m.frontier_ended(),
            }
        }

        assert_eq!(m.frame_count(), 7, "4 + 3 pages concatenated");
        let (_, map) = m.concat_layout().expect("concat layout");
        assert_eq!(map[0], (0, 0), "frame 0 = file 0 page 0");
        assert_eq!(map[3], (0, 3), "frame 3 = file 0 page 3 (last of clip_000)");
        assert_eq!(map[4], (1, 0), "frame 4 rolls into file 1 page 0");
        assert_eq!(map[6], (1, 2), "frame 6 = file 1 page 2 (last of clip_001)");
    }

    /// Mask truth is the stored bit the author set, independent of the TIFF's
    /// black/white sense. The `tiff` decoder normalises to intensity (inverting
    /// WhiteIsZero), so `mask_bits` flips it back for WhiteIsZero. Both cases
    /// must recover the same "left half set" pattern a `numpy` `True` block
    /// would produce. One row of 8 px; the packed byte is the decoder's output.
    #[test]
    fn mask_bits_recover_stored_true_regardless_of_photometric() {
        // BlackIsZero: decoder leaves stored bits as-is (set → 1 → 0b11110000).
        assert_eq!(
            mask_bits(&[0b1111_0000], 8, 1, false),
            vec![1, 1, 1, 1, 0, 0, 0, 0],
        );
        // WhiteIsZero: decoder already inverted the stored bits to intensity
        // (0b00001111); flipping back recovers the same set-region truth.
        assert_eq!(
            mask_bits(&[0b0000_1111], 8, 1, true),
            vec![1, 1, 1, 1, 0, 0, 0, 0],
        );
    }

    /// A 1-bit bilevel TIFF opens as a mask media whose decoded truth is the
    /// **stored bit** the author set, for both PhotometricInterpretation senses
    /// (the `tiff` decoder inverts WhiteIsZero to intensity; `mask_bits` flips
    /// it back). Width 13 exercises the byte-padded row unpacking.
    #[test]
    fn bilevel_tiff_opens_as_mask() {
        let dir = fixture_dir("bilevel");
        let (w, h) = (13usize, 6usize);
        // Stored truth: left half true — what a `numpy` bool block would set.
        let bits: Vec<u8> = (0..w * h)
            .map(|i| u8::from(i % w < w / 2))
            .collect();

        for white_is_zero in [true, false] {
            let path = dir.join(format!("mask_wiz{}.tif", white_is_zero as u8));
            write_bilevel_tiff(&path, w, h, &bits, white_is_zero);

            let m = load(&path).expect("open mask tiff");
            assert!(m.is_mask(), "1-bit bilevel TIFF should be a mask media");
            let frame = SeqReader::open(&path)
                .unwrap()
                .decode(0)
                .unwrap()
                .expect("page 0");
            assert!(frame.is_mask(), "decoded page should be flagged as a mask");
            assert_eq!(frame.size, [w, h]);

            // Decoded truth == stored truth, independent of the photometric.
            for (i, &b) in bits.iter().enumerate() {
                assert_eq!(
                    frame.sample(i),
                    b as u32,
                    "px {i} (white_is_zero = {white_is_zero})"
                );
            }

            // Renders black/white at native size.
            let mut out = Vec::new();
            frame.render_into(0.0, 255.0, &mut out);
            assert_eq!(out.len(), w * h * 4);
        }
    }

    /// `crop` extracts the sub-rectangle's samples (clamped to the frame),
    /// preserving channels and mask flag, with independent (fresh) bounds.
    #[test]
    fn crop_extracts_subrect() {
        // 4×3 single-channel gradient, value = y*4 + x.
        let f = FrameData::new([4, 3], 1, Samples::U8((0..12).collect()));
        // Crop the 2×2 at (1, 1): values [5,6 / 9,10].
        let c = f.crop(1, 1, 2, 2);
        assert_eq!(c.size, [2, 2]);
        assert_eq!(c.channels, 1);
        match &c.samples {
            Samples::U8(v) => assert_eq!(v, &vec![5, 6, 9, 10]),
            _ => panic!("wrong sample type"),
        }
        // Out-of-bounds request clamps to the frame edge.
        let e = f.crop(3, 2, 10, 10);
        assert_eq!(e.size, [1, 1]);
        match &e.samples {
            Samples::U8(v) => assert_eq!(v, &vec![11]),
            _ => panic!("wrong sample type"),
        }
    }

    /// Inserting a frame accounts its bytes; evicting frees them and keeps the
    /// known length intact so the frame can be re-decoded later.
    #[test]
    fn eviction_frees_bytes_and_keeps_length() {
        let dir = fixture_dir("evict");
        let path = dir.join("seq.tif");
        write_multipage_tiff_u16(&path, &[[32, 24], [32, 24]]);
        let mut m = load(&path).expect("open tiff");
        assert_eq!(m.resident_bytes(), 0);

        let frame = SeqReader::open(&path)
            .unwrap()
            .decode(0)
            .unwrap()
            .expect("page 0");
        let bytes = frame.byte_len();
        assert!(bytes > 0);

        m.insert(0, Arc::new(frame));
        assert_eq!(m.resident_bytes(), bytes);
        assert!(m.resident(0).is_some());
        let len = m.frame_count();

        m.evict(0);
        assert_eq!(m.resident_bytes(), 0);
        assert!(m.resident(0).is_none());
        assert_eq!(m.frame_count(), len, "eviction must not change known length");
    }

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
        assert_eq!(f.region_display_bounds(1, 0, 3, 1, false, 0.01), (10.0, 20.0));

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
        assert_eq!(diff_frames(&da, &db).expect("diff").color_f32().1, vec![-4.0, -10.0]);
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
        assert!((lo - 10.0).abs() < 0.01 && (hi - 20.0).abs() < 0.01, "({lo}, {hi})");
    }
}

/// Bits per sample carried by a TIFF colour type.
fn color_bits(c: ColorType) -> u8 {
    match c {
        ColorType::Gray(b)
        | ColorType::RGB(b)
        | ColorType::RGBA(b)
        | ColorType::GrayA(b)
        | ColorType::CMYK(b)
        | ColorType::CMYKA(b)
        | ColorType::YCbCr(b)
        | ColorType::Lab(b)
        | ColorType::Palette(b) => b,
        ColorType::Multiband { bit_depth, .. } => bit_depth,
        // `ColorType` is #[non_exhaustive]; assume 8-bit for anything new.
        _ => 8,
    }
}

