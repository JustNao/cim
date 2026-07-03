#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod decoder;
mod export;
mod media;
mod settings;
mod view;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("cim — Compare Images & Media"),
        ..Default::default()
    };

    // Any paths on the command line are opened at startup.
    let startup: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();

    eframe::run_native(
        "cim",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::CimApp::new(cc, startup)))),
    )
}
