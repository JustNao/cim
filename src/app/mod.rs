//! Application state, wiring, and the egui update loop.
//!
//! `CimApp` is large, so its methods are grouped into sibling modules by
//! concern. Each opens its own `impl CimApp` block and pulls shared types and
//! free helpers in via `use super::*`:
//!
//! - [`decode`]    — background decode pool plumbing and texture upload
//! - [`input`]     — keyboard actions, playback, file drops
//! - [`canvas`]    — the central image area (grid / single / A-B) and overlays
//! - [`panels`]    — toolbar and the tool windows (manager, visualise, settings)
//! - [`export_ui`] — the export panel and plan building
//!
//! All shared types and free helpers live here so every sibling can reach them.

mod canvas;
mod decode;
mod export_ui;
mod input;
mod panels;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Id, Key, Pos2, Rect, Sense, Stroke, TextureHandle,
    TextureId, TextureOptions, Vec2,
};

use crate::decoder::BackgroundDecoder;
use crate::export::{self, Encoder, ExportLayout, ExportPane, ExportPlan, ExportSource};
use crate::media::{self, HistData, Media};
use crate::settings::{Action, Config, Interpolation};
use crate::view::ViewTransform;
use export_ui::ExportRun;

const HEADER_H: f32 = 24.0;
const FOOTER_H: f32 = 20.0;
const GAP: f32 = 0.0;
const HANDLE_HIT: f32 = 24.0; // px around the A/B divider that grabs it

/// Soft ceiling on decoded frames kept resident across all sequences. Beyond
/// it the least-recently-viewed frames are evicted (they re-decode on demand),
/// so a long sequence can't grow memory without bound. Sized to stay
/// comfortable on a modest VNC host; raise it if the machine has RAM to spare.
const CACHE_BUDGET_BYTES: usize = 1536 * 1024 * 1024; // 1.5 GiB

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Grid,
    Single,
    Ab,
}

struct CachedTex {
    handle: TextureHandle,
    shown: usize, // frame index currently uploaded
}

/// Cached histogram for the media shown in the Visualise panel.
struct HistCache {
    key: (u64, usize), // (pane id, frame) this was computed for
    data: HistData,
}

/// One opened media plus its per-pane view/timeline state.
struct Pane {
    id: u64, // stable across reorder/close; matches background-decode results
    path: PathBuf, // source file, for reloading from disk
    media: Media,
    tex: Option<CachedTex>,
    transform: ViewTransform, // used only when !sync_spatial
    frame: usize,             // used only when !sync_temporal
    sync_spatial: bool,
    sync_temporal: bool,
    visible: bool,
    /// Per-pane 0.01% percentile auto-contrast (independent of other panes).
    clip: bool,
    /// Last decode error for this sequence, shown centred over the pane.
    error: Option<String>,
    /// "Load all" requested: keep requesting missing + frontier frames until the
    /// whole sequence is resident and its end is found.
    eager: bool,
}

pub struct CimApp {
    config: Config,
    panes: Vec<Pane>,
    next_id: u64,

    // Shared view/timeline that every synced pane follows.
    shared_view: ViewTransform,
    shared_frame: usize,

    mode: Mode,
    current: usize, // focused pane (single view / keyboard target)
    slot_a: usize,  // A/B view operands
    slot_b: usize,
    ab_split: f32, // 0..1 divider position
    ab_handle_grabbed: bool,

    playing: bool,
    fps: f32,
    play_accum: f32,

    show_settings: bool,
    show_manager: bool,
    show_vis: bool,
    hist: Option<HistCache>,
    rebinding: Option<Action>,

    // Export
    show_export: bool,
    export_mode: Mode,
    /// Selected export crop in IMAGE space (pixels of the compared images).
    /// Chosen in Single view; applied to every pane of the comparison. None =
    /// whole image / whole view.
    export_region: Option<Rect>,
    /// Inclusive (start, end) timeline range to export; None = start to finish.
    export_range: Option<(usize, usize)>,
    selecting_region: bool,
    sel_start: Option<Pos2>,
    /// Live screen-space rubber band while dragging out a region.
    sel_rect: Option<Rect>,
    /// Mode to restore once region selection (forced Single) ends.
    pre_select_mode: Option<Mode>,
    out_height: u32,
    crf: u32,
    export_fps: f32,
    /// Output file name, saved in the current working directory. The user
    /// edits just the name — no save dialog / folder picker.
    export_name: String,
    export_run: Option<ExportRun>,
    cancel_export: bool,
    export_status: String,
    status: String,
    /// Global error not tied to a sequence — rendered as a modal popup.
    error_popup: Option<String>,
    last_area: Rect,
    drag_src: Option<usize>,
    pending_remove: Option<usize>,
    pending_reload: Option<usize>,
    pending_reload_all: bool,

    decoder: BackgroundDecoder,
    inflight: HashSet<(u64, usize)>,
    /// True while a "Load all" batch is still decoding, so the status line can
    /// be cleared once every queued frame has landed.
    decoding_all: bool,
    /// Monotonic per-frame counter driving cache LRU recency.
    clock: u64,
}

impl CimApp {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Vec<PathBuf>) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.window_rounding = 8.0.into();
        cc.egui_ctx.set_style(style);

        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 6))
            .unwrap_or(4);

        let mut app = Self {
            config: Config::load(),
            panes: Vec::new(),
            next_id: 0,
            shared_view: ViewTransform::default(),
            shared_frame: 0,
            mode: Mode::Grid,
            current: 0,
            slot_a: 0,
            slot_b: 0,
            ab_split: 0.5,
            ab_handle_grabbed: false,
            playing: false,
            fps: 12.0,
            play_accum: 0.0,
            show_settings: false,
            show_manager: false,
            show_vis: false,
            hist: None,
            rebinding: None,

            show_export: false,
            export_mode: Mode::Grid,
            export_region: None,
            export_range: None,
            selecting_region: false,
            sel_start: None,
            sel_rect: None,
            pre_select_mode: None,
            out_height: 720,
            crf: 23,
            export_fps: 12.0,
            export_name: "comparison.mp4".into(),
            export_run: None,
            cancel_export: false,
            export_status: String::new(),
            status: String::new(),
            error_popup: None,
            last_area: Rect::NOTHING,
            drag_src: None,
            pending_remove: None,
            pending_reload: None,
            pending_reload_all: false,
            decoder: BackgroundDecoder::new(threads),
            inflight: HashSet::new(),
            decoding_all: false,
            clock: 0,
        };
        app.open_paths(startup);
        app
    }

    // ---- loading ---------------------------------------------------------

    pub(super) fn open_dialog(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images & sequences", crate::cli::LOADABLE_EXTS)
            .add_filter("All files", &["*"])
            .pick_files()
        {
            self.open_paths(paths);
        }
    }

    pub(super) fn open_paths(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            match media::load(&p) {
                Ok(m) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    let clip = m.hi_depth(); // >8-bit sources auto-contrast by default
                    self.panes.push(Pane {
                        id,
                        path: p.clone(),
                        media: m,
                        tex: None,
                        transform: ViewTransform::default(),
                        frame: 0,
                        sync_spatial: true,
                        sync_temporal: true,
                        visible: true,
                        clip,
                        error: None,
                        eager: false,
                    });
                }
                Err(e) => {
                    self.error_popup = Some(format!("Failed to open {}:\n{e}", p.display()))
                }
            }
        }
        let n = self.panes.len();
        self.current = self.current.min(n.saturating_sub(1));
        self.slot_a = self.slot_a.min(n.saturating_sub(1));
        self.slot_b = self.slot_b.min(n.saturating_sub(1));
        if n >= 2 && self.slot_a == self.slot_b {
            self.slot_b = self.slot_a + 1;
        }
        self.shared_view.needs_fit = true;
    }

    pub(super) fn remove_media(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        self.decoder.forget(self.panes[i].id); // drop its persistent reader
        self.panes.remove(i);
        let n = self.panes.len();
        let fix = |v: &mut usize| {
            if *v > i {
                *v -= 1;
            }
            *v = (*v).min(n.saturating_sub(1));
        };
        fix(&mut self.current);
        fix(&mut self.slot_a);
        fix(&mut self.slot_b);
    }

    /// Re-open a pane's file from disk, picking up external changes while
    /// keeping its current frame. Files are opened read-only with shared access,
    /// so a persistent reader never blocks another program from writing them.
    pub(super) fn reload(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let path = self.panes[i].path.clone();
        match media::load(&path) {
            Ok(m) => {
                let id = self.panes[i].id;
                self.decoder.forget(id); // reopen the file for its fresh contents
                // Drop stale in-flight decodes aimed at the old contents.
                self.inflight.retain(|(pid, _)| *pid != id);
                self.panes[i].media = m;
                self.panes[i].tex = None; // re-render the kept frame from fresh data
                self.panes[i].error = None;
                // Frame position is left untouched; frame_disp clamps it if the
                // reloaded file is shorter.
            }
            Err(e) => self.panes[i].error = Some(format!("Reload failed: {e}")),
        }
    }

    pub(super) fn reload_all(&mut self) {
        for i in 0..self.panes.len() {
            self.reload(i);
        }
    }

    // ---- per-pane state resolution --------------------------------------

    pub(super) fn view_ref(&self, i: usize) -> &ViewTransform {
        if self.panes[i].sync_spatial {
            &self.shared_view
        } else {
            &self.panes[i].transform
        }
    }

    pub(super) fn view_mut(&mut self, i: usize) -> &mut ViewTransform {
        if self.panes[i].sync_spatial {
            &mut self.shared_view
        } else {
            &mut self.panes[i].transform
        }
    }

    /// Frame actually shown. Synced media follow the shared timeline but a
    /// shorter sequence *holds on its last frame* once the timeline runs past
    /// its end (rather than wrapping early), then loops with the selected media.
    /// Un-synced media wrap within their own length.
    pub(super) fn frame_disp(&self, i: usize) -> usize {
        let c = self.panes[i].media.frame_count().max(1);
        if self.panes[i].sync_temporal {
            self.shared_frame.min(c - 1)
        } else {
            self.panes[i].frame % c
        }
    }

    /// Length of the shared timeline: the currently selected media drives the
    /// loop. Other synced sequences clamp/hold against this length.
    pub(super) fn timeline_len(&self) -> usize {
        self.panes
            .get(self.current)
            .map(|p| p.media.frame_count())
            .unwrap_or(1)
            .max(1)
    }

    /// Whether the timeline-driving media's true end is known. Until it is, the
    /// timeline holds at the last discovered frame rather than wrapping early.
    pub(super) fn current_at_end(&self) -> bool {
        self.panes.get(self.current).map_or(true, |p| p.media.at_end())
    }

    /// Pixel size of the frame actually on screen for pane `i`. Pages in a
    /// sequence may differ in resolution, so use the resident frame's own size,
    /// falling back to the page-0 size before anything has decoded.
    pub(super) fn disp_size(&self, i: usize) -> [usize; 2] {
        let f = self.frame_disp(i);
        self.panes[i]
            .media
            .resident(f)
            .map(|fr| fr.size)
            .unwrap_or_else(|| self.panes[i].media.size())
    }

    pub(super) fn visible_indices(&self) -> Vec<usize> {
        (0..self.panes.len())
            .filter(|&i| self.panes[i].visible)
            .collect()
    }
}

impl eframe::App for CimApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Global UI scale (buttons/text).
        let scale = self.config.ui_scale.clamp(0.5, 3.0);
        if (ctx.zoom_factor() - scale).abs() > 1e-3 {
            ctx.set_zoom_factor(scale);
        }

        self.clock = self.clock.wrapping_add(1);

        self.pump_decoder();
        self.handle_input(ctx);
        self.advance_playback(ctx);

        // Discover sequence length lazily: eager "Load all" batches drive to the
        // end, otherwise just keep one page ahead of the cursor.
        self.drive_eager();
        self.ensure_lookahead();
        self.poll_decoding_all();
        self.enforce_cache_budget();

        // Keep the shared timeline within the selected media's range.
        let tl = self.timeline_len();
        if self.shared_frame >= tl {
            self.shared_frame = tl - 1;
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(2.0);
            self.draw_toolbar(ui);
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.draw_central(ui, ctx);
        });

        if self.show_manager {
            self.draw_manager(ctx);
        }
        if self.show_vis {
            self.draw_vis(ctx);
        }
        if self.show_export {
            self.draw_export(ctx);
        }
        if self.show_settings {
            self.draw_settings(ctx);
        }

        if self.error_popup.is_some() {
            let msg = self.error_popup.clone().unwrap();
            let mut dismiss = false;
            egui::Window::new("⚠ Error")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(msg);
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| {
                        if ui.button("OK").clicked() {
                            dismiss = true;
                        }
                    });
                });
            if dismiss {
                self.error_popup = None;
            }
        }

        if let Some(i) = self.pending_remove.take() {
            self.remove_media(i);
        }
        if std::mem::take(&mut self.pending_reload_all) {
            self.reload_all();
        }
        if let Some(i) = self.pending_reload.take() {
            self.reload(i);
        }

        // Encode one frame per frame while an export is running.
        if self.export_run.is_some() || self.cancel_export {
            self.export_tick();
        }

        // Keep animating while playing, decoding, or exporting.
        if self.playing || !self.inflight.is_empty() || self.export_run.is_some() {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.save();
    }
}

// ---- shared free helpers -------------------------------------------------

/// The image sub-rect of a cell, between its header and footer strips.
fn image_area(cell: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(cell.min.x, cell.min.y + HEADER_H + 2.0),
        Pos2::new(cell.max.x, cell.max.y - FOOTER_H - 2.0),
    )
}

/// The footer strip at the bottom of a cell.
fn footer_area(cell: Rect) -> Rect {
    Rect::from_min_max(Pos2::new(cell.min.x, cell.max.y - FOOTER_H), cell.max)
}

fn uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

/// Dim everything in `area` outside `r`, then outline `r` (export-region look).
fn dim_outside(painter: &egui::Painter, area: Rect, r: Rect) {
    let dim = Color32::from_black_alpha(120);
    painter.rect_filled(
        Rect::from_min_max(area.min, Pos2::new(area.max.x, r.min.y)),
        0.0,
        dim,
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(area.min.x, r.max.y), area.max),
        0.0,
        dim,
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(area.min.x, r.min.y), Pos2::new(r.min.x, r.max.y)),
        0.0,
        dim,
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(r.max.x, r.min.y), Pos2::new(area.max.x, r.max.y)),
        0.0,
        dim,
    );
    painter.rect_stroke(r, 0.0, Stroke::new(2.0, Color32::from_rgb(240, 200, 80)));
}

/// A small animated dot-spinner badge in the bottom-right of `area`.
fn draw_spinner(painter: &egui::Painter, area: Rect, now: f64) {
    let center = area.right_bottom() - Vec2::splat(20.0);
    painter.circle_filled(center, 13.0, Color32::from_black_alpha(150));
    let n = 8i32;
    let phase = (now * 8.0) as i32;
    for k in 0..n {
        let ang = k as f32 / n as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let pos = center + Vec2::new(ang.cos(), ang.sin()) * 7.0;
        let behind = (phase - k).rem_euclid(n);
        let bright = 1.0 - behind as f32 / n as f32;
        let alpha = (40.0 + 215.0 * bright) as u8;
        painter.circle_filled(pos, 2.0, Color32::from_white_alpha(alpha));
    }
}

/// The view that maps an export cell of exactly `reg`'s pixel size onto the
/// image-space crop `reg` (1:1, centred) — a pane cropped to the region.
fn region_view(reg: Rect) -> ViewTransform {
    ViewTransform {
        zoom: 1.0,
        center: reg.center().to_vec2(),
        needs_fit: false,
    }
}

/// Update an index after `panes.swap(src, dst)`.
fn remap(v: &mut usize, src: usize, dst: usize) {
    if *v == src {
        *v = dst;
    } else if *v == dst {
        *v = src;
    }
}

/// Zoom sensitivity per scroll unit; Shift doubles it.
fn zoom_speed(ctx: &egui::Context) -> f32 {
    if ctx.input(|i| i.modifiers.shift) {
        0.003
    } else {
        0.0015
    }
}

/// Effective wheel delta for zooming. While Shift is held the platform remaps
/// the mouse wheel to the horizontal axis, leaving `raw_scroll_delta.y` at 0 —
/// so fall back to the `x` component when `y` is zero.
fn wheel_delta(ctx: &egui::Context) -> f32 {
    ctx.input(|i| {
        let s = i.raw_scroll_delta;
        if s.y != 0.0 {
            s.y
        } else {
            s.x
        }
    })
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
