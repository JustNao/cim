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

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tiff::decoder::{Decoder, DecodingResult};
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
}

/// Per-channel histogram plus the true value extent, for the Visualise panel.
pub struct HistData {
    pub bins: Vec<Vec<u32>>, // 1 curve if mono, else R,G,B
    pub min: f32,
    pub max: f32,
    pub mono: bool,
}

impl FrameData {
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

    /// Display range [lo, hi] mapped to [0, 255]. With `clip`, a fixed 0.01%
    /// percentile stretch (robust auto-contrast); otherwise the full range.
    fn display_bounds(&self, clip: bool) -> (f32, f32) {
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
    fn percentile_bounds(&self, p: f32) -> (f32, f32) {
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
    fn percentile_bounds_float(&self, p: f32) -> (f32, f32) {
        const NB: usize = 4096;
        let (min, max) = self.value_extent();
        if !(max > min) {
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

    /// Build the 8-bit RGBA buffer egui uploads as a texture.
    pub fn render_rgba(&self, clip: bool) -> Vec<u8> {
        let (lo, hi) = self.display_bounds(clip);
        let denom = hi - lo;
        let scale = if denom > 0.0 { 255.0 / denom } else { 0.0 };
        let map = |s: f32| -> u8 { (((s - lo) * scale).clamp(0.0, 255.0)) as u8 };

        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        let mut out = vec![0u8; px * 4];
        for i in 0..px {
            let base = i * ch;
            let (r, g, b) = if self.color_channels() == 1 {
                let v = map(self.sample_f(base));
                (v, v, v)
            } else {
                (
                    map(self.sample_f(base)),
                    map(self.sample_f(base + 1)),
                    map(self.sample_f(base + 2)),
                )
            };
            out[i * 4] = r;
            out[i * 4 + 1] = g;
            out[i * 4 + 2] = b;
            out[i * 4 + 3] = 255;
        }
        out
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
            for c in 0..cc {
                let s = self.sample_f(base + c);
                if s.is_nan() {
                    continue;
                }
                let bin = (((s - min) / span) * last) as usize;
                bins[c][bin.min(nbins - 1)] += 1;
            }
        }
        HistData {
            bins,
            min,
            max,
            mono: cc == 1,
        }
    }
}

pub enum Media {
    Still(Still),
    TiffSeq(TiffSeq),
}

pub struct Still {
    name: String,
    frame: Arc<FrameData>,
    hi_depth: bool,
}

pub struct TiffSeq {
    name: String,
    path: PathBuf,
    size: [usize; 2],
    hi_depth: bool,
    /// Frames known so far. Grows lazily as later pages are decoded — we never
    /// walk the whole file to learn its length. Slots may be evicted back to
    /// `None` to stay within the cache budget; the length is still preserved.
    cache: Vec<Option<Arc<FrameData>>>,
    /// Recency tick per frame, parallel to `cache`. The budget evicts the
    /// least-recently-used resident frames first.
    last_used: Vec<u64>,
    /// Running total of resident sample bytes (sum of `byte_len` over `Some`s).
    resident_bytes: usize,
    /// Set once a probe past `cache.len()` found no more pages: the real end.
    at_end: bool,
}

impl Media {
    pub fn name(&self) -> &str {
        match self {
            Media::Still(s) => &s.name,
            Media::TiffSeq(t) => &t.name,
        }
    }

    pub fn frame_count(&self) -> usize {
        match self {
            Media::Still(_) => 1,
            Media::TiffSeq(t) => t.cache.len(),
        }
    }

    pub fn size(&self) -> [usize; 2] {
        match self {
            Media::Still(s) => s.frame.size,
            Media::TiffSeq(t) => t.size,
        }
    }

    /// More than 8 bits per sample → clip-on-load is a sensible default.
    pub fn hi_depth(&self) -> bool {
        match self {
            Media::Still(s) => s.hi_depth,
            Media::TiffSeq(t) => t.hi_depth,
        }
    }

    pub fn resident(&self, idx: usize) -> Option<Arc<FrameData>> {
        match self {
            Media::Still(s) => Some(s.frame.clone()),
            Media::TiffSeq(t) => t.cache.get(idx).and_then(|slot| slot.clone()),
        }
    }

    pub fn decode_job(&self, _idx: usize) -> Option<PathBuf> {
        match self {
            Media::Still(_) => None,
            Media::TiffSeq(t) => Some(t.path.clone()),
        }
    }

    pub fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        if let Media::TiffSeq(t) = self {
            if idx < t.cache.len() {
                if let Some(old) = &t.cache[idx] {
                    t.resident_bytes -= old.byte_len();
                }
                t.resident_bytes += frame.byte_len();
                t.cache[idx] = Some(frame);
            } else if idx == t.cache.len() {
                // A frontier probe discovered the next page: extend the length.
                t.resident_bytes += frame.byte_len();
                t.cache.push(Some(frame));
                t.last_used.push(0);
            }
        }
    }

    /// Mark frame `idx` as used at `clock`, so the budget evicts it last.
    pub fn touch(&mut self, idx: usize, clock: u64) {
        if let Media::TiffSeq(t) = self {
            if let Some(u) = t.last_used.get_mut(idx) {
                *u = clock;
            }
        }
    }

    /// Drop a resident frame to reclaim memory. The known length is unchanged;
    /// the frame simply re-decodes on demand if shown again.
    pub fn evict(&mut self, idx: usize) {
        if let Media::TiffSeq(t) = self {
            if let Some(slot) = t.cache.get_mut(idx) {
                if let Some(old) = slot.take() {
                    t.resident_bytes -= old.byte_len();
                }
            }
        }
    }

    /// Sample bytes currently resident, for the memory budget.
    pub fn resident_bytes(&self) -> usize {
        match self {
            Media::Still(s) => s.frame.byte_len(),
            Media::TiffSeq(t) => t.resident_bytes,
        }
    }

    /// Resident frames as `(frame index, last-used tick, byte size)`. Stills
    /// return empty — their single frame is always needed and never evicted.
    pub fn resident_frames(&self) -> Vec<(usize, u64, usize)> {
        match self {
            Media::Still(_) => Vec::new(),
            Media::TiffSeq(t) => t
                .cache
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_ref().map(|f| (i, t.last_used[i], f.byte_len())))
                .collect(),
        }
    }

    /// True once we've confirmed there is no page beyond what we already know
    /// (always true for a still).
    pub fn at_end(&self) -> bool {
        match self {
            Media::Still(_) => true,
            Media::TiffSeq(t) => t.at_end,
        }
    }

    /// Record that a frontier probe found no further page.
    pub fn set_at_end(&mut self) {
        if let Media::TiffSeq(t) = self {
            t.at_end = true;
        }
    }

    pub fn resident_count(&self) -> usize {
        match self {
            Media::Still(_) => 1,
            Media::TiffSeq(t) => t.cache.iter().filter(|f| f.is_some()).count(),
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
    let hi_depth = matches!(
        color,
        C::L16 | C::La16 | C::Rgb16 | C::Rgba16 | C::Rgb32F | C::Rgba32F
    );

    Ok(Media::Still(Still {
        name,
        frame: Arc::new(FrameData {
            size: [w, h],
            channels,
            samples,
        }),
        hi_depth,
    }))
}

fn open_tiff(path: &Path, name: String) -> Result<Media> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut dec = Decoder::new(BufReader::new(file))?;

    let (w, h) = dec.dimensions()?;
    let hi_depth = color_bits(dec.colortype()?) > 8;

    // Only page 0 is inspected here. The page count is discovered lazily as
    // later frames are shown — walking every IFD up front would stall opening a
    // long sequence, and pages may even differ in resolution.
    Ok(Media::TiffSeq(TiffSeq {
        name,
        path: path.to_path_buf(),
        size: [w as usize, h as usize],
        hi_depth,
        cache: vec![None],
        last_used: vec![0],
        resident_bytes: 0,
        at_end: false,
    }))
}

/// Decode a single TIFF page at native bit depth. Stateless / `Send`-safe.
/// Returns `Ok(None)` when `idx` is past the last page — that's how the caller
/// discovers the true length without counting pages ahead of time.
pub fn decode_tiff_page(path: &Path, idx: usize) -> Result<Option<FrameData>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut dec = Decoder::new(BufReader::new(file))?;
    // A fresh decoder already sits on page 0; only seek for later pages. A seek
    // failure means the page doesn't exist → end of sequence.
    if idx > 0 && dec.seek_to_image(idx).is_err() {
        return Ok(None);
    }
    decode_current(&mut dec).map(Some)
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

    let expected = w * h * channels;
    let got = match &samples {
        Samples::U8(v) => v.len(),
        Samples::U16(v) => v.len(),
        Samples::F32(v) => v.len(),
    };
    if got < expected {
        return Err(anyhow!("short TIFF buffer: {got} < {expected}"));
    }

    Ok(FrameData {
        size: [w, h],
        channels,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening a TIFF must not walk the whole file: the length starts at one
    /// page and pages are discovered by decoding, with `Ok(None)` marking the
    /// end. Skips gracefully when the fixture isn't present.
    #[test]
    fn tiff_length_is_discovered_lazily() {
        let path = Path::new("examples/alpes_noisy_a.tif");
        if !path.exists() {
            return;
        }
        let m = load(path).expect("open tiff");
        // Fresh open knows only the first page and hasn't confirmed the end.
        assert_eq!(m.frame_count(), 1);
        assert!(!m.at_end());

        // Walk pages the way the app does until a probe finds nothing.
        let mut pages = 0;
        loop {
            match decode_tiff_page(path, pages).expect("decode") {
                Some(_) => pages += 1,
                None => break,
            }
        }
        assert!(pages >= 1, "at least one page must decode");
        // Probing exactly at the end reports None, not an error.
        assert!(decode_tiff_page(path, pages).unwrap().is_none());
    }

    /// Inserting a frame accounts its bytes; evicting frees them and keeps the
    /// known length intact so the frame can be re-decoded later.
    #[test]
    fn eviction_frees_bytes_and_keeps_length() {
        let path = Path::new("examples/alpes_noisy_a.tif");
        if !path.exists() {
            return;
        }
        let mut m = load(path).expect("open tiff");
        assert_eq!(m.resident_bytes(), 0);

        let frame = decode_tiff_page(path, 0).unwrap().expect("page 0");
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
}

/// Bits per sample carried by a TIFF colour type.
fn color_bits(c: ColorType) -> u8 {
    match c {
        ColorType::Gray(b)
        | ColorType::RGB(b)
        | ColorType::RGBA(b)
        | ColorType::GrayA(b)
        | ColorType::CMYK(b)
        | ColorType::YCbCr(b)
        | ColorType::Palette(b) => b,
    }
}
