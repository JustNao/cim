//! JPEG 2000 stills (`.jp2` and raw `.j2k`/`.j2c`/`.jpc` codestreams).
//!
//! Decoding is **in-process** (`hayro-jpeg2000`), not a shell-out like video:
//! the machines cim runs on don't all have ffmpeg, and the crate is pure Rust
//! with no transitive dependencies, so supporting the format costs the build
//! nothing but a Cargo entry — no C toolchain, which the openjpeg-backed
//! crates would have needed.
//!
//! What matters here is the same thing as everywhere else in `media`: samples
//! come out at **native bit depth**. The decoder hands back one `f32` plane
//! per component plus that component's precision, so a 12-bit image stays
//! 12-bit values in a `u16` and the readout shows the numbers the file holds.
//! Lossless files decode bit-exactly (the tests pin that); a lossy (9/7)
//! file's reconstruction is real-valued and can land just outside the nominal
//! range, so samples are rounded and clamped to the component's own depth.
//!
//! # Resolution levels — why a `.jp2` is not always decoded whole
//!
//! JPEG 2000 is a wavelet codec, so the codestream *contains* a pyramid: asking
//! for half, quarter, … resolution decodes fewer subbands rather than decoding
//! everything and throwing it away. That is the difference between opening a
//! 25000² satellite tile and not, measured on a comparable file:
//!
//! | level | decode | peak RSS |
//! |-------|--------|----------|
//! | full  | 2.2 s  | 972 MB   |
//! | 1/4   | 294 ms | 72 MB    |
//! | 1/32  | 21 ms  | 12 MB    |
//!
//! (8192², ~1.1 bit/px; a 25000² tile is ~9× that — some 25 s and ~9 GB whole.)
//! So a file whose full size exceeds [`budget_px`] is decoded at the finest
//! level that fits, and **the pane is then that smaller image**: its size, its
//! cursor readout, its histogram and its export all describe what was loaded,
//! and the media's name carries the reduction (`tile.jp2 (1/8)`) so it is never
//! a silent substitution. The samples are the wavelet's lowpass — averages, not
//! a decimation of true samples — which is why this is opt-out-able and why the
//! reduction is stated rather than hidden.
//!
//! The **codestream is kept** beside the decoded frame (91 MB against the
//! gigabytes the full image would cost), so changing the budget re-levels every
//! open pane without touching the disk — see [`Jp2Cache`] and `relevel`.

use rust_i18n::t;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use hayro_jpeg2000::{ColorSpace, DecodeSettings, DecoderContext, Image};

use super::{FrameData, Samples};

/// The decoded-pixel ceiling for one JPEG 2000 image, in **pixels** (not
/// megapixels): `Config::jp2_max_mp` × 1e6, or 0 for "decode whole".
///
/// Process-global on purpose. Every decode path — opening a pane, a numbered
/// run's frames, the export worker's own reader — must agree on the level, or
/// an exported frame would not be the frame on screen (§10's parity rule).
/// Threading a config value through all of them is exactly the drift that rule
/// exists to prevent.
static BUDGET_PX: AtomicUsize = AtomicUsize::new(DEFAULT_BUDGET_PX);

/// Default ceiling: 32 MP (≈5657², 64 MB as `u16`) — comfortably more detail
/// than a 4K screen shows, and about a second of decode on a big tile.
pub const DEFAULT_BUDGET_PX: usize = 32_000_000;

/// Set the decoded-pixel ceiling (from `Config::jp2_max_mp`).
pub fn set_budget_px(px: usize) {
    BUDGET_PX.store(px, Ordering::Relaxed);
}

/// The current ceiling; 0 = decode at full resolution.
pub fn budget_px() -> usize {
    BUDGET_PX.load(Ordering::Relaxed)
}

/// The resolution level to decode `native` at under a `budget` of decoded
/// pixels: 0 = full, 1 = half each way (a quarter of the pixels), 2 = a
/// sixteenth, … Chosen as the **finest** level that fits, so an image already
/// under the budget is never reduced.
pub fn level_for(native: [usize; 2], budget: usize) -> u32 {
    if budget == 0 {
        return 0;
    }
    let mut level = 0u32;
    // Deliberately `>>` per step rather than solving it in one go: this is the
    // same halving the codec does, so the predicted size matches what comes back.
    while level < MAX_LEVEL && level_size(native, level).iter().product::<usize>() > budget {
        level += 1;
    }
    level
}

/// The decode target for `native` under `budget` — `None` when it already fits
/// (decode whole), else the reduced level's size.
fn target_for(native: [usize; 2], budget: usize) -> Option<[usize; 2]> {
    match level_for(native, budget) {
        0 => None,
        l => Some(level_size(native, l)),
    }
}

/// Never reduce past this, however small the budget — beyond it the image is a
/// thumbnail and the pane would be showing nothing useful.
const MAX_LEVEL: u32 = 8;

/// The pixel size `native` decodes to at `level` (each level halves, rounding
/// up, which is what the codec's own shrink factor does).
pub fn level_size(native: [usize; 2], level: u32) -> [usize; 2] {
    let d = 1usize << level.min(MAX_LEVEL);
    [native[0].div_ceil(d).max(1), native[1].div_ceil(d).max(1)]
}

/// The codestream kept beside a decoded JPEG 2000 still, so the image can be
/// re-levelled without re-reading (and re-parsing) the file. The bytes are a
/// fraction of the decoded frame — the whole point of holding them.
pub struct Jp2Cache {
    /// The file's bytes, shared so a re-level clones no data.
    pub bytes: Arc<[u8]>,
    /// Full-resolution size from the header, whatever level is decoded.
    pub native: [usize; 2],
    /// The level the resident frame was decoded at (0 = full).
    pub level: u32,
    /// The media's name without the level suffix, so it can be rebuilt.
    pub base_name: String,
}

impl Jp2Cache {
    /// The name a pane shows: the file, plus the reduction when there is one.
    /// Never silent — a reduced image says so wherever the media is named.
    pub fn display_name(&self) -> String {
        if self.level == 0 {
            self.base_name.clone()
        } else {
            format!("{} (1/{})", self.base_name, 1usize << self.level)
        }
    }
}

/// The extensions this module claims: the JP2 container and the raw
/// codestream forms (the decoder sniffs which one it was handed).
pub const EXTS: &[&str] = &["jp2", "j2k", "j2c", "jpc"];

/// Does this lowercased extension name a JPEG 2000 file?
pub fn handles(ext: &str) -> bool {
    EXTS.contains(&ext)
}

/// How the decoded components map onto a `FrameData`: how many of them to
/// keep, and the channel count that produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// Components to read, in order (the leading colour ones, plus alpha).
    pub used: usize,
    /// `FrameData` channels — 1, 3 or 4, the shapes the rest of cim renders.
    pub channels: usize,
}

/// Decide the frame shape from the image's colour space.
///
/// Grey + alpha keeps only the grey plane, exactly as the `image` still path
/// drops `La8`'s alpha; RGB keeps its alpha when there is one. A colour space
/// cim has no rendering for (CMYK, or an ICC/unknown space with a component
/// count that isn't 1 or 3) is refused rather than guessed at — passing four
/// CMYK planes off as RGBA would show a wrong image, which is worse than an
/// error in a tool whose whole point is pixel accuracy.
pub fn shape_for(cs: &ColorSpace, has_alpha: bool, comps: usize) -> Option<Shape> {
    let colour = match cs {
        ColorSpace::Gray => 1,
        ColorSpace::RGB => 3,
        // An unknown / ICC space is usable when it has a shape we can draw.
        ColorSpace::Unknown { num_channels } | ColorSpace::Icc { num_channels, .. } => {
            match num_channels {
                1 => 1,
                3 => 3,
                _ => return None,
            }
        }
        ColorSpace::CMYK => return None,
    };
    if comps < colour {
        return None;
    }
    Some(match (colour, has_alpha && comps > colour) {
        (1, _) => Shape {
            used: 1,
            channels: 1,
        },
        (_, false) => Shape {
            used: 3,
            channels: 3,
        },
        (_, true) => Shape {
            used: 4,
            channels: 4,
        },
    })
}

/// Decode a JPEG 2000 file into a `FrameData` at native bit depth, reduced to
/// the current [`budget_px`] (see the module docs). Used by the standalone-file
/// paths — a numbered run's frames and the export reader — which share the
/// budget so they land on the same image the pane shows.
pub fn decode_jp2(path: &Path) -> Result<FrameData> {
    let data = read_file(path)?;
    let target = match probe_native(&data, path) {
        Ok(native) => target_for(native, budget_px()),
        Err(_) => None, // let the decode below report the real problem
    };
    decode_jp2_bytes(&data, path, target)
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| anyhow!(t!("error.jp2_read", path = path.display(), err = e).into_owned()))
}

/// The image's **full-resolution** size, from the main header alone — no
/// packets are decoded, so this is cheap enough to do before choosing a level.
fn probe_native(data: &[u8], path: &Path) -> Result<[usize; 2]> {
    let image = Image::new(data, &DecodeSettings::default()).map_err(|e| {
        anyhow!(t!(
            "error.jp2_decode",
            path = path.display(),
            err = format!("{e:?}")
        )
        .into_owned())
    })?;
    Ok([image.width() as usize, image.height() as usize])
}

/// Open a `.jp2` as a still: decode it at the level the budget allows and keep
/// the codestream beside it ([`Jp2Cache`]) so the level can be changed later
/// without re-reading the file.
pub(super) fn open_jp2(path: &Path, name: String) -> Result<(FrameData, Jp2Cache)> {
    let data = read_file(path)?;
    let native = probe_native(&data, path)?;
    let level = level_for(native, budget_px());
    let frame = decode_jp2_bytes(&data, path, target_for(native, budget_px()))?;
    Ok((
        frame,
        Jp2Cache {
            bytes: Arc::from(data.into_boxed_slice()),
            native,
            level,
            base_name: name,
        },
    ))
}

/// Re-decode a cached codestream at the level `budget` now implies, updating
/// the cache. `Ok(None)` = the level didn't change, so nothing was decoded.
pub(super) fn relevel(cache: &mut Jp2Cache, budget: usize) -> Result<Option<FrameData>> {
    let level = level_for(cache.native, budget);
    if level == cache.level {
        return Ok(None);
    }
    // No file read: this is what keeping the codestream buys.
    let frame = decode_jp2_bytes(
        &cache.bytes,
        Path::new(&cache.base_name),
        target_for(cache.native, budget),
    )?;
    cache.level = level;
    Ok(Some(frame))
}

/// The body of [`decode_jp2`], split off so the tests can decode an embedded
/// fixture without a file. `path` is only used to name the file in errors;
/// `target` is the level's pixel size (`None` = decode at full resolution —
/// see the module docs).
fn decode_jp2_bytes(data: &[u8], path: &Path, target: Option<[usize; 2]>) -> Result<FrameData> {
    let fail = |err: String| {
        anyhow!(t!("error.jp2_decode", path = path.display(), err = err).into_owned())
    };

    let mut settings = DecodeSettings::default();
    // The crate takes a target *size* and turns it into a count of skipped
    // resolution levels (a power of two), so handing it the level's own size
    // asks for exactly that level.
    if let Some([tw, th]) = target {
        settings.target_resolution = Some((tw as u32, th as u32));
    }
    let image = Image::new(data, &settings).map_err(|e| fail(format!("{e:?}")))?;
    let mut ctx = DecoderContext::default();
    let decoded = image.decode(&mut ctx).map_err(|e| fail(format!("{e:?}")))?;
    let comps = decoded.components();

    let shape =
        shape_for(image.color_space(), image.has_alpha(), comps.len()).ok_or_else(|| {
            anyhow!(t!(
                "error.jp2_color_space",
                path = path.display(),
                kind = format!("{:?}", image.color_space()),
                n = comps.len()
            )
            .into_owned())
        })?;

    // The decoder resolves sub-sampling itself, so every component should span
    // the image grid — but the frame's size and its sample count have to agree
    // or every later index (readout, stats, export) is off, so check rather
    // than assume.
    let (w, h) = (image.width() as usize, image.height() as usize);
    let px = w * h;
    let used = &comps[..shape.used];
    if px == 0 || used.iter().any(|c| c.samples().len() != px) {
        return Err(anyhow!(t!(
            "error.jp2_geometry",
            path = path.display(),
            w = w,
            h = h
        )
        .into_owned()));
    }

    // A component's own precision bounds its values; the widest one decides
    // whether the frame is 8- or 16-bit.
    let depth = used.iter().map(|c| c.bit_depth()).max().unwrap_or(8);
    let planes: Vec<(&[f32], f32)> = used
        .iter()
        .map(|c| (c.samples(), max_value(c.bit_depth())))
        .collect();

    let samples = if depth <= 8 {
        let mut out = vec![0u8; px * shape.channels];
        interleave(&planes, &mut out, |v| v as u8);
        Samples::U8(out)
    } else {
        let mut out = vec![0u16; px * shape.channels];
        interleave(&planes, &mut out, |v| v as u16);
        Samples::U16(out)
    };
    Ok(FrameData::new([w, h], shape.channels, samples))
}

/// Largest value a component of `bits` precision can hold.
fn max_value(bits: u8) -> f32 {
    ((1u32 << bits.clamp(1, 16)) - 1) as f32
}

/// Interleave the component planes into one buffer, rounding each sample and
/// clamping it into its component's range (a lossy reconstruction overshoots).
fn interleave<T: Copy>(planes: &[(&[f32], f32)], out: &mut [T], to: impl Fn(f32) -> T) {
    let n = planes.len();
    for (c, (plane, max)) in planes.iter().enumerate() {
        for (i, &v) in plane.iter().enumerate() {
            out[i * n + c] = to(v.round().clamp(0.0, *max));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lossless 64×32 fixtures, decoded back to the exact values they were
    /// written from (see `src/testdata/README.md`). JPEG 2000 has no encoder
    /// in the tree, so unlike the TIFF/PNG fixtures these are checked in
    /// rather than generated — they are a few hundred bytes each.
    const MONO16: &[u8] = include_bytes!("../testdata/mono16.jp2");
    const RGB8: &[u8] = include_bytes!("../testdata/rgb8.jp2");

    fn decode(data: &[u8]) -> FrameData {
        decode_jp2_bytes(data, Path::new("fixture.jp2"), None).expect("decode")
    }

    #[test]
    fn mono_decodes_bit_exactly_at_native_depth() {
        let f = decode(MONO16);
        assert_eq!(f.size, [64, 32]);
        assert_eq!(f.channels, 1);
        assert!(f.hi_depth(), "16-bit source stays 16-bit");
        let Samples::U16(v) = &f.samples else {
            panic!("expected u16 samples");
        };
        assert_eq!(v.len(), 64 * 32);
        // The fixture holds `(y*64 + x) * 17 mod 65536`, losslessly coded.
        for (i, &got) in v.iter().enumerate() {
            assert_eq!(got as usize, (i * 17) % 65536, "pixel {i}");
        }
    }

    #[test]
    fn rgb_decodes_bit_exactly_through_the_colour_transform() {
        let f = decode(RGB8);
        assert_eq!(f.size, [64, 32]);
        assert_eq!(f.channels, 3);
        assert!(!f.hi_depth());
        let Samples::U8(v) = &f.samples else {
            panic!("expected u8 samples");
        };
        assert_eq!(v.len(), 64 * 32 * 3);
        for i in 0..64 * 32 {
            assert_eq!(
                (v[i * 3], v[i * 3 + 1], v[i * 3 + 2]),
                (
                    (i % 256) as u8,
                    ((i * 3) % 256) as u8,
                    ((i * 7) % 256) as u8
                ),
                "pixel {i}"
            );
        }
    }

    #[test]
    fn a_jp2_loads_as_a_still_media() {
        let dir = crate::testutil::fixture_dir("jp2");
        let path = dir.join("mono16.jp2");
        std::fs::write(&path, MONO16).expect("write fixture");
        let media = crate::media::load(&path).expect("load");
        assert_eq!(media.size(), [64, 32]);
        assert_eq!(media.frame_count(), 1);
        assert!(media.hi_depth());
        // The same file through the sequence/export entry point.
        let frame = crate::media::decode_file(&path).expect("decode_file");
        assert_eq!(frame.size, [64, 32]);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let err = decode_jp2_bytes(b"not a jpeg 2000 file at all", Path::new("bad.jp2"), None);
        assert!(err.is_err());
    }

    #[test]
    fn every_claimed_extension_is_loadable_and_recognised() {
        for ext in EXTS {
            assert!(handles(ext), "{ext}");
            assert!(
                crate::cli::LOADABLE_EXTS.contains(ext),
                "{ext} missing from LOADABLE_EXTS"
            );
        }
    }

    #[test]
    fn shape_follows_the_colour_space() {
        let gray = shape_for(&ColorSpace::Gray, false, 1).unwrap();
        assert_eq!(
            gray,
            Shape {
                used: 1,
                channels: 1
            }
        );
        // Grey + alpha drops the alpha, like the `image` path's `La8`.
        let gray_a = shape_for(&ColorSpace::Gray, true, 2).unwrap();
        assert_eq!(
            gray_a,
            Shape {
                used: 1,
                channels: 1
            }
        );
        assert_eq!(
            shape_for(&ColorSpace::RGB, false, 3).unwrap(),
            Shape {
                used: 3,
                channels: 3
            }
        );
        assert_eq!(
            shape_for(&ColorSpace::RGB, true, 4).unwrap(),
            Shape {
                used: 4,
                channels: 4
            }
        );
        // Alpha claimed but not delivered: keep the colour planes.
        assert_eq!(
            shape_for(&ColorSpace::RGB, true, 3).unwrap(),
            Shape {
                used: 3,
                channels: 3
            }
        );
        // Shapes cim can't draw are refused rather than guessed at.
        assert!(shape_for(&ColorSpace::CMYK, false, 4).is_none());
        assert!(shape_for(&ColorSpace::Unknown { num_channels: 2 }, false, 2).is_none());
        assert!(shape_for(&ColorSpace::RGB, false, 2).is_none());
        // An unknown space with a drawable shape is fine.
        assert_eq!(
            shape_for(&ColorSpace::Unknown { num_channels: 3 }, false, 3).unwrap(),
            Shape {
                used: 3,
                channels: 3
            }
        );
    }

    #[test]
    fn lossy_overshoot_is_clamped_into_the_component_range() {
        let plane = [-3.4f32, 0.4, 127.6, 255.49, 260.0];
        let mut out = vec![0u8; plane.len()];
        interleave(&[(&plane, max_value(8))], &mut out, |v| v as u8);
        assert_eq!(out, vec![0, 0, 128, 255, 255]);
    }

    #[test]
    fn the_level_is_the_finest_one_that_fits_the_budget() {
        let tile = [25000usize, 25000];
        // 625 MP against the 32 MP default: 1/4 is still 39 MP, so 1/8.
        assert_eq!(level_for(tile, 32_000_000), 3);
        assert_eq!(level_size(tile, 3), [3125, 3125]);
        assert!(level_size(tile, 3).iter().product::<usize>() <= 32_000_000);
        // One level finer would not have fit — "finest that fits", not "safest".
        assert!(level_size(tile, 2).iter().product::<usize>() > 32_000_000);
        // An image already under the budget is never reduced.
        assert_eq!(level_for([4096, 4096], 32_000_000), 0);
        assert_eq!(level_for([5000, 5000], 32_000_000), 0);
        // 0 = decode whole, whatever the size.
        assert_eq!(level_for(tile, 0), 0);
        // A tiny budget stops at the thumbnail floor rather than reducing forever.
        assert_eq!(level_for(tile, 1), MAX_LEVEL);
    }

    #[test]
    fn the_level_always_brings_the_image_under_the_budget() {
        for &budget in &[1_000_000usize, 8_000_000, 32_000_000, 256_000_000] {
            for &side in &[512usize, 5000, 25000, 100_000] {
                let native = [side, side / 2 + 1];
                let level = level_for(native, budget);
                let out = level_size(native, level);
                assert!(
                    out.iter().product::<usize>() <= budget || level == MAX_LEVEL,
                    "{side} at {budget}: level {level} -> {out:?}"
                );
            }
        }
    }

    #[test]
    fn a_reduced_image_says_so_in_its_name() {
        let cache = |level| Jp2Cache {
            bytes: Arc::from(Vec::new().into_boxed_slice()),
            native: [25000, 25000],
            level,
            base_name: "tile.jp2".into(),
        };
        assert_eq!(cache(0).display_name(), "tile.jp2");
        assert_eq!(cache(3).display_name(), "tile.jp2 (1/8)");
    }

    /// The point of keeping the codestream: a level change is a decode, not a
    /// file read — and it really does change the frame's size.
    #[test]
    fn releveling_uses_the_kept_codestream() {
        let mut cache = Jp2Cache {
            bytes: Arc::from(MONO16.to_vec().into_boxed_slice()),
            native: [64, 32],
            level: 0,
            base_name: "mono16.jp2".into(),
        };
        // 64×32 = 2048 px; a 512-pixel budget forces one halving.
        let frame = relevel(&mut cache, 512).expect("relevel").expect("changed");
        assert_eq!(cache.level, 1);
        assert_eq!(frame.size, [32, 16]);
        assert_eq!(cache.display_name(), "mono16.jp2 (1/2)");
        // Asking again for the same budget decodes nothing.
        assert!(relevel(&mut cache, 512).expect("relevel").is_none());
        // Back to whole.
        let frame = relevel(&mut cache, 0).expect("relevel").expect("changed");
        assert_eq!((cache.level, frame.size), (0, [64, 32]));
    }
}
