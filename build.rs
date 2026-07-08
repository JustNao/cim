//! Build script: embed the application icon into the Windows `.exe` so it shows
//! in Explorer, the taskbar and the title bar.
//!
//! We keep a single source of truth for the icon — `assets/icon.png`, which is
//! also baked into the binary for the runtime window icon (see `main.rs`). Here
//! we decode that PNG and re-encode it as a `.ico` into `OUT_DIR`, then hand it
//! to the Windows SDK resource compiler via `winresource`. On non-Windows
//! targets this is a no-op.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
fn embed_windows_icon() {
    use image::{codecs::ico::IcoEncoder, imageops::FilterType, ExtendedColorType, ImageEncoder};
    use std::path::Path;

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let ico_path = Path::new(&out_dir).join("icon.ico");

    // The .ico format caps a single image at 256×256, so resize the (larger)
    // source down. Nearest would suffice for pixel work, but the icon is UI
    // chrome, not sampled media — Lanczos keeps it crisp when downscaled.
    let img = image::open("assets/icon.png")
        .expect("decode assets/icon.png")
        .resize_exact(256, 256, FilterType::Lanczos3)
        .to_rgba8();

    let mut buf = Vec::new();
    IcoEncoder::new(&mut buf)
        .write_image(img.as_raw(), 256, 256, ExtendedColorType::Rgba8)
        .expect("encode icon.ico");
    std::fs::write(&ico_path, &buf).expect("write icon.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("utf-8 OUT_DIR path"));
    res.compile().expect("compile Windows resource (icon)");
}
