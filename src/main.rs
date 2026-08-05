#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod cpu;
mod debug;
mod decoder;
mod export;
mod gpu;
mod imageproc;
mod media;
mod offsets;
mod palette;
mod renderer;
mod settings;
#[cfg(test)]
mod testutil;
mod tone;
mod view;
mod watcher;

use eframe::egui;

// UI translations, baked in from `locales/*.yml` (rust-i18n **version 1** format:
// one flat file per locale). English is the default; French is the fallback, so a
// key missing from `en.yml` shows its French text rather than the raw key.
rust_i18n::i18n!("locales", fallback = "fr");

fn main() -> eframe::Result<()> {
    let config = settings::Config::load();
    // Pick the UI language before anything prints or draws: `--help` and the
    // completion output are localised too, and they never reach the window.
    settings::apply_locale(&config.language);

    // Handle CLI-only requests (--help, completion) and expand sequence tokens
    // before we ever open a window.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let (inputs, view) = match cli::parse(args) {
        cli::Cli::Run { inputs, view } => (inputs, view),
        cli::Cli::Exit(code) => std::process::exit(code),
    };

    // Which renderer this run gets, decided here because eframe can only be told
    // once — hence the "takes effect after restart" note in Settings.
    //
    // **glow (OpenGL) stays the default and the fallback.** It is what every CPU
    // -mode run and every machine without a usable adapter uses, unchanged from
    // before this option existed, which is the point: adding a GPU path must not
    // change the graphics stack under users who aren't asking for one. GPU mode
    // takes wgpu instead, because sharing its device is what lets the tone map
    // write into a texture egui samples without a readback (see `crate::gpu`).
    // If wgpu then fails to start, eframe falls back on its own and `CimApp`
    // finds no render state, which is simply CPU mode.
    let gpu = gpu::wants_gpu(config.render_backend);
    let native_options = eframe::NativeOptions {
        renderer: if gpu {
            eframe::Renderer::Wgpu
        } else {
            eframe::Renderer::Glow
        },
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            // Ask for the adapter's real limits, not wgpu's portable defaults —
            // a 4096² RGBA 16-bit frame needs a storage binding four times the
            // default ceiling (see `gpu::device_descriptor`).
            device_descriptor: std::sync::Arc::new(gpu::device_descriptor),
            ..Default::default()
        },
        viewport: egui::ViewportBuilder::default()
            .with_min_inner_size([640.0, 400.0])
            // The app opens filling the screen, but NOT via `with_maximized`:
            // on Windows winit applies that flag at window creation with
            // `ShowWindow(SW_MAXIMIZE)`, which shows the still-unpainted
            // window — a white flash — defeating eframe's own
            // hidden-until-first-frame handling. Instead ask for an oversized
            // window (eframe clamps it to the monitor) so the first frame is
            // painted at full-screen size while hidden, and let `tick`
            // maximize on the first update. Unmaximizing then restores to
            // roughly the monitor size.
            .with_inner_size([10000.0, 10000.0])
            .with_title("cim")
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "cim",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::CimApp::new(cc, inputs, view)))),
    )
}

/// The window icon, decoded at startup from the PNG baked into the binary.
/// eframe wants raw RGBA8 pixels (`IconData`), not an encoded PNG, so we decode
/// it with the `image` crate. `include_bytes!` embeds the file into the exe at
/// build time, so the icon ships with it — there's no runtime path to find.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("decode embedded app icon")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}
