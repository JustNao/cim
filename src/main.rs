#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod decoder;
mod export;
mod imageproc;
mod media;
mod settings;
mod view;

use eframe::egui;

fn main() -> eframe::Result<()> {
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

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0])
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
