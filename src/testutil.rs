//! Test-support fixture generators — `#[cfg(test)]` only.
//!
//! The suite used to depend on real capture files in `examples/` and silently
//! skipped when they were absent (i.e. everywhere but one dev machine). These
//! helpers write small **deterministic, synthetic** fixtures into a fresh temp
//! dir instead, so every fixture-driven test actually runs, on any machine,
//! with ground truth the test can compute for itself.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tiff::encoder::{colortype, TiffEncoder};

/// A fresh, unique directory under the system temp dir for one test's
/// fixtures. Unique per call (pid + counter) so parallel tests never share.
pub fn fixture_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cim_fixtures_{tag}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Deterministic 16-bit grayscale page: a value ramp offset by `seed` so every
/// page of a sequence is distinct and any pixel's value is predictable.
pub fn gray16_page(w: usize, h: usize, seed: u16) -> Vec<u16> {
    (0..w * h)
        .map(|i| seed.wrapping_add((i as u16).wrapping_mul(97)))
        .collect()
}

/// Write a multi-page 16-bit grayscale TIFF, one page per entry of `sizes`
/// (pages may differ in resolution, like real capture sequences). Page `k`
/// uses `gray16_page(w, h, k * 1000)`.
pub fn write_multipage_tiff_u16(path: &Path, sizes: &[[usize; 2]]) {
    let mut file = File::create(path).expect("create tiff");
    let mut enc = TiffEncoder::new(&mut file).expect("tiff encoder");
    for (k, &[w, h]) in sizes.iter().enumerate() {
        let data = gray16_page(w, h, (k as u16) * 1000);
        enc.write_image::<colortype::Gray16>(w as u32, h as u32, &data)
            .expect("write tiff page");
    }
}

/// Write a numbered 8-bit grayscale PNG run (`frame_000.png`, …) into `dir`,
/// returning the paths in order. Frame `k` is a ramp offset by `k * 10`.
pub fn write_png_run(dir: &Path, count: usize, w: usize, h: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|k| {
            let path = dir.join(format!("frame_{k:03}.png"));
            let data: Vec<u8> = (0..w * h)
                .map(|i| ((i as u8).wrapping_mul(31)).wrapping_add((k as u8) * 10))
                .collect();
            image::save_buffer(&path, &data, w as u32, h as u32, image::ColorType::L8)
                .expect("write png");
            path
        })
        .collect()
}

/// Write a minimal **1-bit bilevel** TIFF by hand (uncompressed, MSB-first,
/// rows byte-padded — the baseline layout). The `tiff` crate's encoder can't
/// emit 1-bit images, but its decoder reads them, which is exactly what the
/// mask pipeline consumes.
///
/// `bits` holds the **stored** truth per pixel (what an array author set, one
/// 0/1 byte each, row-major) — i.e. the value `mask_bits` must recover.
/// `white_is_zero` picks PhotometricInterpretation 0 (the TIFF default, what
/// `tifffile` writes for a bool array) vs 1 (BlackIsZero).
pub fn write_bilevel_tiff(path: &Path, w: usize, h: usize, bits: &[u8], white_is_zero: bool) {
    assert_eq!(bits.len(), w * h);
    // Pack the stored bits MSB-first, each row padded to a whole byte.
    let stride = w.div_ceil(8);
    let mut data = vec![0u8; stride * h];
    for y in 0..h {
        for x in 0..w {
            if bits[y * w + x] != 0 {
                data[y * stride + x / 8] |= 1 << (7 - (x % 8));
            }
        }
    }

    // Little-endian classic TIFF: header, pixel data, then one IFD.
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"II\x2a\x00"); // byte order + magic 42
    let data_off = 8u32;
    let mut ifd_off = data_off + data.len() as u32;
    if ifd_off % 2 == 1 {
        ifd_off += 1; // IFDs must sit on a word boundary
    }
    out.extend_from_slice(&ifd_off.to_le_bytes());
    out.extend_from_slice(&data);
    out.resize(ifd_off as usize, 0);

    // (tag, type, count, value) — type 3 = SHORT, 4 = LONG. A SHORT value is
    // left-justified in the 4-byte field, which little-endian gives for free.
    let entries: [(u16, u16, u32, u32); 9] = [
        (256, 4, 1, w as u32),                          // ImageWidth
        (257, 4, 1, h as u32),                          // ImageLength
        (258, 3, 1, 1),                                 // BitsPerSample
        (259, 3, 1, 1),                                 // Compression: none
        (262, 3, 1, if white_is_zero { 0 } else { 1 }), // Photometric
        (273, 4, 1, data_off),                          // StripOffsets
        (277, 3, 1, 1),                                 // SamplesPerPixel
        (278, 4, 1, h as u32),                          // RowsPerStrip
        (279, 4, 1, data.len() as u32),                 // StripByteCounts
    ];
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, ty, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

    File::create(path)
        .expect("create bilevel tiff")
        .write_all(&out)
        .expect("write bilevel tiff");
}
