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

mod fastscan;
mod loader;
mod percentile;
mod render;
mod source;
mod stats;

pub use fastscan::{availability as fast_jump_availability, fast_jump, fast_load_offsets};
pub use loader::{decode_file, load, load_sequence, SeqReader};
pub use render::ToneLut;
pub use source::{DecodeReq, Media};
pub use stats::{diff_frames, reduce_frames, HistData, Reduce, RegionStats};

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use tiff::encoder::{colortype, TiffEncoder};

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

    /// Channels that carry colour (alpha excluded) — used for stats and to gate
    /// the mono-only Colormap tone.
    pub(crate) fn color_channels(&self) -> usize {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::loader::mask_bits;
    use crate::testutil::*;
    use std::sync::Arc;

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

    /// The incremental LRU peeks the oldest resident frame (by `last_used`),
    /// never the protected/shown one, and stays correct as frames are touched
    /// and evicted — the property `enforce_cache_budget` relies on.
    #[test]
    fn lru_evicts_oldest_and_protects_shown() {
        let dir = fixture_dir("lru");
        let path = dir.join("seq.tif");
        write_multipage_tiff_u16(&path, &[[16, 16], [16, 16], [16, 16], [16, 16]]);
        let mut m = load(&path).expect("open tiff");

        // Make frames 0..4 resident with strictly increasing recency (frame i
        // used at tick i+1), so frame 0 is the least recently used.
        for i in 0..4 {
            let f = SeqReader::open(&path).unwrap().decode(i).unwrap().expect("page");
            m.insert(i, Arc::new(f));
            m.touch(i, i as u64 + 1);
        }

        // Protecting the shown frame 0, the oldest evictable is frame 1.
        assert_eq!(m.lru_evictable(0).map(|(t, f, _)| (t, f)), Some((2, 1)));

        // Re-touching frame 1 to newest moves it to the back of the order.
        m.touch(1, 99);
        assert_eq!(m.lru_evictable(0).map(|(_, f, _)| f), Some(2));

        // Eviction walks the recency order: 2, then 3, then 1.
        m.evict(2);
        assert_eq!(m.lru_evictable(0).map(|(_, f, _)| f), Some(3));
        m.evict(3);
        assert_eq!(m.lru_evictable(0).map(|(_, f, _)| f), Some(1));
        m.evict(1);
        assert_eq!(m.lru_evictable(0), None); // only the shown frame is left
    }

}

