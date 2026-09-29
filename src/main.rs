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
mod thumbs;
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

    // Start loading the inputs now: creating the window takes ~150 ms, and the
    // first frames decode meanwhile instead of after it. The budget first, so
    // the loads run on the instance's share of the CPU.
    cpu::set_budget(config.cpu_budget);
    media::jp2::set_budget_px(config.jp2_max_mp.saturating_mul(1_000_000));
    let preload = std::sync::Mutex::new(Some(app::Preload::start(inputs.clone())));

    // The renderer is chosen once, here (hence "takes effect after restart" in
    // Settings). glow is the default and the fallback; an accelerated run takes
    // wgpu, whose device the tone map shares to write straight into a texture
    // egui samples (see `crate::gpu`).
    if gpu::wants_gpu(config.hardware_accel) {
        // The windowless adapter probe can't tell whether the device can
        // present to this window (remote / VNC sessions often can't), and
        // eframe doesn't fall back by itself: on error, run again on glow.
        match run(&config, &inputs, &view, &preload, eframe::Renderer::Wgpu) {
            Ok(()) => return Ok(()),
            Err(e) => {
                crate::debug::log(&format!("gpu: wgpu renderer failed to start ({e})"));
                eprintln!("cim: hardware acceleration unavailable ({e}); using the CPU renderer");
            }
        }
    }
    run(&config, &inputs, &view, &preload, eframe::Renderer::Glow)
}

/// Open the window and run the app on `renderer`.
///
/// May be called twice (the wgpu attempt above, then glow), so the preload is
/// taken rather than moved, and a second attempt whose first already consumed
/// it loads the inputs again.
fn run(
    config: &settings::Config,
    inputs: &[cli::Input],
    view: &cli::ViewState,
    preload: &std::sync::Mutex<Option<app::Preload>>,
    renderer: eframe::Renderer,
) -> eframe::Result<()> {
    let (config, view) = (config.clone(), view.clone());
    let native_options = eframe::NativeOptions {
        renderer,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            // Ask for the adapter's real limits, not wgpu's portable defaults —
            // a 4096² RGBA 16-bit frame needs a storage binding four times the
            // default ceiling (see `gpu::device_descriptor`).
            device_descriptor: std::sync::Arc::new(gpu::device_descriptor),
            // The **same** backend set the probe resolved against. egui-wgpu
            // would otherwise default to `PRIMARY | GL`, so a machine whose
            // Vulkan device can't drive the surface would fall through to wgpu's
            // GLES backend, which panics on `eglMakeCurrent` rather than
            // declining — a crash instead of the fallback above. See
            // `gpu::BACKENDS` for why GL has no business being here.
            supported_backends: gpu::BACKENDS,
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
        Box::new(move |cc| {
            let preload = preload
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| app::Preload::start(inputs.to_vec()));
            Ok(Box::new(app::CimApp::new(cc, config, preload, view)))
        }),
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
