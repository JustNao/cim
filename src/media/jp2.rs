//! JPEG 2000 (`.jp2`) stills, decoded through the **ffmpeg CLI** — the same
//! external tool video loading (`media::video`) and export already use, so
//! adding the format costs no decode crate and no build-time C dependency.
//!
//! `ffprobe` reports the image's size and decoded pixel format; `ffmpeg` then
//! writes the single frame as rawvideo on a pipe. The output pixel format is
//! chosen to **preserve native values wherever ffmpeg can**: a mono image is
//! asked for at its own bit depth (`gray12le` stays 12-bit data in a `u16`),
//! so the readout, histograms and export see the samples the file holds. The
//! one place that isn't possible is a **colour** image deeper than 8 bits:
//! ffmpeg's packed RGB formats only come in 8 and 16 bits, so a 12-bit RGB
//! source is scaled up to 16 (documented, like the video path's CFR
//! assumption).

use rust_i18n::t;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

use super::video::{ffmpeg_hint, ffprobe_stream, field};
use super::{FrameData, Samples};

/// How one decoded JPEG 2000 image is asked for and read back: the rawvideo
/// pixel format handed to ffmpeg plus the `FrameData` shape it yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixLayout {
    /// The `-pix_fmt` ffmpeg writes on the pipe.
    pub fmt: &'static str,
    /// Samples per pixel in that format (1 mono, 3 RGB, 4 RGBA).
    pub channels: usize,
    /// Samples are 16-bit little-endian rather than bytes.
    pub wide: bool,
}

impl PixLayout {
    fn bytes_per_pixel(&self) -> usize {
        self.channels * if self.wide { 2 } else { 1 }
    }
}

/// What `probe_jp2` learns from ffprobe.
pub struct Jp2Meta {
    pub size: [usize; 2],
    pub layout: PixLayout,
}

/// Choose the output format for a source pixel format, keeping native values
/// where ffmpeg's rawvideo formats allow it.
///
/// * mono (`gray*`, and `ya*` — grey + alpha, whose alpha is dropped exactly
///   as the `image` still path drops `La8`'s) keeps its own depth, byte-swapped
///   to little-endian when needed, so a 10/12/14-bit file stays 10/12/14-bit;
/// * anything else is colour: 8-bit sources → `rgb24`/`rgba`, deeper ones →
///   `rgb48le`/`rgba64le` (ffmpeg has no packed 12-bit RGB, so those samples
///   are rescaled to 16 bits by the conversion).
///
/// An unrecognised format falls in the colour branch, whose depth then comes
/// from the trailing bit count in the name (`yuv444p12le` → 12).
pub fn layout_for(pix_fmt: &str) -> PixLayout {
    let fmt = pix_fmt.trim();
    let bits = component_bits(fmt);
    let alpha = has_alpha(fmt);
    if fmt.starts_with("gray") || fmt.starts_with("ya") {
        return PixLayout {
            fmt: match bits {
                ..=8 => "gray",
                9 => "gray9le",
                10 => "gray10le",
                12 => "gray12le",
                14 => "gray14le",
                _ => "gray16le",
            },
            channels: 1,
            wide: bits > 8,
        };
    }
    match (bits > 8, alpha) {
        (false, false) => PixLayout {
            fmt: "rgb24",
            channels: 3,
            wide: false,
        },
        (false, true) => PixLayout {
            fmt: "rgba",
            channels: 4,
            wide: false,
        },
        (true, false) => PixLayout {
            fmt: "rgb48le",
            channels: 3,
            wide: true,
        },
        (true, true) => PixLayout {
            fmt: "rgba64le",
            channels: 4,
            wide: true,
        },
    }
}

/// Bits per component, read from the trailing digits of a pixel-format name
/// (`gray12le` → 12, `yuv420p10be` → 10). The packed byte-per-component names
/// carry no number (`rgb24` is 24 *per pixel*), so anything without a trailing
/// count is 8-bit — as are the packed 16-bit-per-pixel oddities (`rgb565le`),
/// whose components are narrower still. The two packed names that *are* 16 bits
/// per component (`rgb48*`, `rgba64*`) state a per-pixel total, and are read as
/// such below.
fn component_bits(fmt: &str) -> u32 {
    let digits: String = fmt
        .trim_end_matches(['l', 'b', 'e'])
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    // A count only means "per component" when the name ends in an endianness
    // marker; `rgb24`/`bgr0` and friends are per-pixel totals.
    if !(fmt.ends_with("le") || fmt.ends_with("be")) {
        return 8;
    }
    match digits.chars().rev().collect::<String>().parse::<u32>() {
        Ok(n) if (9..=16).contains(&n) => n,
        // The packed 16-bit-per-component names state a per-*pixel* total even
        // though they carry an endianness marker (`rgb48le`, `rgba64le`).
        Ok(48) | Ok(64) => 16,
        _ => 8,
    }
}

/// Whether a pixel format carries an alpha channel (ffmpeg's names all mark it
/// with an `a`: `rgba`, `argb`, `yuva444p`, `gbrap12le`…). Only the colour
/// branch asks — grey + alpha is handled as mono above.
fn has_alpha(fmt: &str) -> bool {
    let base = fmt
        .trim_end_matches("le")
        .trim_end_matches("be")
        .trim_end_matches(|c: char| c.is_ascii_digit());
    ["rgba", "bgra", "argb", "abgr", "yuva", "gbrap"]
        .iter()
        .any(|a| base.starts_with(a))
}

/// Extract the image's geometry and pixel format from ffprobe's `key=value`
/// lines. Pure, so the parsing is unit-testable without ffprobe installed.
fn parse_jp2_probe(text: &str) -> Result<Jp2Meta> {
    let dim = |key| {
        field(text, key)
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| anyhow!("missing {key}"))
    };
    let pix_fmt = field(text, "pix_fmt").ok_or_else(|| anyhow!("missing pix_fmt"))?;
    Ok(Jp2Meta {
        size: [dim("width")?, dim("height")?],
        layout: layout_for(pix_fmt),
    })
}

/// Read a JPEG 2000 image's size and pixel format with `ffprobe`.
pub fn probe_jp2(path: &Path) -> Result<Jp2Meta> {
    let text = ffprobe_stream(path, "stream=width,height,pix_fmt")?;
    parse_jp2_probe(&text)
        .with_context(|| format!("unsupported JPEG 2000 image in {}", path.display()))
}

/// Decode a `.jp2` into a `FrameData` at its native depth (see the module
/// docs for the one colour case that is rescaled).
pub fn decode_jp2(path: &Path) -> Result<FrameData> {
    let meta = probe_jp2(path)?;
    let [w, h] = meta.size;
    let layout = meta.layout;

    // `-v error` + a discarded stderr: ffmpeg can never block on an unread
    // stderr pipe; a failure surfaces as a short read plus the exit status.
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", layout.fmt])
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!(t!(
                "error.tool_not_found",
                tool = "ffmpeg",
                hint = ffmpeg_hint()
            )
            .into_owned()),
            _ => anyhow!(t!("error.tool_failed", tool = "ffmpeg", err = e).into_owned()),
        })?;

    let mut child = out;
    let mut buf = Vec::with_capacity(w * h * layout.bytes_per_pixel());
    let read = child
        .stdout
        .as_mut()
        .expect("piped stdout")
        .read_to_end(&mut buf);
    let status = child.wait().ok();
    read.map_err(|e| {
        anyhow!(t!("error.ffmpeg_read", n = 0, path = path.display(), err = e).into_owned())
    })?;

    let want = w * h * layout.bytes_per_pixel();
    if buf.len() < want {
        return Err(anyhow!(t!(
            "error.ffmpeg_stopped",
            n = 0,
            path = path.display(),
            status = status
                .filter(|s| !s.success())
                .map(|s| format!(" ({s})"))
                .unwrap_or_default()
        )
        .into_owned()));
    }
    buf.truncate(want);

    let samples = if layout.wide {
        Samples::U16(
            buf.chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect(),
        )
    } else {
        Samples::U8(buf)
    };
    Ok(FrameData::new([w, h], layout.channels, samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn mono_keeps_its_native_depth() {
        assert_eq!(
            layout_for("gray"),
            PixLayout {
                fmt: "gray",
                channels: 1,
                wide: false
            }
        );
        for (src, want) in [
            ("gray10le", "gray10le"),
            ("gray12be", "gray12le"),
            ("gray16be", "gray16le"),
            ("gray14le", "gray14le"),
        ] {
            let l = layout_for(src);
            assert_eq!((l.fmt, l.channels, l.wide), (want, 1, true), "{src}");
        }
    }

    #[test]
    fn grey_with_alpha_is_still_mono() {
        assert_eq!(layout_for("ya8").channels, 1);
        let l = layout_for("ya16le");
        assert_eq!((l.fmt, l.channels), ("gray16le", 1));
    }

    #[test]
    fn colour_picks_packed_rgb_by_depth_and_alpha() {
        for (src, fmt, ch, wide) in [
            ("rgb24", "rgb24", 3, false),
            ("yuv420p", "rgb24", 3, false),
            ("rgba", "rgba", 4, false),
            ("gbrp12le", "rgb48le", 3, true),
            ("rgb48be", "rgb48le", 3, true),
            ("gbrap10le", "rgba64le", 4, true),
            ("yuva444p12le", "rgba64le", 4, true),
        ] {
            let l = layout_for(src);
            assert_eq!((l.fmt, l.channels, l.wide), (fmt, ch, wide), "{src}");
        }
    }

    #[test]
    fn packed_per_pixel_counts_are_not_read_as_depth() {
        // `rgb24`/`bgr0` state bits *per pixel*, not per component.
        assert!(!layout_for("rgb24").wide);
        assert!(!layout_for("bgr0").wide);
        assert!(!layout_for("rgb565le").wide);
    }

    #[test]
    fn probe_parsing_needs_a_real_stream() {
        let m = parse_jp2_probe("width=64\nheight=32\npix_fmt=gray12le\n").unwrap();
        assert_eq!(m.size, [64, 32]);
        assert_eq!(m.layout.fmt, "gray12le");
        assert!(parse_jp2_probe("width=0\nheight=32\npix_fmt=gray\n").is_err());
        assert!(parse_jp2_probe("width=64\nheight=32\n").is_err());
    }

    // ---- integration test (needs the ffmpeg CLI; skips gracefully) -------

    /// Encode a small `.jp2` from a known 16-bit grey ramp, or `None` when
    /// ffmpeg isn't installed.
    fn fixture_jp2(pix_fmt: &str, name: &str) -> Option<PathBuf> {
        let path = crate::testutil::fixture_dir("jp2").join(name);
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x32:rate=1:duration=1")
            .args(["-frames:v", "1", "-pix_fmt", pix_fmt, "-c:v", "jpeg2000"])
            .args(["-pred", "1"]) // lossless (reversible 5/3 wavelet)
            .arg(&path)
            .stdin(Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn decodes_a_mono_16_bit_jp2() {
        let Some(path) = fixture_jp2("gray16le", "mono16.jp2") else {
            return; // ffmpeg not installed
        };
        let f = decode_jp2(&path).expect("decode jp2");
        assert_eq!(f.size, [64, 32]);
        assert_eq!(f.channels, 1);
        assert!(matches!(f.samples, Samples::U16(_)), "native 16-bit kept");
        assert!(f.hi_depth());
    }

    #[test]
    fn decodes_a_colour_jp2_through_media_load() {
        let Some(path) = fixture_jp2("rgb24", "rgb8.jp2") else {
            return; // ffmpeg not installed
        };
        let media = crate::media::load(&path).expect("load jp2");
        assert_eq!(media.size(), [64, 32]);
        assert_eq!(media.frame_count(), 1);
        let frame = media.resident(0).expect("still frame is resident");
        assert_eq!(frame.channels, 3);
    }
}
