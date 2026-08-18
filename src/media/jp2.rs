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

use rust_i18n::t;
use std::path::Path;

use anyhow::{anyhow, Result};
use hayro_jpeg2000::{ColorSpace, DecodeSettings, DecoderContext, Image};

use super::{FrameData, Samples};

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

/// Decode a JPEG 2000 file into a `FrameData` at native bit depth.
pub fn decode_jp2(path: &Path) -> Result<FrameData> {
    let data = std::fs::read(path)
        .map_err(|e| anyhow!(t!("error.jp2_read", path = path.display(), err = e).into_owned()))?;
    decode_jp2_bytes(&data, path)
}

/// The body of [`decode_jp2`], split off so the tests can decode an embedded
/// fixture without a file. `path` is only used to name the file in errors.
fn decode_jp2_bytes(data: &[u8], path: &Path) -> Result<FrameData> {
    let fail = |err: String| {
        anyhow!(t!("error.jp2_decode", path = path.display(), err = err).into_owned())
    };

    let image = Image::new(data, &DecodeSettings::default()).map_err(|e| fail(format!("{e:?}")))?;
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
        decode_jp2_bytes(data, Path::new("fixture.jp2")).expect("decode")
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
        let err = decode_jp2_bytes(b"not a jpeg 2000 file at all", Path::new("bad.jp2"));
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
}
