//! Fast jump: predict a page's byte position in a **regularly laid out**
//! multi-page TIFF instead of walking the IFD chain to it.
//!
//! A TIFF page is an IFD (tag list) plus strip data at arbitrary offsets, so in
//! general reaching page N means following N next-IFD pointers — O(N), the cost
//! `pending_seek` rides out one probe at a time (§4). But the scientific writers
//! this tool mostly reads (`tifffile`, ImageJ) emit **uniform, uncompressed**
//! pages back to back, so every page sits at a fixed byte stride. [`FastScan`]
//! measures that stride from the first two IFDs, predicts page N's position as
//! `ifd0 + N × stride`, and **validates before trusting**: the predicted IFD
//! must match the first page's shape tag for tag, its strip data must sit at
//! the same stride, and its predecessor's next-IFD pointer must land on it. A
//! prediction that fails any check is discarded (never a wrong frame — the
//! caller falls back to the ordinary chain walk or cancels), so this is purely
//! an opportunistic O(1) shortcut. Both classic TIFF and **BigTIFF** (64-bit
//! offsets, wider IFDs — the ≥4 GiB case where the shortcut matters most) are
//! handled; the width-dependent reads branch on `FastScan::big`.
//!
//! Used three ways: [`SeqReader`](super::SeqReader) consults a measured layout
//! to make far probes/decodes O(1); the frame bar's **Load offsets fast** button
//! calls [`fast_load_offsets`] to complete the whole timeline length by
//! binary-searching each file's page count; and the frame readout calls
//! [`fast_jump`] to validate + decode an arbitrary typed index directly (falling
//! back to riding the frontier when the prediction can't be made).
//! [`availability`] gates the button and rides the *Load offsets* hover text.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::source::Media;
use super::{FrameData, Samples};

// TIFF tag ids (classic, baseline).
const T_WIDTH: u16 = 256;
const T_HEIGHT: u16 = 257;
const T_BITS: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC: u16 = 262;
const T_STRIP_OFFSETS: u16 = 273;
const T_SPP: u16 = 277;
const T_STRIP_COUNTS: u16 = 279;
const T_PLANAR: u16 = 284;
const T_PREDICTOR: u16 = 317;
const T_TILE_WIDTH: u16 = 322;
const T_SAMPLE_FORMAT: u16 = 339;

/// The fields of one IFD that determine whether a page matches the measured
/// template (and how to read its pixels). Unknown tags are ignored — they don't
/// affect where the strip data lives, which is all prediction relies on.
#[derive(Clone, PartialEq)]
struct PageIfd {
    entries: u64,
    width: u32,
    height: u32,
    /// Bits per sample, one per channel (all required equal).
    bits: Vec<u16>,
    spp: u16,
    compression: u16,
    photometric: u16,
    /// 1 = unsigned int (the default), 3 = IEEE float.
    sample_format: u16,
    planar: u16,
    predictor: u16,
    tiled: bool,
    strip_offsets: Vec<u64>,
    strip_counts: Vec<u64>,
    next: u64,
}

/// A measured regular layout plus its own file handle: page N's IFD is
/// predicted at `ifd0 + N × stride` and validated against `template` before
/// anything is trusted. Built by [`FastScan::open`]; the `Err(reason)` strings
/// are user-facing (they ride the *Load offsets* hover text).
pub struct FastScan {
    file: File,
    file_len: u64,
    big_endian: bool,
    /// BigTIFF (magic 43): 64-bit offsets, an 8-byte IFD entry count, 20-byte
    /// entries and an 8-byte value/offset field — otherwise identical to classic
    /// TIFF (magic 42). All the width-dependent reads branch on this.
    big: bool,
    ifd0: u64,
    stride: u64,
    template: PageIfd,
}

impl FastScan {
    /// Width of the IFD entry-count field at the start of an IFD (u16 classic,
    /// u64 BigTIFF).
    fn count_width(&self) -> u64 {
        if self.big {
            8
        } else {
            2
        }
    }

    /// Width of an entry's *value count* field and its value/offset field, both
    /// this wide (u32 classic, u64 BigTIFF). An entry is thus `4 + 2·field_width`
    /// bytes (12 classic, 20 BigTIFF), with the value/offset at `4 + field_width`.
    fn field_width(&self) -> usize {
        if self.big {
            8
        } else {
            4
        }
    }

    /// Read a field/offset-sized integer: u32 (classic) or u64 (BigTIFF).
    fn read_field(&self, b: &[u8]) -> u64 {
        if self.big {
            get_u64(b, self.big_endian)
        } else {
            get_u32(b, self.big_endian) as u64
        }
    }

    /// Read the IFD entry count: u16 (classic) or u64 (BigTIFF).
    fn read_count(&self, b: &[u8]) -> u64 {
        if self.big {
            get_u64(b, self.big_endian)
        } else {
            get_u16(b, self.big_endian) as u64
        }
    }
}

impl FastScan {
    /// Open `path` (classic TIFF or BigTIFF) and measure its page stride from the
    /// first two IFDs, rejecting (with a human-readable reason) any file whose
    /// page positions can't be predicted: compression, tiling, planar layout,
    /// pages differing in shape, an irregular stride, or sample layouts the raw
    /// reader can't reproduce bit-exactly.
    pub fn open(path: &Path) -> Result<FastScan, String> {
        let mut file = File::open(path).map_err(|e| format!("can't open the file: {e}"))?;
        let file_len = file
            .metadata()
            .map_err(|e| format!("can't stat the file: {e}"))?
            .len();

        // 16 bytes covers both header shapes (any real TIFF is far larger).
        let mut header = [0u8; 16];
        read_at(&mut file, 0, &mut header).ok_or("not a readable TIFF")?;
        let big_endian = match &header[..2] {
            b"II" => false,
            b"MM" => true,
            _ => return Err("not a TIFF file".into()),
        };
        // Classic TIFF (magic 42): 8-byte header, 32-bit first-IFD offset.
        // BigTIFF (magic 43): a 16-byte header carrying the offset bytesize
        // (must be 8) + a zero word, then the 64-bit first-IFD offset.
        let (big, ifd0) = match get_u16(&header[2..4], big_endian) {
            42 => (false, get_u32(&header[4..8], big_endian) as u64),
            43 => {
                if get_u16(&header[4..6], big_endian) != 8
                    || get_u16(&header[6..8], big_endian) != 0
                {
                    return Err("malformed BigTIFF header".into());
                }
                (true, get_u64(&header[8..16], big_endian))
            }
            _ => return Err("not a TIFF file".into()),
        };

        let mut scan = FastScan {
            file,
            file_len,
            big_endian,
            big,
            ifd0,
            stride: 0,                 // measured below
            template: PageIfd::default(), // replaced below
        };
        let p0 = scan
            .read_ifd(ifd0)
            .ok_or("unreadable first page header")?;

        // Everything prediction (and the raw strip reader) depends on.
        if p0.tiled {
            return Err("pages are tiled".into());
        }
        if p0.compression != 1 || p0.predictor != 1 {
            return Err("pages are compressed (no fixed byte stride)".into());
        }
        if p0.planar != 1 {
            return Err("pages use planar (non-interleaved) storage".into());
        }
        if !matches!(p0.photometric, 1 | 2) {
            return Err("unsupported photometric interpretation".into());
        }
        if !matches!(p0.spp, 1 | 3 | 4) {
            return Err("unsupported channel count".into());
        }
        let bits = p0.bits.first().copied().unwrap_or(0);
        if p0.bits.iter().any(|&b| b != bits) {
            return Err("channels differ in bit depth".into());
        }
        match (bits, p0.sample_format) {
            (8 | 16, 1) | (32, 1) | (32, 3) => {}
            _ => return Err("unsupported bit depth or sample format".into()),
        }
        // Uncompressed strips must add up to exactly width × height × bytes; a
        // mismatch means padding or a layout the raw reader would misread.
        let expect = p0.width as u64 * p0.height as u64 * p0.spp as u64 * (bits as u64 / 8);
        if p0.strip_counts.iter().sum::<u64>() != expect {
            return Err("strip data doesn't match the page dimensions".into());
        }

        if p0.next == 0 {
            return Err("single-page TIFF (no stride to measure)".into());
        }
        let p1 = scan
            .read_ifd(p0.next)
            .ok_or("unreadable second page header")?;
        if !p1.same_shape(&p0) {
            return Err("the first two pages differ in size or format".into());
        }
        if p0.next <= ifd0 {
            return Err("pages aren't laid out forward in the file".into());
        }
        let stride = p0.next - ifd0;
        // The data must ride the same stride as the IFDs, and page 2 (if any)
        // must continue it — one irregular writer quirk and prediction is off.
        if !offsets_at_stride(&p1.strip_offsets, &p0.strip_offsets, stride)
            || (p1.next != 0 && p1.next != ifd0 + 2 * stride)
        {
            return Err("irregular page placement (positions can't be predicted)".into());
        }

        scan.stride = stride;
        scan.template = p0;
        Ok(scan)
    }

    /// Predicted byte offset of page `idx`'s IFD.
    fn predicted(&self, idx: usize) -> u64 {
        self.ifd0 + idx as u64 * self.stride
    }

    /// Whether a page exists at its predicted position: its IFD must match the
    /// template tag for tag, its strip data must sit at the predicted stride,
    /// its next-IFD pointer must continue the stride (or end the file), and its
    /// predecessor's next-IFD pointer must land on it (so the real page chain
    /// enters the predicted position — a truncated or spliced file fails here).
    /// `false` covers both "past the end" and "irregular"; the caller decides
    /// whether to fall back to the chain walk or cancel.
    pub fn validate(&mut self, idx: usize) -> bool {
        if idx == 0 {
            return true; // the template itself
        }
        let off = self.predicted(idx);
        let Some(p) = self.read_ifd(off) else {
            return false;
        };
        let shift = idx as u64 * self.stride;
        if !p.same_shape(&self.template)
            || !offsets_at_stride(&p.strip_offsets, &self.template.strip_offsets, shift)
            || (p.next != 0 && p.next != off + self.stride)
        {
            return false;
        }
        self.next_of(off - self.stride) == Some(off)
    }

    /// Validate page `idx` and decode it from its raw strips — bit-exact with
    /// the `tiff`-crate decode for the layouts `open` admits (uncompressed
    /// BlackIsZero / RGB chunky data, where the strip bytes *are* the samples,
    /// modulo byte order). `None` when the prediction doesn't validate.
    pub fn read_page(&mut self, idx: usize) -> Option<FrameData> {
        if !self.validate(idx) {
            return None;
        }
        let t = &self.template;
        let total: u64 = t.strip_counts.iter().sum();
        let mut raw = vec![0u8; total as usize];
        let mut at = 0usize;
        let shift = idx as u64 * self.stride;
        let (offsets, counts) = (t.strip_offsets.clone(), t.strip_counts.clone());
        for (off, n) in offsets.iter().zip(&counts) {
            read_at(&mut self.file, off + shift, &mut raw[at..at + *n as usize])?;
            at += *n as usize;
        }

        let t = &self.template;
        let be = self.big_endian;
        let samples = match (t.bits[0], t.sample_format) {
            (8, _) => Samples::U8(raw),
            (16, _) => Samples::U16(
                raw.chunks_exact(2)
                    .map(|c| get_u16(c, be))
                    .collect(),
            ),
            (32, 3) => Samples::F32(
                raw.chunks_exact(4)
                    .map(|c| f32::from_bits(get_u32(c, be)))
                    .collect(),
            ),
            // 32-bit uint: widen to f32, matching `decode_current`'s fallback.
            (32, _) => Samples::F32(
                raw.chunks_exact(4)
                    .map(|c| get_u32(c, be) as f32)
                    .collect(),
            ),
            _ => return None, // open() admits no other layout
        };
        Some(FrameData::new(
            [t.width as usize, t.height as usize],
            t.spp as usize,
            samples,
        ))
    }

    /// Exact page count, found by binary-searching the largest index that
    /// validates (≈20 header reads instead of walking every IFD). The last
    /// page's next-IFD pointer must be 0 — a nonzero one means the chain
    /// continues off-stride, so the count can't be trusted.
    pub fn page_count(&mut self) -> Result<usize, String> {
        // Pages 0 and 1 are known valid (measured); anything whose IFD would
        // start past EOF can't be.
        let mut lo = 1usize;
        let mut hi = ((self.file_len.saturating_sub(self.ifd0)) / self.stride + 2) as usize;
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.validate(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        if self.next_of(self.predicted(lo)) != Some(0) {
            return Err("irregular sequence tail (page count can't be trusted)".into());
        }
        Ok(lo + 1)
    }

    /// The next-IFD pointer of the IFD at `off` (with an entry-count sanity
    /// check against the template). `None` when unreadable.
    fn next_of(&mut self, off: u64) -> Option<u64> {
        let (cw, fw) = (self.count_width(), self.field_width());
        let es = 4 + 2 * fw; // entry size
        let mut cb = [0u8; 8];
        read_at(&mut self.file, off, &mut cb[..cw as usize])?;
        let n = self.read_count(&cb);
        if n != self.template.entries {
            return None;
        }
        let mut next = [0u8; 8];
        read_at(&mut self.file, off + cw + n * es as u64, &mut next[..fw])?;
        Some(self.read_field(&next))
    }

    /// Parse the IFD at `off` into the fields prediction cares about. `None`
    /// when it can't be a plausible IFD (truncated, absurd entry count, or a
    /// required tag missing / of an unexpected type). Handles both classic and
    /// BigTIFF entry widths via `count_width`/`field_width`.
    fn read_ifd(&mut self, off: u64) -> Option<PageIfd> {
        let (cw, fw) = (self.count_width(), self.field_width());
        let es = 4 + 2 * fw; // entry size (12 classic, 20 BigTIFF)
        if off == 0 || off + cw > self.file_len {
            return None;
        }
        let mut cb = [0u8; 8];
        read_at(&mut self.file, off, &mut cb[..cw as usize])?;
        let n = self.read_count(&cb);
        if n == 0 || n > 4096 {
            return None;
        }
        let mut buf = vec![0u8; n as usize * es + fw]; // entries + next-IFD pointer
        read_at(&mut self.file, off + cw, &mut buf)?;

        let mut p = PageIfd {
            entries: n,
            ..PageIfd::default()
        };
        p.next = self.read_field(&buf[n as usize * es..]);
        for e in buf[..n as usize * es].chunks_exact(es) {
            let be = self.big_endian;
            let (tag, ty) = (get_u16(&e[0..2], be), get_u16(&e[2..4], be));
            let count = self.read_field(&e[4..4 + fw]);
            let val = &e[4 + fw..]; // the value/offset field
            // Only the tags prediction needs are parsed; a value too large for
            // scalar use below simply fails the read (`values` handles arrays).
            match tag {
                T_WIDTH => p.width = self.value(ty, count, val)? as u32,
                T_HEIGHT => p.height = self.value(ty, count, val)? as u32,
                T_BITS => {
                    p.bits = self
                        .values(ty, count, val)?
                        .into_iter()
                        .map(|v| v as u16)
                        .collect()
                }
                T_COMPRESSION => p.compression = self.value(ty, count, val)? as u16,
                T_PHOTOMETRIC => p.photometric = self.value(ty, count, val)? as u16,
                T_STRIP_OFFSETS => p.strip_offsets = self.values(ty, count, val)?,
                T_SPP => p.spp = self.value(ty, count, val)? as u16,
                T_STRIP_COUNTS => p.strip_counts = self.values(ty, count, val)?,
                T_PLANAR => p.planar = self.value(ty, count, val)? as u16,
                T_PREDICTOR => p.predictor = self.value(ty, count, val)? as u16,
                T_SAMPLE_FORMAT => {
                    p.sample_format = self.values(ty, count, val)?.first().copied()? as u16
                }
                T_TILE_WIDTH => p.tiled = true,
                _ => {}
            }
        }
        if p.width == 0 || p.height == 0 || p.bits.is_empty() || p.strip_offsets.is_empty() {
            return None;
        }
        if p.strip_offsets.len() != p.strip_counts.len() {
            return None;
        }
        Some(p)
    }

    /// A single scalar tag value.
    fn value(&mut self, ty: u16, count: u64, val: &[u8]) -> Option<u64> {
        if count != 1 {
            return None;
        }
        self.values(ty, count, val).map(|v| v[0])
    }

    /// A tag's values (SHORT / LONG / BigTIFF LONG8), inline in the value/offset
    /// field when they fit (≤ 4 bytes classic, ≤ 8 BigTIFF), else read from the
    /// offset it holds.
    fn values(&mut self, ty: u16, count: u64, val: &[u8]) -> Option<Vec<u64>> {
        let each = match ty {
            3 => 2,  // SHORT
            4 => 4,  // LONG
            16 => 8, // LONG8 (BigTIFF)
            _ => return None,
        };
        let total = each * count as usize;
        let inline;
        let bytes: &[u8] = if total <= self.field_width() {
            &val[..total]
        } else {
            let off = self.read_field(val);
            let mut buf = vec![0u8; total];
            read_at(&mut self.file, off, &mut buf)?;
            inline = buf;
            &inline
        };
        Some(
            bytes
                .chunks_exact(each)
                .map(|c| match each {
                    2 => get_u16(c, self.big_endian) as u64,
                    4 => get_u32(c, self.big_endian) as u64,
                    _ => get_u64(c, self.big_endian),
                })
                .collect(),
        )
    }
}

impl PageIfd {
    /// Whether another page has the same shape as this one — everything that
    /// must be equal between pages for stride prediction (positions may differ;
    /// those are checked against the stride separately).
    fn same_shape(&self, o: &PageIfd) -> bool {
        self.entries == o.entries
            && self.width == o.width
            && self.height == o.height
            && self.bits == o.bits
            && self.spp == o.spp
            && self.compression == o.compression
            && self.photometric == o.photometric
            && self.sample_format == o.sample_format
            && self.planar == o.planar
            && self.predictor == o.predictor
            && !self.tiled
            && !o.tiled
            && self.strip_counts == o.strip_counts
    }
}

impl Default for PageIfd {
    fn default() -> Self {
        PageIfd {
            entries: 0,
            width: 0,
            height: 0,
            bits: Vec::new(),
            spp: 1,
            compression: 1,
            photometric: 1,
            sample_format: 1,
            planar: 1,
            predictor: 1,
            tiled: false,
            strip_offsets: Vec::new(),
            strip_counts: Vec::new(),
            next: 0,
        }
    }
}

/// Every strip of a page must sit exactly `shift` bytes after the template's.
fn offsets_at_stride(offs: &[u64], base: &[u64], shift: u64) -> bool {
    offs.len() == base.len() && offs.iter().zip(base).all(|(o, b)| *o == b + shift)
}

/// Read exactly `buf.len()` bytes at absolute `off`. `None` on any short read
/// or I/O error — validation treats both as "not a valid page there".
fn read_at(file: &mut File, off: u64, buf: &mut [u8]) -> Option<()> {
    file.seek(SeekFrom::Start(off)).ok()?;
    file.read_exact(buf).ok()
}

fn get_u16(b: &[u8], big_endian: bool) -> u16 {
    let b: [u8; 2] = b[..2].try_into().unwrap();
    if big_endian {
        u16::from_be_bytes(b)
    } else {
        u16::from_le_bytes(b)
    }
}

fn get_u32(b: &[u8], big_endian: bool) -> u32 {
    let b: [u8; 4] = b[..4].try_into().unwrap();
    if big_endian {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    }
}

fn get_u64(b: &[u8], big_endian: bool) -> u64 {
    let b: [u8; 8] = b[..8].try_into().unwrap();
    if big_endian {
        u64::from_be_bytes(b)
    } else {
        u64::from_le_bytes(b)
    }
}

// ---- the Fast-jump feature entry points ------------------------------------

/// Whether a fast path (jump / offset discovery) can work on this media at all,
/// measured from its (first) file. `Err(reason)` is user-facing — it's shown in
/// the frame bar's **Load offsets** hover text, and gates whether the **Load
/// offsets fast** button appears. Cheap — a few small header reads — but file
/// I/O, so callers cache it per pane.
pub fn availability(media: &Media) -> Result<(), String> {
    match media {
        Media::TiffSeq(t) => FastScan::open(&t.path).map(drop),
        Media::ConcatSeq(c) => match c.files.first() {
            Some(f) => FastScan::open(f).map(drop),
            None => Err("empty sequence".into()),
        },
        Media::FileSeq(_) => {
            Err("a still run's length is already known — seek via the frame readout".into())
        }
        Media::Still(_) => Err("not a multi-page sequence".into()),
    }
}

/// "Load offsets fast": discover the sequence's **whole** length in one pass of
/// binary searches (each file's exact page count in ~log₂(pages) header reads —
/// [`FastScan::page_count`]) instead of probing every frame, then mark the
/// timeline fully known so the frame readout can seek anywhere instantly. On
/// `Err` **nothing changes** (the caller falls back to the ordinary
/// metadata-only discovery). A `ConcatSeq` counts every file and builds the
/// complete global map, verified against whatever prefix ordinary discovery had
/// already built.
pub fn fast_load_offsets(media: &mut Media) -> Result<(), String> {
    let paths = offset_paths(media)
        .ok_or("fast offset discovery only applies to multi-page TIFF sequences")?;
    let counts = scan_offset_counts(&paths)?;
    apply_offset_counts(media, &counts)
}

/// The file(s) an offset scan would measure — one path for a lone TIFF, every
/// file for a concatenation — or `None` for media with no lazily-discovered
/// length to complete (a still, or a numbered run whose length is already known).
/// A cheap variant match, no I/O, so the UI thread can decide whether to queue a
/// background scan without touching the disk.
pub fn offset_paths(media: &Media) -> Option<Vec<PathBuf>> {
    match media {
        Media::TiffSeq(t) => Some(vec![t.path.clone()]),
        Media::ConcatSeq(c) => Some(c.files.clone()),
        _ => None,
    }
}

/// Measure each file's exact page count by binary search (§4) — the I/O-bound
/// half of a fast offset discovery, split out so it can run **off the UI thread**
/// (`crate::offsets`), taking only paths (never the pane's `Media`). `Err` means
/// the layout isn't fast-scannable; the caller then leaves the sequence to
/// discover its length lazily.
pub fn scan_offset_counts(paths: &[PathBuf]) -> Result<Vec<usize>, String> {
    if paths.is_empty() {
        return Err("empty sequence".into());
    }
    let mut counts = Vec::with_capacity(paths.len());
    for path in paths {
        counts.push(FastScan::open(path)?.page_count()?);
    }
    Ok(counts)
}

/// Apply page counts measured by [`scan_offset_counts`] to `media`: grow its
/// known length to the total and mark the end, so the whole timeline is instantly
/// seekable. The mutating half, run on the UI thread. A `ConcatSeq` verifies the
/// counts against whatever prefix ordinary discovery already built (`Err`,
/// nothing changed, on disagreement — see `set_full_layout`).
pub fn apply_offset_counts(media: &mut Media, counts: &[usize]) -> Result<(), String> {
    match media {
        Media::TiffSeq(t) => {
            let count = *counts
                .first()
                .ok_or("no page count measured for the sequence")?;
            t.frames.note_len_to(count);
            t.at_end = true;
            Ok(())
        }
        Media::ConcatSeq(c) => c.set_full_layout(counts),
        _ => Err("fast offset discovery only applies to multi-page TIFF sequences".into()),
    }
}

/// Jump straight to global frame `target`: validate + decode it at its
/// predicted position and grow the known length through it in one step, without
/// walking (or decoding) anything in between. On `Err` **nothing changes** —
/// the caller cancels the jump (no fallback discovery is started).
///
/// A `ConcatSeq` works too: each file before the target is opened and its exact
/// page count found by binary search (O(log pages) header reads per file), the
/// global map is extended through the target — verified against whatever prefix
/// ordinary discovery had already built — and the target page decodes from its
/// own file. Runs synchronously on the caller's thread: a handful of tiny reads
/// per file, not a decode sweep.
pub fn fast_jump(media: &mut Media, target: usize) -> Result<(), String> {
    match media {
        Media::TiffSeq(t) => {
            let mut scan = FastScan::open(&t.path)?;
            let frame = scan
                .read_page(target)
                .ok_or("no page at the predicted position (past the end, or irregular)")?;
            t.frames.note_len_to(target + 1);
            t.frames.insert(target, Arc::new(frame));
            Ok(())
        }
        Media::ConcatSeq(c) => {
            let mut counts = Vec::new();
            let mut before = 0usize; // frames in fully counted files
            for (i, path) in c.files.clone().iter().enumerate() {
                let mut scan = FastScan::open(path)?;
                let n = scan.page_count()?;
                if before + n > target {
                    let page = target - before;
                    let frame = scan
                        .read_page(page)
                        .ok_or("no page at the predicted position")?;
                    c.extend_known(&counts, i, page)?;
                    c.frames.insert(target, Arc::new(frame));
                    return Ok(());
                }
                counts.push(n);
                before += n;
            }
            Err("the target frame is past the end of the run".into())
        }
        _ => Err("fast jump only applies to multi-page TIFF sequences".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{load, load_sequence, SeqReader};
    use crate::testutil::{
        fixture_dir, write_bilevel_tiff, write_multipage_bigtiff_u16, write_multipage_tiff_u16,
    };

    fn u16s(f: &FrameData) -> &[u16] {
        match &f.samples {
            Samples::U16(v) => v,
            _ => panic!("expected u16 samples"),
        }
    }

    #[test]
    fn stride_layout_measures_reads_and_counts() {
        let dir = fixture_dir("fastscan");
        let path = dir.join("run.tif");
        write_multipage_tiff_u16(&path, &[[9, 7]; 5]);

        let mut scan = FastScan::open(&path).expect("uniform pages measure");
        assert_eq!(scan.page_count().unwrap(), 5);
        assert!(scan.validate(4));
        assert!(!scan.validate(5)); // past the end
        assert!(scan.read_page(7).is_none());

        // Bit-exact with the ordinary chain-walking decode.
        let mut reader = SeqReader::open(&path).unwrap();
        for k in [0usize, 3, 4] {
            let fast = scan.read_page(k).expect("page reads at prediction");
            let slow = reader.decode(k).unwrap().expect("page exists");
            assert_eq!(fast.size, slow.size);
            assert_eq!(u16s(&fast), u16s(&slow), "page {k} must match");
        }
    }

    #[test]
    fn bigtiff_measures_reads_and_jumps() {
        let dir = fixture_dir("fastscan_big");
        let path = dir.join("big.tif");
        write_multipage_bigtiff_u16(&path, &[[9, 7]; 6]);

        // The 64-bit-offset layout measures and counts just like a classic one.
        let mut scan = FastScan::open(&path).expect("BigTIFF measures");
        assert!(scan.big, "should be detected as BigTIFF");
        assert_eq!(scan.page_count().unwrap(), 6);

        // Raw decode is bit-exact with the tiff-crate chain walk.
        let mut reader = SeqReader::open(&path).unwrap();
        for k in [0usize, 2, 5] {
            let fast = scan.read_page(k).expect("page reads at prediction");
            let slow = reader.decode(k).unwrap().expect("page exists");
            assert_eq!(u16s(&fast), u16s(&slow), "BigTIFF page {k} must match");
        }

        // The whole feature works end to end on a BigTIFF.
        let mut media = load(&path).unwrap();
        fast_load_offsets(&mut media).expect("regular BigTIFF");
        assert_eq!(media.frame_count(), 6);
        assert!(media.at_end());
    }

    #[test]
    fn far_jumps_hold_on_a_long_sequence() {
        let dir = fixture_dir("fastscan_long");
        let path = dir.join("long.tif");
        write_multipage_tiff_u16(&path, &[[4, 3]; 300]);

        let mut scan = FastScan::open(&path).unwrap();
        assert_eq!(scan.page_count().unwrap(), 300);

        let mut media = load(&path).unwrap();
        fast_jump(&mut media, 271).expect("far jump");
        assert_eq!(media.frame_count(), 272);
        let jumped = media.resident(271).unwrap();
        let slow = SeqReader::open(&path).unwrap().decode(271).unwrap().unwrap();
        assert_eq!(u16s(&jumped), u16s(&slow));
    }

    #[test]
    fn measure_rejects_unpredictable_layouts() {
        let dir = fixture_dir("fastscan_rej");

        // Pages differing in size: no uniform stride.
        let mixed = dir.join("mixed.tif");
        write_multipage_tiff_u16(&mixed, &[[9, 7], [5, 4], [9, 7]]);
        assert!(FastScan::open(&mixed).is_err());

        // Single page: nothing to measure a stride from.
        let single = dir.join("single.tif");
        write_multipage_tiff_u16(&single, &[[9, 7]]);
        assert!(FastScan::open(&single).is_err());

        // 1-bit bilevel mask: unsupported bit depth (and single-page anyway).
        let mask = dir.join("mask.tif");
        write_bilevel_tiff(&mask, 8, 4, &[1u8; 32], false);
        assert!(FastScan::open(&mask).is_err());
    }

    #[test]
    fn fast_jump_grows_a_tiff_seq_without_discovery() {
        let dir = fixture_dir("fastjump");
        let path = dir.join("run.tif");
        write_multipage_tiff_u16(&path, &[[9, 7]; 6]);

        let mut media = load(&path).unwrap();
        assert_eq!(media.frame_count(), 1); // only page 0 known

        fast_jump(&mut media, 4).expect("jump inside the file");
        assert_eq!(media.frame_count(), 5); // known through the target
        assert!(!media.at_end()); // nothing said page 5 doesn't exist
        let jumped = media.resident(4).expect("target decoded by the jump");
        let slow = SeqReader::open(&path).unwrap().decode(4).unwrap().unwrap();
        assert_eq!(u16s(&jumped), u16s(&slow));

        // Past the real end: the jump cancels and nothing changes.
        assert!(fast_jump(&mut media, 10).is_err());
        assert_eq!(media.frame_count(), 5);
    }

    #[test]
    fn scan_and_apply_offsets_split_matches_and_rejects_irregular() {
        let dir = fixture_dir("scan_split");

        // A regular run: measurement (off-thread half) returns the per-file
        // counts, and applying them (UI-thread half) completes the length.
        let path = dir.join("run.tif");
        write_multipage_tiff_u16(&path, &[[6, 5]; 7]);
        let mut media = load(&path).unwrap();
        let paths = offset_paths(&media).expect("a TIFF seq exposes its file");
        let counts = scan_offset_counts(&paths).expect("regular layout measures");
        assert_eq!(counts, vec![7]);
        assert_eq!(media.frame_count(), 1); // nothing applied yet
        apply_offset_counts(&mut media, &counts).expect("apply");
        assert_eq!(media.frame_count(), 7);
        assert!(media.at_end());

        // An irregular (mixed-size) run isn't fast-scannable: the measurement
        // Errs, so the auto-scan leaves the sequence to discover lazily.
        let mixed = dir.join("mixed.tif");
        write_multipage_tiff_u16(&mixed, &[[9, 7], [5, 4], [9, 7]]);
        let mixed_media = load(&mixed).unwrap();
        let mixed_paths = offset_paths(&mixed_media).unwrap();
        assert!(scan_offset_counts(&mixed_paths).is_err());
    }

    #[test]
    fn fast_load_offsets_completes_length_in_one_step() {
        let dir = fixture_dir("fastoffsets");

        // Lone TIFF: the whole length is discovered and the end is marked, with
        // no frame decoded (offsets only).
        let path = dir.join("run.tif");
        write_multipage_tiff_u16(&path, &[[6, 5]; 9]);
        let mut media = load(&path).unwrap();
        assert_eq!(media.frame_count(), 1);
        assert!(!media.at_end());
        fast_load_offsets(&mut media).expect("regular layout");
        assert_eq!(media.frame_count(), 9);
        assert!(media.at_end());
        assert_eq!(media.resident_count(), 0); // offsets only — no page decoded

        // Concatenation: every file counted, one seamless map, end marked.
        let a = dir.join("c_000.tif");
        let b = dir.join("c_001.tif");
        write_multipage_tiff_u16(&a, &[[6, 5]; 2]);
        write_multipage_tiff_u16(&b, &[[6, 5]; 3]);
        let mut concat = load_sequence(&[a, b], "c".into()).unwrap();
        fast_load_offsets(&mut concat).expect("regular concat");
        assert_eq!(concat.frame_count(), 5);
        assert!(concat.at_end());
        let (_, map) = concat.concat_layout().unwrap();
        assert_eq!(map, vec![(0, 0), (0, 1), (1, 0), (1, 1), (1, 2)]);
    }

    #[test]
    fn fast_jump_spans_concatenated_files() {
        let dir = fixture_dir("fastjump_concat");
        let a = dir.join("run_000.tif");
        let b = dir.join("run_001.tif");
        write_multipage_tiff_u16(&a, &[[9, 7]; 3]);
        write_multipage_tiff_u16(&b, &[[9, 7]; 4]);

        let mut media = load_sequence(&[a, b.clone()], "run".into()).unwrap();
        // Global frame 5 = file 1, page 2 — counted across the file boundary.
        fast_jump(&mut media, 5).expect("jump across files");
        assert_eq!(media.frame_count(), 6);
        let jumped = media.resident(5).expect("target decoded");
        let slow = SeqReader::open(&b).unwrap().decode(2).unwrap().unwrap();
        assert_eq!(u16s(&jumped), u16s(&slow));

        // The discovered map must agree with the measured counts.
        let (_, map) = media.concat_layout().unwrap();
        assert_eq!(map, vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)]);

        // Past the total page count: cancelled, nothing changes.
        assert!(fast_jump(&mut media, 10).is_err());
        assert_eq!(media.frame_count(), 6);
    }
}
