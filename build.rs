//! Build script: embed the application icon into the Windows `.exe` so it shows
//! cleanly in Explorer, the taskbar and the title bar.
//!
//! A Windows `.ico` is a *bundle* of several square sizes; Explorer picks 16/32/48,
//! the taskbar ~24–32, larger views 256. If those sizes aren't present, Windows
//! downscales whatever is on the fly with a poor scaler — the pixelated look. So
//! we build a **multi-size** icon covering every size Windows asks for.
//!
//! Source, in priority order per size:
//!   1. `assets/icon-<N>.png` (an exact N×N export) if it exists — use it verbatim.
//!      Exporting the small sizes straight from the vector art is the crispest
//!      option (each is pixel-grid-aligned; a downscaled raster never matches it).
//!   2. else the largest `assets/icon-<N>.png` / `assets/icon.png` available,
//!      high-quality (Lanczos3) resized down.
//!
//! We hand the assembled `.ico` to the SDK resource compiler via `winresource`.
//! On non-Windows targets this whole file is a no-op.

/// Sizes Windows actually renders an icon at, largest first (largest is also the
/// resize source when a dedicated per-size export is absent).
#[cfg(windows)]
const SIZES: &[u32] = &[256, 128, 64, 48, 32, 24, 16];

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    for &n in SIZES {
        println!("cargo:rerun-if-changed=assets/icon-{n}.png");
    }

    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
fn embed_windows_icon() {
    use image::{imageops::FilterType, RgbaImage};
    use std::path::{Path, PathBuf};

    // The best source to *downscale* from: the largest exact export present, else
    // the base icon.png. (Upscaling a small source would look worse than letting
    // Windows do it, so we only ever downscale from the biggest thing we have.)
    let per_size = |n: u32| PathBuf::from(format!("assets/icon-{n}.png"));
    let resize_src: RgbaImage = SIZES
        .iter()
        .map(|&n| per_size(n))
        .find(|p| p.exists())
        .map(|p| image::open(&p).expect("decode per-size icon").to_rgba8())
        .unwrap_or_else(|| {
            image::open("assets/icon.png")
                .expect("decode assets/icon.png")
                .to_rgba8()
        });

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &n in SIZES {
        // Prefer a hand-exported size, crisp from the vector; else resize.
        let exact = per_size(n);
        let rgba: RgbaImage = if exact.exists() {
            let img = image::open(&exact)
                .expect("decode per-size icon")
                .to_rgba8();
            if img.dimensions() == (n, n) {
                img
            } else {
                image::imageops::resize(&img, n, n, FilterType::Lanczos3)
            }
        } else {
            image::imageops::resize(&resize_src, n, n, FilterType::Lanczos3)
        };
        let image = ico::IconImage::from_rgba_data(n, n, rgba.into_raw());
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode ico entry"));
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let ico_path = Path::new(&out_dir).join("icon.ico");
    let file = std::fs::File::create(&ico_path).expect("create icon.ico");
    dir.write(file).expect("write icon.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("utf-8 OUT_DIR path"));
    res.compile().expect("compile Windows resource (icon)");
}
