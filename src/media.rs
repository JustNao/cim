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
    pub min: u32,
    pub max: u32,
    pub mono: bool,
}

impl FrameData {
    #[inline]
    pub fn sample(&self, idx: usize) -> u32 {
        match &self.samples {
            Samples::U8(v) => v[idx] as u32,
            Samples::U16(v) => v[idx] as u32,
        }
    }

    /// Largest representable value for the sample type (255 or 65535).
    pub fn max_possible(&self) -> u32 {
        match self.samples {
            Samples::U8(_) => 255,
            Samples::U16(_) => 65535,
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
        if self.color_channels() == 1 {
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
        } else {
            (0.0, self.max_possible() as f32)
        }
    }

    /// Values at the `p`% and `(100 - p)`% percentiles of the colour samples.
    fn percentile_bounds(&self, p: f32) -> (f32, f32) {
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

    /// Build the 8-bit RGBA buffer egui uploads as a texture.
    pub fn render_rgba(&self, clip: bool) -> Vec<u8> {
        let (lo, hi) = self.display_bounds(clip);
        let scale = 255.0 / (hi - lo).max(1.0);
        let map = |s: u32| -> u8 { (((s as f32 - lo) * scale).clamp(0.0, 255.0)) as u8 };

        let px = self.size[0] * self.size[1];
        let ch = self.channels;
        let mut out = vec![0u8; px * 4];
        for i in 0..px {
            let base = i * ch;
            let (r, g, b) = if self.color_channels() == 1 {
                let v = map(self.sample(base));
                (v, v, v)
            } else {
                (
                    map(self.sample(base)),
                    map(self.sample(base + 1)),
                    map(self.sample(base + 2)),
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

        let mut min = u32::MAX;
        let mut max = 0u32;
        for i in 0..px {
            let base = i * self.channels;
            for c in 0..cc {
                let s = self.sample(base + c);
                min = min.min(s);
                max = max.max(s);
            }
        }
        if min > max {
            min = 0;
            max = self.max_possible();
        }
        let span = (max - min).max(1) as f32;
        let last = (nbins - 1) as f32;

        let mut bins = vec![vec![0u32; nbins]; cc];
        for i in 0..px {
            let base = i * self.channels;
            for c in 0..cc {
                let s = self.sample(base + c);
                let bin = (((s - min) as f32 / span) * last) as usize;
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
    cache: Vec<Option<Arc<FrameData>>>,
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
            if let Some(slot) = t.cache.get_mut(idx) {
                *slot = Some(frame);
            }
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
        _ => (Samples::U8(dynimg.to_rgba8().into_raw()), 4),
    };
    let hi_depth = matches!(color, C::L16 | C::La16 | C::Rgb16 | C::Rgba16);

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

    // Count pages by walking IFDs (cheap: only metadata is read).
    let mut pages = 1usize;
    while dec.more_images() {
        dec.next_image()?;
        pages += 1;
    }

    Ok(Media::TiffSeq(TiffSeq {
        name,
        path: path.to_path_buf(),
        size: [w as usize, h as usize],
        hi_depth,
        cache: vec![None; pages],
    }))
}

/// Decode a single TIFF page at native bit depth. Stateless / `Send`-safe.
pub fn decode_tiff_page(path: &Path, idx: usize) -> Result<FrameData> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut dec = Decoder::new(BufReader::new(file))?;
    dec.seek_to_image(idx)
        .with_context(|| format!("seek to page {idx}"))?;
    decode_current(&mut dec)
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
