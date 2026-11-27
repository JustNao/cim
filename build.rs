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
//! On non-Windows targets the icon step is a no-op.
//!
//! It also **embeds `proprietary.yaml`** (see `src/proprietary.rs`) when one is
//! present at build time — at the crate root, or wherever `CIM_PROPRIETARY`
//! points — so a build made with it needs no file shipped beside it. Without
//! one the embedded text is empty and the build is the generic one. The file is
//! git-ignored, so the names it holds never reach the repository.

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

    embed_proprietary();

    #[cfg(windows)]
    embed_windows_icon();
}

/// Copy the build's `proprietary.yaml` (or nothing) to `OUT_DIR`, where
/// `src/proprietary.rs` `include_str!`s it.
fn embed_proprietary() {
    use std::path::PathBuf;
    println!("cargo:rerun-if-env-changed=CIM_PROPRIETARY");
    let src = std::env::var_os("CIM_PROPRIETARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("proprietary.yaml"));
    println!("cargo:rerun-if-changed={}", src.display());
    let text = std::fs::read_to_string(&src).unwrap_or_default();
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"))
        .join("proprietary.yaml");
    // Rewrite only on a change: the crate rebuilds whenever this file's mtime
    // moves, and a build without the file re-runs this script every time.
    if std::fs::read_to_string(&out).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&out, text).expect("write embedded proprietary.yaml");
    }
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
