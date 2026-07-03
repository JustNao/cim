//! Application state and the egui update loop.

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

/// An in-progress export: encoder + snapshotted plan + progress.
struct ExportRun {
    enc: Encoder,
    plan: ExportPlan,
    frame: usize,
    total: usize,
    path: String,
}

const HEADER_H: f32 = 24.0;
const FOOTER_H: f32 = 20.0;
const GAP: f32 = 0.0;
const HANDLE_HIT: f32 = 24.0; // px around the A/B divider that grabs it

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
        };
        app.open_paths(startup);
        app
    }

    // ---- loading ---------------------------------------------------------

    fn open_dialog(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images & sequences", crate::cli::LOADABLE_EXTS)
            .add_filter("All files", &["*"])
            .pick_files()
        {
            self.open_paths(paths);
        }
    }

    fn open_paths(&mut self, paths: Vec<PathBuf>) {
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

    fn remove_media(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
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
    /// keeping its current frame. Decodes only ever open the file read-only and
    /// briefly, so it's never held locked against the program writing it.
    fn reload(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let path = self.panes[i].path.clone();
        match media::load(&path) {
            Ok(m) => {
                let id = self.panes[i].id;
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

    fn reload_all(&mut self) {
        for i in 0..self.panes.len() {
            self.reload(i);
        }
    }

    // ---- per-pane state resolution --------------------------------------

    fn view_ref(&self, i: usize) -> &ViewTransform {
        if self.panes[i].sync_spatial {
            &self.shared_view
        } else {
            &self.panes[i].transform
        }
    }

    fn view_mut(&mut self, i: usize) -> &mut ViewTransform {
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
    fn frame_disp(&self, i: usize) -> usize {
        let c = self.panes[i].media.frame_count().max(1);
        if self.panes[i].sync_temporal {
            self.shared_frame.min(c - 1)
        } else {
            self.panes[i].frame % c
        }
    }

    /// Length of the shared timeline: the currently selected media drives the
    /// loop. Other synced sequences clamp/hold against this length.
    fn timeline_len(&self) -> usize {
        self.panes
            .get(self.current)
            .map(|p| p.media.frame_count())
            .unwrap_or(1)
            .max(1)
    }

    /// Whether the timeline-driving media's true end is known. Until it is, the
    /// timeline holds at the last discovered frame rather than wrapping early.
    fn current_at_end(&self) -> bool {
        self.panes.get(self.current).map_or(true, |p| p.media.at_end())
    }

    /// Pixel size of the frame actually on screen for pane `i`. Pages in a
    /// sequence may differ in resolution, so use the resident frame's own size,
    /// falling back to the page-0 size before anything has decoded.
    fn disp_size(&self, i: usize) -> [usize; 2] {
        let f = self.frame_disp(i);
        self.panes[i]
            .media
            .resident(f)
            .map(|fr| fr.size)
            .unwrap_or_else(|| self.panes[i].media.size())
    }

    fn visible_indices(&self) -> Vec<usize> {
        (0..self.panes.len())
            .filter(|&i| self.panes[i].visible)
            .collect()
    }

    // ---- background decode plumbing -------------------------------------

    fn pump_decoder(&mut self) {
        for d in self.decoder.drain() {
            self.inflight.remove(&(d.id, d.frame));
            match d.result {
                Ok(Some(frame)) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.insert(d.frame, frame);
                        p.error = None; // a good frame clears any stale error
                    }
                }
                Ok(None) => {
                    // Frontier probe found no page here: the sequence ends before it.
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.set_at_end();
                    }
                }
                Err(e) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.error = Some(format!("Frame {}: {e}", d.frame + 1));
                    }
                }
            }
        }
    }

    fn request(&mut self, idx: usize, frame: usize) {
        let id = self.panes[idx].id;
        if self.inflight.contains(&(id, frame)) {
            return;
        }
        if let Some(path) = self.panes[idx].media.decode_job(frame) {
            self.decoder.request(id, frame, path);
            self.inflight.insert((id, frame));
        }
    }

    fn load_all(&mut self) {
        for p in &mut self.panes {
            p.eager = true;
        }
        self.status = "Queued all frames for background decoding…".into();
        self.decoding_all = true;
    }

    /// While "Load all" is active, keep every eager pane requesting its missing
    /// known frames plus one frontier probe, so an unknown-length sequence loads
    /// fully and reveals its end. A pane clears its flag once every frame is
    /// resident and its end has been found.
    fn drive_eager(&mut self) {
        for i in 0..self.panes.len() {
            if !self.panes[i].eager {
                continue;
            }
            let known = self.panes[i].media.frame_count();
            let mut pending = false;
            for f in 0..known {
                if self.panes[i].media.resident(f).is_none() {
                    self.request(i, f);
                    pending = true;
                }
            }
            if !self.panes[i].media.at_end() {
                self.request(i, known); // probe for a next page
                pending = true;
            }
            if !pending {
                self.panes[i].eager = false;
            }
        }
    }

    /// Keep the next page discovered for panes the user is browsing, so stepping
    /// forward and the timeline length stay ahead of the cursor without ever
    /// decoding past what's actually being viewed.
    fn ensure_lookahead(&mut self) {
        for i in 0..self.panes.len() {
            if self.panes[i].eager || self.panes[i].media.at_end() {
                continue;
            }
            let known = self.panes[i].media.frame_count();
            // Probe one page beyond the frame currently shown.
            if self.frame_disp(i) + 2 > known {
                self.request(i, known);
            }
        }
    }

    /// Clear the "decoding…" status once the whole batch has landed.
    fn poll_decoding_all(&mut self) {
        if self.decoding_all && !self.panes.iter().any(|p| p.eager) && self.inflight.is_empty() {
            self.decoding_all = false;
            if self.status == "Queued all frames for background decoding…" {
                self.status.clear();
            }
        }
    }

    // ---- textures --------------------------------------------------------

    fn tex_options(&self) -> TextureOptions {
        let magnification = match self.config.vis.interp {
            Interpolation::Nearest => egui::TextureFilter::Nearest,
            Interpolation::Bilinear => egui::TextureFilter::Linear,
        };
        TextureOptions {
            magnification,
            minification: egui::TextureFilter::Linear,
            ..Default::default()
        }
    }

    /// Ensure pane `idx` shows the best texture available for its current frame.
    /// Returns `(texture, loading)`: if the target frame is resident it uploads
    /// and returns it (`loading = false`); otherwise it queues a decode and
    /// returns the *previously shown* texture with `loading = true`, so the pane
    /// keeps displaying the last frame while the new one decodes.
    fn prepare(&mut self, ctx: &egui::Context, idx: usize) -> (Option<TextureId>, bool) {
        let f = self.frame_disp(idx);
        if let Some(frame) = self.panes[idx].media.resident(f) {
            let need = match &self.panes[idx].tex {
                Some(t) => t.shown != f,
                None => true,
            };
            if need {
                // Only run the (expensive) render + upload when the texture is stale.
                let opts = self.tex_options();
                let pixels = frame.render_rgba(self.panes[idx].clip);
                let img = ColorImage::from_rgba_unmultiplied(frame.size, &pixels);
                let p = &mut self.panes[idx];
                match &mut p.tex {
                    Some(t) => {
                        t.handle.set(img, opts);
                        t.shown = f;
                    }
                    None => {
                        let handle = ctx.load_texture(format!("m{}", p.id), img, opts);
                        p.tex = Some(CachedTex { handle, shown: f });
                    }
                }
            }
            (Some(self.panes[idx].tex.as_ref().unwrap().handle.id()), false)
        } else {
            self.request(idx, f);
            let last = self.panes[idx].tex.as_ref().map(|t| t.handle.id());
            (last, true)
        }
    }

    // ---- actions ---------------------------------------------------------

    fn apply_action(&mut self, action: Action, ctx: &egui::Context) {
        let n = self.panes.len();
        match action {
            Action::ToggleView => {
                self.mode = match self.mode {
                    Mode::Grid => Mode::Single,
                    Mode::Single => Mode::Ab,
                    Mode::Ab => Mode::Grid,
                };
            }
            Action::ViewGrid => self.mode = Mode::Grid,
            Action::ViewSingle => self.mode = Mode::Single,
            Action::ViewAb => self.mode = Mode::Ab,
            Action::NextMedia if n > 0 => self.current = (self.current + 1) % n,
            Action::PrevMedia if n > 0 => self.current = (self.current + n - 1) % n,
            Action::NextFrame => {
                let tl = self.timeline_len();
                if self.shared_frame + 1 < tl {
                    self.shared_frame += 1;
                } else if self.current_at_end() {
                    self.shared_frame = 0; // wrap only once the real length is known
                }
                // else hold at the frontier; lookahead extends it shortly
            }
            Action::PrevFrame => {
                let tl = self.timeline_len();
                if self.shared_frame > 0 {
                    self.shared_frame -= 1;
                } else if self.current_at_end() {
                    self.shared_frame = tl - 1;
                }
            }
            Action::ResetView => {
                self.shared_view.needs_fit = true;
                for p in &mut self.panes {
                    p.transform.needs_fit = true;
                }
            }
            Action::ActualSize if n > 0 => {
                let size = self.panes[self.current].media.size();
                self.view_mut(self.current).actual_size(size);
            }
            Action::ZoomIn if n > 0 => {
                let a = self.last_area;
                self.view_mut(self.current).zoom_at(1.25, a.center(), a);
            }
            Action::ZoomOut if n > 0 => {
                let a = self.last_area;
                self.view_mut(self.current).zoom_at(1.0 / 1.25, a.center(), a);
            }
            Action::LoadAll => self.load_all(),
            Action::OpenFiles => self.open_dialog(),
            Action::ToggleSettings => self.show_settings = !self.show_settings,
            Action::ToggleManager => self.show_manager = !self.show_manager,
            Action::ToggleVis => self.show_vis = !self.show_vis,
            Action::ToggleExport => self.toggle_export(),
            Action::PlayPause => self.playing = !self.playing,
            Action::SelectMedia(i) if i < n => {
                self.current = i;
                self.mode = Mode::Single;
            }
            _ => {}
        }
        ctx.request_repaint();
    }

    fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playing {
            return;
        }
        let tl = self.timeline_len();
        let at_end = self.current_at_end();
        if tl <= 1 && at_end {
            return;
        }
        let dt = ctx.input(|i| i.stable_dt).min(0.25);
        self.play_accum += dt;
        let step = 1.0 / self.fps.max(0.1);
        while self.play_accum >= step {
            self.play_accum -= step;
            if self.shared_frame + 1 < tl {
                self.shared_frame += 1;
            } else if at_end {
                self.shared_frame = 0;
            } else {
                // At the discovered frontier — wait for the next page rather than
                // wrapping early; drop the backlog so we don't burst afterwards.
                self.play_accum = 0.0;
                break;
            }
            for p in &mut self.panes {
                if !p.sync_temporal {
                    let c = p.media.frame_count();
                    if p.frame + 1 < c {
                        p.frame += 1;
                    } else if p.media.at_end() {
                        p.frame = 0;
                    }
                }
            }
        }
    }

    // ---- input -----------------------------------------------------------

    fn handle_input(&mut self, ctx: &egui::Context) {
        if let Some(action) = self.rebinding {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                if k != Key::Escape {
                    self.config.keybindings.set(action, k);
                    self.config.save();
                }
                self.rebinding = None;
            }
            return;
        }

        for action in Action::all() {
            if let Some(key) = self.config.keybindings.key_for(action) {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.apply_action(action, ctx);
                }
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.open_paths(dropped);
        }
    }

    // ---- toolbar ---------------------------------------------------------

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("📂 Open").clicked() {
                self.open_dialog();
            }
            ui.separator();
            for (mode, label) in [
                (Mode::Grid, "▦ Grid"),
                (Mode::Single, "▢ Single"),
                (Mode::Ab, "◧ A/B"),
            ] {
                if ui.selectable_label(self.mode == mode, label).clicked() {
                    self.mode = mode;
                }
            }
            ui.separator();
            if ui.button("Fit").clicked() {
                self.apply_action_local(Action::ResetView);
            }
            if ui.button("100%").clicked() && !self.panes.is_empty() {
                let size = self.panes[self.current].media.size();
                self.view_mut(self.current).actual_size(size);
            }
            ui.label(format!("{:.0}%", self.view_zoom_label() * 100.0));

            ui.separator();
            let play = if self.playing { "⏸" } else { "▶" };
            if ui.button(play).clicked() {
                self.playing = !self.playing;
            }
            ui.add(
                egui::Slider::new(&mut self.fps, 1.0..=60.0)
                    .suffix(" fps")
                    .fixed_decimals(0),
            );

            let tl = self.timeline_len();
            if tl > 1 {
                ui.label("frame");
                ui.add(
                    egui::Slider::new(&mut self.shared_frame, 0..=tl - 1)
                        .clamping(egui::SliderClamping::Always),
                );
            }

            ui.separator();
            if ui.button("⤓ Load all").clicked() {
                self.load_all();
            }
            if ui.selectable_label(self.show_manager, "☰ Media").clicked() {
                self.show_manager = !self.show_manager;
            }
            if ui.selectable_label(self.show_vis, "Visualise").clicked() {
                self.show_vis = !self.show_vis;
            }
            if ui.selectable_label(self.show_export, "Export").clicked() {
                self.toggle_export();
            }
            if ui.selectable_label(self.show_settings, "⚙ Settings").clicked() {
                self.show_settings = !self.show_settings;
            }
        });

        // A/B operand pickers.
        if self.mode == Mode::Ab && self.panes.len() >= 1 {
            ui.horizontal(|ui| {
                self.ab_picker(ui, true);
                ui.separator();
                self.ab_picker(ui, false);
            });
        }

        if !self.status.is_empty() {
            ui.label(egui::RichText::new(&self.status).weak().small());
        }
    }

    fn ab_picker(&mut self, ui: &mut egui::Ui, is_a: bool) {
        let n = self.panes.len();
        let slot = if is_a { &mut self.slot_a } else { &mut self.slot_b };
        *slot = (*slot).min(n - 1);
        ui.label(if is_a { "A:" } else { "B:" });
        if ui.small_button("◀").clicked() {
            *slot = (*slot + n - 1) % n;
        }
        let name = self.panes[*slot].media.name().to_string();
        ui.monospace(format!("{}·{}", *slot + 1, ellipsize(&name, 16)));
        if ui.small_button("▶").clicked() {
            *slot = (*slot + 1) % n;
        }
    }

    fn view_zoom_label(&self) -> f32 {
        if self.panes.is_empty() {
            1.0
        } else {
            self.view_ref(self.current.min(self.panes.len() - 1)).zoom
        }
    }

    fn apply_action_local(&mut self, action: Action) {
        if action == Action::ResetView {
            self.shared_view.needs_fit = true;
            for p in &mut self.panes {
                p.transform.needs_fit = true;
            }
        }
    }

    // ---- central drawing -------------------------------------------------

    fn draw_central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let area = ui.available_rect_before_wrap();
        self.last_area = area;

        if self.panes.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                "Open images or drop files here\n(mp4/avi coming later)",
                FontId::proportional(18.0),
                Color32::from_gray(140),
            );
            return;
        }

        match self.mode {
            Mode::Single => {
                let idx = self.current.min(self.panes.len() - 1);
                self.draw_pane(ui, ctx, idx, area);
            }
            Mode::Grid => {
                let vis = self.visible_indices();
                if vis.is_empty() {
                    ui.painter().text(
                        area.center(),
                        Align2::CENTER_CENTER,
                        "All media hidden — enable some in ☰ Media",
                        FontId::proportional(16.0),
                        Color32::from_gray(140),
                    );
                    return;
                }
                let cells = self.grid_cells(&vis, area);
                for &(idx, cell) in &cells {
                    self.draw_pane(ui, ctx, idx, cell);
                }
                self.finish_reorder(ctx, &cells);
            }
            Mode::Ab => self.draw_ab(ui, ctx, area),
        }

        self.region_overlay(ui, ctx, area);
    }

    /// Draw / edit the export region rectangle over the view.
    ///
    /// Selection is done in Single view (forced while `selecting_region`): the
    /// screen drag is converted to IMAGE space on release, so the same crop
    /// applies to every pane of the comparison afterwards.
    fn region_overlay(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect) {
        if !self.show_export || self.panes.is_empty() {
            return;
        }

        if self.selecting_region {
            let resp = ui.interact(area, Id::new("region_sel"), Sense::drag());
            let pos = ctx.input(|i| i.pointer.interact_pos());
            if resp.drag_started() {
                self.sel_start = pos;
            }
            if resp.dragged() {
                if let (Some(s), Some(c)) = (self.sel_start, pos) {
                    self.sel_rect = Some(Rect::from_two_pos(s, c).intersect(area));
                }
            }
            if resp.drag_stopped() {
                self.selecting_region = false;
                self.sel_start = None;
                // Discard a zero-size accidental click, then map to image space.
                self.export_region = self
                    .sel_rect
                    .take()
                    .filter(|r| r.width() >= 4.0 && r.height() >= 4.0)
                    .and_then(|r| self.screen_rect_to_image(r, area));
                if let Some(m) = self.pre_select_mode.take() {
                    self.mode = m;
                }
            }
            if let Some(r) = self.sel_rect {
                dim_outside(&ui.painter_at(area), area, r);
            }
            return;
        }

        // Region chosen: show it on every pane it applies to.
        let Some(reg) = self.export_region else { return };
        let panes_areas: Vec<(usize, Rect)> = match self.mode {
            Mode::Single => {
                vec![(self.current.min(self.panes.len() - 1), image_area(area))]
            }
            Mode::Grid => self
                .grid_cells(&self.visible_indices(), area)
                .iter()
                .map(|&(idx, cell)| (idx, image_area(cell)))
                .collect(),
            // The wipe shares one image area; both sides are spatially the same
            // place, so pane A's view is representative.
            Mode::Ab => vec![(
                self.slot_a.min(self.panes.len() - 1),
                Rect::from_min_max(area.min, Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0)),
            )],
        };
        for (idx, img_area) in panes_areas {
            let v = self.view_ref(idx);
            let r = Rect::from_two_pos(
                v.img_to_screen(reg.min.to_vec2(), img_area),
                v.img_to_screen(reg.max.to_vec2(), img_area),
            )
            .intersect(img_area);
            if r.is_positive() {
                dim_outside(&ui.painter_at(img_area), img_area, r);
            }
        }
    }

    /// Convert a screen-space rect (drawn in Single view over `area`) into the
    /// image-space crop it covers, clamped to the current image's bounds.
    fn screen_rect_to_image(&self, r: Rect, area: Rect) -> Option<Rect> {
        let idx = self.current.min(self.panes.len().checked_sub(1)?);
        let img_area = image_area(area);
        let v = self.view_ref(idx);
        let [w, h] = self.disp_size(idx);
        let a = v.screen_to_img(r.min, img_area);
        let b = v.screen_to_img(r.max, img_area);
        let reg = Rect::from_two_pos(a.to_pos2(), b.to_pos2())
            .intersect(Rect::from_min_max(Pos2::ZERO, Pos2::new(w as f32, h as f32)));
        (reg.width() >= 1.0 && reg.height() >= 1.0).then_some(reg)
    }

    fn grid_cells(&self, vis: &[usize], area: Rect) -> Vec<(usize, Rect)> {
        let n = vis.len();
        let cols = self.config.max_columns.max(1).min(n).max(1);
        let rows = (n + cols - 1) / cols;
        let cw = (area.width() - GAP * (cols as f32 - 1.0)) / cols as f32;
        let ch = (area.height() - GAP * (rows as f32 - 1.0)) / rows as f32;
        let mut cells = Vec::with_capacity(n);
        for (k, &idx) in vis.iter().enumerate() {
            let r = k / cols;
            let c = k % cols;
            let min = Pos2::new(
                area.min.x + c as f32 * (cw + GAP),
                area.min.y + r as f32 * (ch + GAP),
            );
            cells.push((idx, Rect::from_min_size(min, Vec2::new(cw, ch))));
        }
        cells
    }

    fn finish_reorder(&mut self, ctx: &egui::Context, cells: &[(usize, Rect)]) {
        let Some(src) = self.drag_src else { return };
        if !ctx.input(|i| i.pointer.any_released()) {
            return;
        }
        if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
            if let Some(&(dst, _)) = cells.iter().find(|(_, r)| r.contains(pos)) {
                if dst != src {
                    self.panes.swap(src, dst);
                    remap(&mut self.current, src, dst);
                    remap(&mut self.slot_a, src, dst);
                    remap(&mut self.slot_b, src, dst);
                }
            }
        }
        self.drag_src = None;
    }

    fn draw_pane(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, idx: usize, cell: Rect) {
        let img_area = image_area(cell);
        let size = self.panes[idx].media.size();

        // Fit this pane's effective view on first draw / after a reset.
        {
            let v = self.view_mut(idx);
            if v.needs_fit {
                v.fit(size, img_area);
            }
        }

        let (tex, loading) = self.prepare(ctx, idx);
        let painter = ui.painter_at(img_area);
        painter.rect_filled(img_area, 0.0, Color32::from_gray(24));
        if let Some(id) = tex {
            let v = *self.view_ref(idx);
            let rect = v.image_rect(self.disp_size(idx), img_area);
            painter.image(id, rect, uv(), Color32::WHITE);
        }
        if loading {
            draw_spinner(&painter, img_area, ctx.input(|i| i.time));
        }
        self.draw_pane_error(ui, idx, img_area);

        // Interaction: zoom / pan / ctrl-drag reorder. Disabled while the user
        // is dragging out an export region (so that drag isn't stolen).
        let sense = if self.selecting_region {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let resp = ui.interact(img_area, Id::new(("pane", idx)), sense);
        if !self.selecting_region {
            let ctrl = ctx.input(|i| i.modifiers.ctrl);
            if resp.hovered() {
                let scroll = wheel_delta(ctx);
                if scroll != 0.0 {
                    let anchor = ctx
                        .input(|i| i.pointer.hover_pos())
                        .unwrap_or(img_area.center());
                    let speed = zoom_speed(ctx);
                    self.view_mut(idx).zoom_at((scroll * speed).exp(), anchor, img_area);
                }
            }
            if resp.drag_started() && ctrl {
                self.drag_src = Some(idx);
            }
            if resp.dragged() && self.drag_src.is_none() {
                let d = resp.drag_delta();
                self.view_mut(idx).pan(d);
            }
            if resp.clicked() {
                self.current = idx;
            }
        }

        self.draw_header(ui, idx, cell);
        self.draw_footer(ui, idx, resp.hover_pos(), img_area, footer_area(cell));

        // No persistent pane border (it doubles up at zero gap, breaking the
        // middle pane). Borders show only during a ctrl-drag reorder: blue on
        // the pane being moved, green on the pane it would swap with.
        if self.drag_src == Some(idx) {
            ui.painter()
                .rect_stroke(cell, 0.0, Stroke::new(2.0, Color32::from_rgb(120, 170, 240)));
        } else if self.drag_src.is_some()
            && ctx
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| cell.contains(p))
        {
            ui.painter()
                .rect_stroke(cell, 0.0, Stroke::new(2.0, Color32::from_rgb(120, 210, 120)));
        }
    }

    /// If this sequence failed to decode, paint its message centred over `rect`.
    fn draw_pane_error(&self, ui: &egui::Ui, idx: usize, rect: Rect) {
        let Some(msg) = self.panes[idx].error.as_deref() else {
            return;
        };
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
        let col = Color32::from_rgb(240, 130, 130);
        let galley = painter.layout(
            format!("⚠  {msg}"),
            FontId::proportional(15.0),
            col,
            (rect.width() - 32.0).max(48.0),
        );
        let pos = rect.center() - galley.size() / 2.0;
        painter.galley(pos, galley, col);
    }

    fn draw_header(&mut self, ui: &mut egui::Ui, idx: usize, cell: Rect) {
        let header = Rect::from_min_size(cell.min, Vec2::new(cell.width(), HEADER_H));
        let hp = ui.painter_at(header);
        let focused = idx == self.current;
        hp.rect_filled(
            header,
            0.0,
            if focused {
                Color32::from_rgb(40, 70, 110)
            } else {
                Color32::from_gray(34)
            },
        );

        let count = self.panes[idx].media.frame_count();
        let name = self.panes[idx].media.name();
        let title = if count > 1 {
            let resident = self.panes[idx].media.resident_count();
            let sync = match (
                self.panes[idx].sync_spatial,
                self.panes[idx].sync_temporal,
            ) {
                (true, true) => "",
                (false, true) => "  ⊘pos",
                (true, false) => "  ⊘time",
                (false, false) => "  ⊘pos ⊘time",
            };
            // Until the real end is found, show the known count with a "+" so
            // it's clear more frames may still be discovered.
            let count_str = if self.panes[idx].media.at_end() {
                format!("{count}")
            } else {
                format!("{count}+")
            };
            format!(
                "{}  {}   {}/{}  ({} in mem){}",
                idx + 1,
                name,
                self.frame_disp(idx) + 1,
                count_str,
                resident,
                sync
            )
        } else {
            format!("{}  {}", idx + 1, name)
        };
        hp.text(
            header.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(13.0),
            Color32::from_gray(220),
        );

        let close = Rect::from_min_size(
            Pos2::new(header.max.x - HEADER_H, header.min.y),
            Vec2::splat(HEADER_H),
        );
        let close_resp = ui.interact(close, Id::new(("close", idx)), Sense::click());
        hp.text(
            close.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(18.0),
            if close_resp.hovered() {
                Color32::from_rgb(230, 120, 120)
            } else {
                Color32::from_gray(160)
            },
        );
        if close_resp.clicked() {
            self.pending_remove = Some(idx);
        }
    }

    /// Bottom status strip: resolution (h×w), cursor pixel, native value.
    fn draw_footer(
        &self,
        ui: &egui::Ui,
        idx: usize,
        hover: Option<Pos2>,
        img_area: Rect,
        footer: Rect,
    ) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, Color32::from_gray(28));

        let [w, h] = self.disp_size(idx);
        let mut text = format!("{h}×{w}");

        if let Some(pos) = hover {
            if img_area.contains(pos) {
                let p = self.view_ref(idx).screen_to_img(pos, img_area);
                let (x, y) = (p.x.floor() as i64, p.y.floor() as i64);
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    let (x, y) = (x as usize, y as usize);
                    let f = self.frame_disp(idx);
                    if let Some(frame) = self.panes[idx].media.resident(f) {
                        text = format!("{h}×{w}    x {x}  y {y}    {}", frame.pixel_string(x, y));
                    } else {
                        text = format!("{h}×{w}    x {x}  y {y}");
                    }
                }
            }
        }

        fp.text(
            footer.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            text,
            FontId::monospace(12.0),
            Color32::from_gray(200),
        );
    }

    // ---- A/B wipe view ---------------------------------------------------

    fn draw_ab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect) {
        let n = self.panes.len();
        let a = self.slot_a.min(n - 1);
        let b = self.slot_b.min(n - 1);

        // Reserve a footer strip; images live in `img`.
        let img = Rect::from_min_max(
            area.min,
            Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0),
        );
        let footer = Rect::from_min_max(Pos2::new(area.min.x, area.max.y - FOOTER_H), area.max);

        for &idx in &[a, b] {
            let size = self.panes[idx].media.size();
            let v = self.view_mut(idx);
            if v.needs_fit {
                v.fit(size, img);
            }
        }

        let (ta, la) = self.prepare(ctx, a);
        let (tb, lb) = self.prepare(ctx, b);
        let now = ctx.input(|i| i.time);
        let split_x = img.min.x + self.ab_split.clamp(0.02, 0.98) * img.width();
        let left = Rect::from_min_max(img.min, Pos2::new(split_x, img.max.y));
        let right = Rect::from_min_max(Pos2::new(split_x, img.min.y), img.max);

        self.draw_ab_side(ui, a, ta, la, img, left, true, now);
        self.draw_ab_side(ui, b, tb, lb, img, right, false, now);
        self.draw_pane_error(ui, a, left);
        self.draw_pane_error(ui, b, right);

        // Divider line + grab handle.
        let p = ui.painter_at(img);
        p.line_segment(
            [Pos2::new(split_x, img.min.y), Pos2::new(split_x, img.max.y)],
            Stroke::new(2.0, Color32::from_gray(240)),
        );
        let knob = Pos2::new(split_x, img.center().y);
        p.circle_filled(knob, 9.0, Color32::from_gray(240));
        p.text(
            knob,
            Align2::CENTER_CENTER,
            "↔",
            FontId::proportional(12.0),
            Color32::from_gray(30),
        );

        // Interaction: divider drag, else pan/zoom the side under the cursor.
        let sense = if self.selecting_region {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let resp = ui.interact(img, Id::new("ab_area"), sense);
        let ptr = ctx.input(|i| i.pointer.interact_pos());
        if !self.selecting_region {
            if resp.drag_started() {
                self.ab_handle_grabbed = ptr.map_or(false, |p| (p.x - split_x).abs() <= HANDLE_HIT);
            }
            if resp.dragged() {
                let d = resp.drag_delta();
                if self.ab_handle_grabbed {
                    self.ab_split = ((split_x + d.x - img.min.x) / img.width()).clamp(0.02, 0.98);
                } else if let Some(pos) = ptr {
                    let side = if pos.x < split_x { a } else { b };
                    self.view_mut(side).pan(d);
                }
            }
            if resp.drag_stopped() {
                self.ab_handle_grabbed = false;
            }
            if resp.hovered() {
                let scroll = wheel_delta(ctx);
                if scroll != 0.0 {
                    if let Some(pos) = ptr {
                        let side = if pos.x < split_x { a } else { b };
                        let speed = zoom_speed(ctx);
                        self.view_mut(side).zoom_at((scroll * speed).exp(), pos, img);
                    }
                }
            }
        }

        // Footer readout for whichever side the cursor is over.
        let hover = resp.hover_pos();
        let side = hover.map(|pos| if pos.x < split_x { a } else { b });
        self.draw_footer(ui, side.unwrap_or(a), hover, img, footer);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_ab_side(
        &self,
        ui: &egui::Ui,
        idx: usize,
        tex: Option<TextureId>,
        loading: bool,
        area: Rect,
        clip: Rect,
        is_a: bool,
        now: f64,
    ) {
        let painter = ui.painter_at(clip);
        painter.rect_filled(clip, 0.0, Color32::from_gray(18));
        if let Some(id) = tex {
            let rect = self.view_ref(idx).image_rect(self.disp_size(idx), area);
            painter.image(id, rect, uv(), Color32::WHITE);
        }
        if loading {
            draw_spinner(&painter, clip, now);
        }
        // Corner label.
        let tag = format!(
            "{}  {}",
            if is_a { "A" } else { "B" },
            self.panes[idx].media.name()
        );
        let anchor = if is_a {
            (clip.left_top() + Vec2::new(8.0, 8.0), Align2::LEFT_TOP)
        } else {
            (clip.right_top() + Vec2::new(-8.0, 8.0), Align2::RIGHT_TOP)
        };
        painter.text(
            anchor.0,
            anchor.1,
            tag,
            FontId::proportional(13.0),
            Color32::from_gray(230),
        );
    }

    // ---- windows ---------------------------------------------------------

    fn draw_manager(&mut self, ctx: &egui::Context) {
        let mut open = self.show_manager;
        let shared_view = self.shared_view;
        let shared_frame = self.shared_frame;

        egui::Window::new("☰ Media")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                if self.panes.is_empty() {
                    ui.label("No media open. Use 📂 Open or drop files onto the window.");
                    return;
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("media_table")
                        .num_columns(9)
                        .striped(true)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Show");
                            ui.label("#");
                            ui.label("Name");
                            ui.label("Frames");
                            ui.label("Single");
                            ui.label("A / B");
                            ui.label("Sync");
                            ui.label("Clip");
                            ui.label("");
                            ui.end_row();

                            // Aggregate row: each toggle here drives the matching
                            // column for every media below it. Single / A / B are
                            // single-target selectors, so they get no aggregate.
                            {
                                let mut all_vis = self.panes.iter().all(|p| p.visible);
                                if ui
                                    .checkbox(&mut all_vis, "")
                                    .on_hover_text("Show / hide all")
                                    .changed()
                                {
                                    for p in &mut self.panes {
                                        p.visible = all_vis;
                                    }
                                }
                                ui.label("");
                                ui.strong("all");
                                ui.label("");
                                ui.label(""); // Single
                                ui.label(""); // A / B
                                ui.horizontal(|ui| {
                                    let mut all_pos = self.panes.iter().all(|p| p.sync_spatial);
                                    if ui.checkbox(&mut all_pos, "Pos").changed() {
                                        for p in &mut self.panes {
                                            if !all_pos && p.sync_spatial {
                                                p.transform = shared_view;
                                            }
                                            p.sync_spatial = all_pos;
                                        }
                                    }
                                    let mut all_time = self.panes.iter().all(|p| p.sync_temporal);
                                    if ui.checkbox(&mut all_time, "Time").changed() {
                                        for p in &mut self.panes {
                                            if !all_time && p.sync_temporal {
                                                p.frame = shared_frame;
                                            }
                                            p.sync_temporal = all_time;
                                        }
                                    }
                                });
                                let mut all_clip = self.panes.iter().all(|p| p.clip);
                                if ui
                                    .checkbox(&mut all_clip, "0.01%")
                                    .on_hover_text("Clip all")
                                    .changed()
                                {
                                    for p in &mut self.panes {
                                        if p.clip != all_clip {
                                            p.clip = all_clip;
                                            p.tex = None; // rebuild with new mapping
                                        }
                                    }
                                }
                                if ui
                                    .small_button("⟳")
                                    .on_hover_text("Reload all from disk")
                                    .clicked()
                                {
                                    self.pending_reload_all = true;
                                }
                                ui.end_row();
                            }

                            let mut to_remove = None;
                            let mut to_reload = None;
                            for i in 0..self.panes.len() {
                                let count = self.panes[i].media.frame_count();
                                let resident = self.panes[i].media.resident_count();

                                ui.checkbox(&mut self.panes[i].visible, "");

                                ui.monospace(format!("{}", i + 1));

                                let name = self.panes[i].media.name().to_string();
                                ui.label(ellipsize(&name, 26));

                                if count > 1 {
                                    ui.monospace(format!("{count}  ({resident}◈)"));
                                } else {
                                    ui.monospace("still");
                                }

                                if ui
                                    .selectable_label(self.current == i, "▢")
                                    .on_hover_text("Show alone in Single view")
                                    .clicked()
                                {
                                    self.current = i;
                                    self.mode = Mode::Single;
                                }

                                ui.horizontal(|ui| {
                                    if ui.selectable_label(self.slot_a == i, "A").clicked() {
                                        self.slot_a = i;
                                    }
                                    if ui.selectable_label(self.slot_b == i, "B").clicked() {
                                        self.slot_b = i;
                                    }
                                });

                                ui.horizontal(|ui| {
                                    let mut ss = self.panes[i].sync_spatial;
                                    if ui.checkbox(&mut ss, "Pos").changed() {
                                        if !ss {
                                            self.panes[i].transform = shared_view;
                                        }
                                        self.panes[i].sync_spatial = ss;
                                    }
                                    let mut st = self.panes[i].sync_temporal;
                                    if ui.checkbox(&mut st, "Time").changed() {
                                        if !st {
                                            self.panes[i].frame = shared_frame;
                                        }
                                        self.panes[i].sync_temporal = st;
                                    }
                                });

                                let mut clip = self.panes[i].clip;
                                if ui
                                    .checkbox(&mut clip, "0.01%")
                                    .on_hover_text("Percentile auto-contrast for this media")
                                    .changed()
                                {
                                    self.panes[i].clip = clip;
                                    self.panes[i].tex = None; // rebuild with new mapping
                                }

                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button("⟳")
                                        .on_hover_text("Reload this media from disk")
                                        .clicked()
                                    {
                                        to_reload = Some(i);
                                    }
                                    if ui.small_button("×").clicked() {
                                        to_remove = Some(i);
                                    }
                                });
                                ui.end_row();
                            }

                            if let Some(i) = to_remove {
                                self.pending_remove = Some(i);
                            }
                            if let Some(i) = to_reload {
                                self.pending_reload = Some(i);
                            }
                        });
                });
            });
        self.show_manager = open;
    }

    fn draw_vis(&mut self, ctx: &egui::Context) {
        self.update_histogram();
        let mut open = self.show_vis;
        let mut changed = false;
        egui::Window::new("Visualise")
            .open(&mut open)
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Interpolation");
                    egui::ComboBox::from_id_salt("interp")
                        .selected_text(match self.config.vis.interp {
                            Interpolation::Nearest => "Nearest",
                            Interpolation::Bilinear => "Bilinear",
                        })
                        .show_ui(ui, |ui| {
                            changed |= ui
                                .selectable_value(
                                    &mut self.config.vis.interp,
                                    Interpolation::Nearest,
                                    "Nearest",
                                )
                                .changed();
                            changed |= ui
                                .selectable_value(
                                    &mut self.config.vis.interp,
                                    Interpolation::Bilinear,
                                    "Bilinear",
                                )
                                .changed();
                        });
                });

                ui.add_space(6.0);
                ui.separator();
                ui.heading("Histogram");
                self.draw_histogram(ui);
            });

        if changed {
            // Rebuild textures so filter/clip changes are visible immediately.
            for p in &mut self.panes {
                p.tex = None;
            }
            self.config.save();
        }
        self.show_vis = open;
    }

    /// Recompute the histogram of the focused media/frame when it changes.
    fn update_histogram(&mut self) {
        if self.panes.is_empty() {
            self.hist = None;
            return;
        }
        let cur = self.current.min(self.panes.len() - 1);
        let f = self.frame_disp(cur);
        let key = (self.panes[cur].id, f);
        if self.hist.as_ref().map(|h| h.key) == Some(key) {
            return;
        }
        if let Some(frame) = self.panes[cur].media.resident(f) {
            self.hist = Some(HistCache {
                key,
                data: frame.histogram_display(256),
            });
        }
    }

    fn draw_histogram(&self, ui: &mut egui::Ui) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 140.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(16));

        let Some(hist) = &self.hist else { return };
        let data = &hist.data;

        // Peak across every channel/bin; sqrt scaling makes tails legible.
        let peak = data
            .bins
            .iter()
            .flat_map(|c| c.iter().copied())
            .max()
            .unwrap_or(1)
            .max(1) as f32;

        let colors: &[Color32] = if data.mono {
            &[Color32::from_gray(210)]
        } else {
            &[
                Color32::from_rgb(230, 90, 90),
                Color32::from_rgb(90, 210, 90),
                Color32::from_rgb(100, 140, 240),
            ]
        };

        for (ci, chan) in data.bins.iter().enumerate() {
            let nb = chan.len().max(2);
            let mut pts = Vec::with_capacity(nb);
            for (v, &count) in chan.iter().enumerate() {
                let x = rect.left() + (v as f32 / (nb - 1) as f32) * rect.width();
                let h = (count as f32 / peak).sqrt();
                let y = rect.bottom() - h * rect.height();
                pts.push(Pos2::new(x, y));
            }
            painter.add(egui::Shape::line(pts, Stroke::new(1.0, colors[ci])));
        }

        // True value extent under the graph: min at left, max at right.
        // Whole numbers (integer sources) print plainly; floats get 4 digits.
        let fmt = |v: f32| -> String {
            if v.fract() == 0.0 {
                format!("{}", v as i64)
            } else {
                format!("{v:.4}")
            }
        };
        ui.horizontal(|ui| {
            ui.monospace(format!("min {}", fmt(data.min)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(format!("max {}", fmt(data.max)));
            });
        });
    }

    // ---- export ----------------------------------------------------------

    fn toggle_export(&mut self) {
        self.show_export = !self.show_export;
        if self.show_export {
            self.export_mode = self.mode; // default to what's on screen
        } else if self.selecting_region {
            // Panel closed mid-selection: abandon it and restore the view.
            self.selecting_region = false;
            self.sel_start = None;
            self.sel_rect = None;
            if let Some(m) = self.pre_select_mode.take() {
                self.mode = m;
            }
        }
    }

    /// Snapshot a participating pane for the export plan.
    fn export_pane(&self, idx: usize) -> ExportPane {
        let p = &self.panes[idx];
        let source = match p.media.decode_job(0) {
            Some(path) => ExportSource::Seq { path },
            None => ExportSource::Still(
                p.media.resident(0).expect("still frame always resident"),
            ),
        };
        ExportPane::new(
            *self.view_ref(idx),
            p.clip,
            p.media.frame_count(),
            p.sync_temporal,
            p.frame,
            source,
        )
    }

    /// The composition-space rect the export renders (fixes the output aspect).
    /// With an image-space crop, panes become cells of exactly the crop's pixel
    /// size laid out side by side; without one it's the live screen area.
    fn export_canvas(&self) -> Rect {
        match self.export_region {
            Some(reg) => {
                let (w, h) = (reg.width(), reg.height());
                match self.export_mode {
                    Mode::Grid => {
                        let n = self.visible_indices().len().max(1);
                        let cols = self.config.max_columns.max(1).min(n);
                        let rows = (n + cols - 1) / cols;
                        Rect::from_min_size(
                            Pos2::ZERO,
                            Vec2::new(cols as f32 * w, rows as f32 * h),
                        )
                    }
                    Mode::Single | Mode::Ab => {
                        Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))
                    }
                }
            }
            None => self.last_area,
        }
    }

    /// Inclusive (start, end) of the exported timeline range, clamped to what's
    /// currently known of the timeline. None = start to finish.
    fn export_frames(&self) -> (usize, usize) {
        let tl = self.timeline_len().max(1);
        let (s, e) = self.export_range.unwrap_or((0, tl - 1));
        let s = s.min(tl - 1);
        (s, e.clamp(s, tl - 1))
    }

    fn build_export_plan(&self) -> Result<ExportPlan, String> {
        if self.panes.is_empty() {
            return Err("No media to export".into());
        }
        let area = self.last_area;
        if self.export_region.is_none() && area.width() < 2.0 {
            return Err("View not ready yet".into());
        }
        let crop = self.export_region;
        let region = self.export_canvas();
        let (out_w, out_h) = export::out_dims(region, self.out_height);
        // Always nearest for export: source pixels are sampled directly onto the
        // output grid, so blending would soften detail the comparison exists to show.
        let bilinear = false;
        let (start, end) = self.export_frames();
        let total = end - start + 1;

        let mut panes = Vec::new();
        let layout = match self.export_mode {
            Mode::Grid => {
                let vis = self.visible_indices();
                if vis.is_empty() {
                    return Err("No visible media (enable some in ☰ Media)".into());
                }
                let mut v = Vec::new();
                if let Some(reg) = crop {
                    // Side-by-side of just the cropped image region: one cell of
                    // the crop's exact pixel size per pane, nothing outside it.
                    let cols = self.config.max_columns.max(1).min(vis.len());
                    for (k, &idx) in vis.iter().enumerate() {
                        let (r, c) = (k / cols, k % cols);
                        let cell = Rect::from_min_size(
                            Pos2::new(c as f32 * reg.width(), r as f32 * reg.height()),
                            reg.size(),
                        );
                        let mut pane = self.export_pane(idx);
                        pane.view = region_view(reg);
                        panes.push(pane);
                        v.push((k, cell));
                    }
                } else {
                    let cells = self.grid_cells(&vis, area);
                    for (k, &(idx, cell)) in cells.iter().enumerate() {
                        panes.push(self.export_pane(idx));
                        v.push((k, image_area(cell)));
                    }
                }
                ExportLayout::Grid(v)
            }
            Mode::Single => {
                let idx = self.current.min(self.panes.len() - 1);
                let mut pane = self.export_pane(idx);
                let cell = match crop {
                    Some(reg) => {
                        pane.view = region_view(reg);
                        Rect::from_min_size(Pos2::ZERO, reg.size())
                    }
                    None => image_area(area),
                };
                panes.push(pane);
                ExportLayout::Single(0, cell)
            }
            Mode::Ab => {
                let n = self.panes.len();
                let a = self.slot_a.min(n - 1);
                let b = self.slot_b.min(n - 1);
                let mut pa = self.export_pane(a);
                let mut pb = self.export_pane(b);
                let img = match crop {
                    Some(reg) => {
                        pa.view = region_view(reg);
                        pb.view = region_view(reg);
                        Rect::from_min_size(Pos2::ZERO, reg.size())
                    }
                    None => Rect::from_min_max(
                        area.min,
                        Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0),
                    ),
                };
                panes.push(pa);
                panes.push(pb);
                let split_x = img.min.x + self.ab_split.clamp(0.02, 0.98) * img.width();
                ExportLayout::Ab {
                    a: 0,
                    b: 1,
                    img,
                    split_x,
                }
            }
        };

        Ok(ExportPlan {
            panes,
            layout,
            region,
            out_w,
            out_h,
            start,
            total,
            bilinear,
        })
    }

    fn start_export(&mut self) {
        let name = self.export_name.trim();
        if name.is_empty() {
            self.export_status = "Enter an output file name first".into();
            return;
        }
        let name = if Path::new(name).extension().is_some() {
            name.to_string()
        } else {
            format!("{name}.mp4")
        };
        let path = PathBuf::from(&name); // relative -> current working directory
        let plan = match self.build_export_plan() {
            Ok(p) => p,
            Err(e) => {
                self.export_status = e;
                return;
            }
        };
        let (w, h, total) = (plan.out_w, plan.out_h, plan.total);
        match Encoder::start(&path, w, h, self.export_fps, self.crf) {
            Ok(enc) => {
                self.export_status = format!("Exporting {total} frames…");
                self.export_run = Some(ExportRun {
                    enc,
                    plan,
                    frame: 0,
                    total,
                    path: path.display().to_string(),
                });
            }
            Err(e) => self.export_status = e,
        }
    }

    /// Encode one frame per call; driven from `update` while a run is active.
    fn export_tick(&mut self) {
        let Some(mut run) = self.export_run.take() else {
            return;
        };
        if self.cancel_export {
            self.cancel_export = false;
            run.enc.kill();
            self.export_status = "Export cancelled".into();
            return; // run dropped
        }
        if run.frame >= run.total {
            self.export_status = match run.enc.finish() {
                Ok(()) => format!("Exported {} frames → {}", run.total, run.path),
                Err(e) => e,
            };
            return; // run dropped
        }
        let buf = run.plan.compose(run.frame);
        match run.enc.write_frame(&buf) {
            Ok(()) => {
                run.frame += 1;
                self.export_run = Some(run);
            }
            Err(e) => {
                run.enc.kill();
                self.export_status = format!("Export failed: {e}");
            }
        }
    }

    fn draw_export(&mut self, ctx: &egui::Context) {
        let mut open = self.show_export;
        let running = self.export_run.is_some();
        let region = self.export_canvas();
        let (out_w, out_h) = export::out_dims(region, self.out_height);
        let tl = self.timeline_len().max(1);
        let (start, end) = self.export_frames();
        let total = end - start + 1;

        egui::Window::new("Export comparison")
            .open(&mut open)
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    egui::Grid::new("export_grid")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Layout");
                            egui::ComboBox::from_id_salt("exp_layout")
                                .selected_text(match self.export_mode {
                                    Mode::Grid => "Side by side",
                                    Mode::Single => "Single",
                                    Mode::Ab => "A / B wipe",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.export_mode, Mode::Grid, "Side by side");
                                    ui.selectable_value(&mut self.export_mode, Mode::Single, "Single");
                                    ui.selectable_value(&mut self.export_mode, Mode::Ab, "A / B wipe");
                                });
                            ui.end_row();

                            ui.label("Region");
                            ui.horizontal(|ui| {
                                if ui
                                    .button("Select…")
                                    .on_hover_text(
                                        "Drag the crop on a single image; it then applies \
                                         to every pane of the comparison",
                                    )
                                    .clicked()
                                {
                                    // Pick the crop on one image: force Single
                                    // view for the drag, restore after.
                                    if self.mode != Mode::Single {
                                        self.pre_select_mode = Some(self.mode);
                                        self.mode = Mode::Single;
                                    }
                                    self.selecting_region = true;
                                }
                                let has = self.export_region.is_some();
                                if ui.add_enabled(has, egui::Button::new("Full view")).clicked() {
                                    self.export_region = None;
                                }
                                match self.export_region {
                                    Some(r) => ui.label(format!(
                                        "{}×{} px",
                                        r.width().round() as u32,
                                        r.height().round() as u32
                                    )),
                                    None => ui.label("full"),
                                };
                            });
                            ui.end_row();

                            ui.label("Frames");
                            ui.horizontal(|ui| {
                                let mut all = self.export_range.is_none();
                                if ui.checkbox(&mut all, "all").changed() {
                                    self.export_range =
                                        if all { None } else { Some((0, tl - 1)) };
                                }
                                if let Some((s, e)) = self.export_range {
                                    // Shown 1-based; stored 0-based inclusive.
                                    let (mut s1, mut e1) = (s + 1, e + 1);
                                    ui.add(
                                        egui::DragValue::new(&mut s1)
                                            .range(1..=e1)
                                            .prefix("from "),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut e1)
                                            .range(s1..=tl)
                                            .prefix("to "),
                                    );
                                    self.export_range = Some((s1 - 1, e1 - 1));
                                }
                            });
                            ui.end_row();

                            ui.label("Output height");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut self.out_height).range(120..=2160));
                                if ui.button("= view").clicked() {
                                    self.out_height = region.height().round() as u32;
                                }
                                ui.monospace(format!("→ {out_w}×{out_h}"));
                            });
                            ui.end_row();

                            ui.label("Compression");
                            ui.add(
                                egui::Slider::new(&mut self.crf, 0..=51)
                                    .text("CRF")
                                    .custom_formatter(|n, _| format!("{n:.0}")),
                            );
                            ui.end_row();

                            ui.label("FPS");
                            ui.add(egui::DragValue::new(&mut self.export_fps).range(1.0..=60.0));
                            ui.end_row();
                        });
                });

                // Sequence lengths are discovered lazily, so warn when a media's
                // true end isn't known yet — the range above may be short.
                if self.panes.iter().any(|p| !p.media.at_end()) {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            Color32::from_rgb(240, 200, 120),
                            "⚠ Some media aren't fully loaded — frame counts may be incomplete.",
                        );
                        if ui.button("⤓ Load all").clicked() {
                            self.load_all();
                        }
                    });
                }

                ui.label(format!(
                    "{total} frames · {:.1}s",
                    total as f32 / self.export_fps.max(1.0),
                ));

                ui.horizontal(|ui| {
                    ui.label("Save as");
                    ui.add_enabled(
                        !running,
                        egui::TextEdit::singleline(&mut self.export_name).desired_width(180.0),
                    );
                });
                ui.label(
                    egui::RichText::new(format!(
                        "→ {}",
                        ellipsize(
                            &std::env::current_dir()
                                .unwrap_or_default()
                                .display()
                                .to_string(),
                            40
                        )
                    ))
                    .weak()
                    .small(),
                );

                ui.separator();
                if let Some(run) = &self.export_run {
                    ui.add(
                        egui::ProgressBar::new(run.frame as f32 / run.total.max(1) as f32)
                            .text(format!("{}/{}", run.frame, run.total)),
                    );
                    if ui.button("Cancel").clicked() {
                        self.cancel_export = true;
                    }
                } else {
                    let ready = !self.export_name.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Export MP4")).clicked() {
                        self.start_export();
                    }
                }

                if !self.export_status.is_empty() {
                    ui.label(&self.export_status);
                }
            });
        self.show_export = open;
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("⚙ Settings")
            .open(&mut open)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.heading("Layout");
                ui.horizontal(|ui| {
                    ui.label("Max columns");
                    ui.add(egui::Slider::new(&mut self.config.max_columns, 1..=8));
                });
                ui.horizontal(|ui| {
                    ui.label("UI scale");
                    ui.add(
                        egui::Slider::new(&mut self.config.ui_scale, 0.6..=2.0)
                            .suffix("×")
                            .fixed_decimals(2),
                    );
                });

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Keyboard shortcuts");
                ui.add_space(4.0);

                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    egui::Grid::new("keys")
                        .num_columns(3)
                        .striped(true)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            for action in Action::all() {
                                ui.label(action.label());
                                let key_txt = self
                                    .config
                                    .keybindings
                                    .key_for(action)
                                    .map(|k| k.name().to_string())
                                    .unwrap_or_else(|| "—".into());
                                if self.rebinding == Some(action) {
                                    ui.colored_label(
                                        Color32::from_rgb(240, 200, 120),
                                        "press a key…",
                                    );
                                } else {
                                    ui.monospace(key_txt);
                                }
                                ui.horizontal(|ui| {
                                    if ui.small_button("Rebind").clicked() {
                                        self.rebinding = Some(action);
                                    }
                                    if ui.small_button("Clear").clicked() {
                                        self.config.keybindings.clear(action);
                                        self.config.save();
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });

                ui.add_space(8.0);
                ui.separator();
                if ui.button("Save settings").clicked() {
                    self.config.save();
                    self.status = "Settings saved".into();
                }
            });
        self.show_settings = open;
    }
}

impl eframe::App for CimApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Global UI scale (buttons/text).
        let scale = self.config.ui_scale.clamp(0.5, 3.0);
        if (ctx.zoom_factor() - scale).abs() > 1e-3 {
            ctx.set_zoom_factor(scale);
        }

        self.pump_decoder();
        self.handle_input(ctx);
        self.advance_playback(ctx);

        // Discover sequence length lazily: eager "Load all" batches drive to the
        // end, otherwise just keep one page ahead of the cursor.
        self.drive_eager();
        self.ensure_lookahead();
        self.poll_decoding_all();

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

// ---- helpers -------------------------------------------------------------

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
