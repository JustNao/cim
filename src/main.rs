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

/// Build the window icon: a white "C" on black, drawn procedurally (no asset
/// file) so the app no longer falls back to eframe's default "e" logo.
fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let center = (N as f32 - 1.0) / 2.0;
    let ro = 0.42 * N as f32; // outer radius of the ring
    let ri = 0.24 * N as f32; // inner radius of the ring
    let gap = 0.62_f32; // half-angle (radians) of the C's opening, facing +x

    let mut rgba = Vec::with_capacity(N * N * 4);
    for y in 0..N {
        for x in 0..N {
            // 3x3 supersample for anti-aliasing.
            let mut cover = 0.0_f32;
            for sy in 0..3 {
                for sx in 0..3 {
                    let px = x as f32 + (sx as f32 + 0.5) / 3.0 - 0.5;
                    let py = y as f32 + (sy as f32 + 0.5) / 3.0 - 0.5;
                    let dx = px - center;
                    let dy = py - center;
                    let r = (dx * dx + dy * dy).sqrt();
                    let in_ring = r >= ri && r <= ro;
                    let in_gap = dx > 0.0 && dy.atan2(dx).abs() < gap;
                    if in_ring && !in_gap {
                        cover += 1.0 / 9.0;
                    }
                }
            }
            let v = (cover * 255.0).round() as u8;
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
    }

    egui::IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}
