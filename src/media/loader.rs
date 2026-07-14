//! Opening and decoding media files: the `load*` constructors that turn paths
//! into [`Media`], the stateless still decoders, and the persistent
//! [`SeqReader`] (plus the TIFF bilevel-mask bit handling).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::ColorType;

use super::source::{ConcatSeq, FileSeq, Media, SeqCache, Still, TiffSeq};
use super::{FrameData, Samples};

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

/// A `File` wrapped so the wall-clock time spent inside its `read` / `seek`
/// calls accumulates into a shared counter. The `tiff` crate interleaves file
/// reads with decompression inside `read_image`, so the I/O layer is the only
/// place true file time can be told apart from CPU decode — [`SeqReader`]
/// reports it per decode via `take_io`, splitting the profiler's Decode stage
/// into **Read (file I/O)** and **Decode (CPU)**. Two `Instant::now` per
/// syscall is noise next to the syscall itself, so it's not debug-gated.
struct TimedFile {
    file: File,
    io_nanos: Arc<AtomicU64>,
}

impl Read for TimedFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let t = std::time::Instant::now();
        let r = self.file.read(buf);
        self.io_nanos
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }
}

impl Seek for TimedFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let t = std::time::Instant::now();
        let r = self.file.seek(pos);
        self.io_nanos
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }
}

/// A persistent TIFF reader for one sequence.
///
/// The `tiff` crate caches the byte offset of each IFD it has walked, but only
/// within a single `Decoder`. Keeping one `SeqReader` alive per sequence keeps
/// that cache warm, so seeking to page `k` no longer re-walks the whole IFD
/// chain from the start on every decode (which made a sweep O(N²)).
pub struct SeqReader {
    dec: Decoder<BufReader<TimedFile>>,
    io_nanos: Arc<AtomicU64>,
}

impl SeqReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let io_nanos = Arc::new(AtomicU64::new(0));
        Ok(Self {
            dec: Decoder::new(BufReader::new(TimedFile {
                file,
                io_nanos: Arc::clone(&io_nanos),
            }))?,
            io_nanos,
        })
    }

    /// Drain the accumulated file-I/O time (reads + seeks) since the last call.
    /// The decode worker takes it once per job, so each `Done` carries the I/O
    /// share of that decode for the profiler.
    pub fn take_io(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.io_nanos.swap(0, Ordering::Relaxed))
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

fn decode_current<R: Read + Seek>(dec: &mut Decoder<R>) -> Result<FrameData> {
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
pub(super) fn mask_bits(packed: &[u8], w: usize, h: usize, white_is_zero: bool) -> Vec<u8> {
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

