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
    self, Align2, Color32, ColorImage, FontId, Id, Key, PointerButton, Pos2, Rect, Sense, Stroke,
    TextureHandle, TextureId, TextureOptions, Vec2,
};

use crate::cli;
use crate::decoder::BackgroundDecoder;
use crate::export::{self, Encoder, ExportLayout, ExportPane, ExportPlan, ExportSource};
use crate::media::{self, HistData, Media, Reduce, RegionStats};
use crate::settings::{Action, Config, ContrastMode, ToneOptions};
use crate::view::ViewTransform;
use export_ui::ExportRun;

const HEADER_H: f32 = 24.0;
const FOOTER_H: f32 = 20.0;
const GAP: f32 = 0.0;
const HANDLE_HIT: f32 = 24.0; // px around the A/B divider that grabs it
const MODIFY_W: f32 = 108.0; // width of the header "Transformations" button
const COMPUTE_W: f32 = 62.0; // width of the header "Compute" button

/// How often to repaint while background decodes are pending (and we're not
/// playing or exporting): often enough to pick up landed frames and keep the
/// loading spinner turning, but far below monitor rate so we don't busy-spin —
/// the dominant idle cost over VNC / software rendering. ~30 fps.
const DECODE_POLL: std::time::Duration = std::time::Duration::from_millis(33);

/// Outline / accent colour for the right-drag statistics region (cyan, so it
/// reads distinct from the amber export-region rectangle).
const REGION_COL: Color32 = Color32::from_rgb(90, 210, 230);

// Soft ceiling on decoded frames kept resident across all sequences. Beyond it
// the least-recently-viewed frames are evicted (they re-decode on demand), so a
// long sequence can't grow memory without bound. Configurable in Settings
// (`config.cache_budget_mb`); see `CimApp::cache_budget_bytes`.

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

/// A boolean mask from another pane, tinted and drawn over a pane. Config only
/// (no texture), so it can be shared across tone-synced panes; the tinted
/// texture is cached separately per pane in `Pane.overlay_tex`.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct OverlaySpec {
    src_id: u64, // stable id of the mask pane supplying the overlay
    color: Color32,
    opacity: f32, // 0..1
}

/// Cached histogram for the media shown in the Visualise panel.
struct HistCache {
    key: (u64, usize), // (pane id, frame) this was computed for
    data: HistData,
}

/// Cached statistics for a pane's current view of the shared stats region.
/// Recomputed when the frame or the region (via `stats_gen`) changes.
struct RegionStatsCache {
    key: (usize, u64), // (frame, stats_gen)
    data: RegionStats,
}

/// How a pane's media was opened, so it can be reloaded from disk and emitted
/// back into a replay command.
enum Source {
    /// A single file (still or multi-page TIFF).
    File(PathBuf),
    /// A numbered still sequence: the compact `PREFIX%0Nd,START,END.EXT` token
    /// it was opened from, plus the individual frame files.
    Sequence { token: String, files: Vec<PathBuf> },
    /// A computed image (Compute pane) — generated in memory from another pane's
    /// frames, not backed by a file. `reload` recomputes it.
    Computed,
}

/// A Compute pane: derives a single displayed image from other panes — a
/// mean/std reduction across one source's resident frames, or a per-pixel
/// difference of two sources' current frames — with an inline Save.
struct Compute {
    kind: Reduce,
    /// Stable id of the source pane (source A for `Diff`), if chosen.
    source_id: Option<u64>,
    /// Second source (B) for `Diff`; unused by the reductions.
    source_b: Option<u64>,
    /// Recompute automatically whenever the inputs' shown frame(s) change.
    auto: bool,
    /// Input signature at the last (attempted) compute, so auto-refresh only
    /// recomputes when something actually changed. See `compute_sig`.
    last_sig: u64,
    /// Save UI expanded (showing the file-name input).
    saving: bool,
    save_name: String,
    /// Short result / error line shown in the controls.
    status: String,
}

/// A Compute panel being configured before its result pane exists: shown as a
/// floating panel where the header "Compute" button was clicked. Picks the mode
/// and source(s); its "Compute" button realizes the result pane (`open_compute`),
/// after which the controls live on that pane (see [`Compute`]).
struct ComputeDraft {
    /// Top-left of the floating panel (the click location).
    pos: Pos2,
    kind: Reduce,
    source_id: Option<u64>,
    source_b: Option<u64>,
}

/// One opened media plus its per-pane view/timeline state.
struct Pane {
    id: u64, // stable across reorder/close; matches background-decode results
    source: Source, // how to reload it / re-emit it in a replay command
    media: Media,
    tex: Option<CachedTex>,
    transform: ViewTransform, // used only when !sync_spatial
    frame: usize,             // used only when !sync_temporal
    sync_spatial: bool,
    sync_temporal: bool,
    /// Follow the shared "Transformations" (tone + options + details) instead of
    /// this pane's own — synced across selected rows in the media manager.
    sync_tone: bool,
    visible: bool,
    /// Per-pane tone-mapping mode (Linear or proprietary LUT_ALPHA).
    contrast: ContrastMode,
    /// Per-mode tone options (clip percentile, LUT_ALPHA knobs, …), edited in
    /// the pane's "Modify" popup.
    tone: ToneOptions,
    /// The "Modify" options popup is open for this pane.
    show_opts: bool,
    /// Per-pane proprietary DETAILS_ENHANCED detail enhancement.
    details: bool,
    /// Optional boolean-mask overlay drawn on top of this pane (config only;
    /// shared across synced panes via `overlay_of`).
    overlay: Option<OverlaySpec>,
    /// Cached tinted overlay texture for this pane (rebuilt when the effective
    /// overlay config or the mask's shown frame changes).
    overlay_tex: Option<CachedTex>,
    /// When set, this pane's tone bounds come from the shared stats region
    /// instead of the whole image (min/max, or 0.01% clip). Replicated to every
    /// pane by the "Tone ⟵ region" button.
    region_tone: bool,
    /// Cached statistics of the shared region for this pane's current frame.
    stats: Option<RegionStatsCache>,
    /// Cached histogram for this pane's current frame (Transformations popup).
    /// Per pane so multiple open popups don't thrash one shared cache.
    hist: Option<HistCache>,
    /// Present iff this is a Compute pane (its media is a generated still).
    compute: Option<Compute>,
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
    /// Shared "Transformations" (tone mode + options + details) that every pane
    /// with `sync_tone` follows, so editing one synced pane's Transformations
    /// popup updates them all.
    shared_contrast: ContrastMode,
    shared_tone: ToneOptions,
    shared_details: bool,
    /// Shared mask overlay (rides the same `sync_tone` as the tone).
    shared_overlay: Option<OverlaySpec>,
    /// A requested timeline frame not yet reachable because the sequence's
    /// length is still being discovered (e.g. from `--frame` at launch). While
    /// set, discovery is driven forward until this frame exists, then the
    /// timeline jumps to it. Cleared by any manual frame navigation.
    pending_seek: Option<usize>,

    mode: Mode,
    current: usize, // focused pane (single view / keyboard target)
    /// Pane whose sequence drives the shared timeline / playback / scrubber.
    /// Decoupled from `current` so selecting a still to view doesn't take over
    /// (or hide) the transport. Kept pointing at a sequence by `ensure_control`.
    control: usize,
    slot_a: usize,  // A/B view operands
    slot_b: usize,
    ab_split: f32, // 0..1 divider position
    ab_handle_grabbed: bool,

    playing: bool,
    /// Loop the sequence when playback reaches the end (on by default). When
    /// off, playback stops on the last frame instead of wrapping.
    loop_playback: bool,
    /// Inclusive frame sub-range to loop over on the control sequence; `None`
    /// loops the whole (discovered) sequence. Set by dragging the timeline
    /// brackets, reset to full by the loop-range button.
    loop_range: Option<(usize, usize)>,
    /// Which loop bracket the pointer is dragging: `Some(true)` = start (left),
    /// `Some(false)` = end (right); `None` = not dragging a bracket.
    loop_drag: Option<bool>,
    fps: f32,
    play_accum: f32,

    show_settings: bool,
    show_manager: bool,
    /// The "View command" window: shows a `cim …` line that reopens the current
    /// files at the current view, for copying / sharing.
    show_viewcmd: bool,
    rebinding: Option<Action>,
    /// The Compute panel being configured (opened by a header "Compute" button,
    /// floating where it was clicked) before its result pane exists.
    compute_draft: Option<ComputeDraft>,
    /// The draft's "Compute" button was clicked: realize `compute_draft` into a
    /// pane after the draw (deferred to avoid growing `panes` mid-draw).
    pending_compute_create: bool,

    /// Draw the per-region stats panels (histogram + numbers + LUT button).
    /// Toggled by the button in the panel's top-left corner; when off, a small
    /// button under the region brings it back. The outline stays visible.
    show_stats: bool,
    /// Right-drag statistics region, in IMAGE space (like `export_region`), so
    /// the same crop and its per-pane stats replicate across every pane. `None`
    /// = no region selected.
    stats_region: Option<Rect>,
    /// Bumped whenever `stats_region` changes, so cached per-pane stats and
    /// region-tone textures know to recompute.
    stats_gen: u64,
    /// In-progress right-drag: anchor / current screen positions, the pane it
    /// started on, and that pane's coordinate area (for screen↔image mapping).
    stats_sel_start: Option<Pos2>,
    stats_sel_now: Option<Pos2>,
    stats_sel_pane: Option<usize>,
    stats_sel_area: Rect,

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
    /// Row being dragged to reorder in the ☰ Media manager (a pane vec index).
    manager_drag: Option<usize>,
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
    /// Reused RGBA scratch buffer for texture rendering, so `prepare` doesn't
    /// allocate a full-image buffer on every frame change.
    render_scratch: Vec<u8>,
}

impl CimApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        inputs: Vec<cli::Input>,
        view: cli::ViewState,
    ) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        // Square corners everywhere: windows, menus, and every widget state.
        let sq = egui::Rounding::ZERO;
        style.visuals.window_rounding = sq;
        style.visuals.menu_rounding = sq;
        for w in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            w.rounding = sq;
        }
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
            shared_contrast: ContrastMode::LinearClip,
            shared_tone: ToneOptions::default(),
            shared_details: false,
            shared_overlay: None,
            pending_seek: None,
            mode: Mode::Grid,
            current: 0,
            control: 0,
            slot_a: 0,
            slot_b: 0,
            ab_split: 0.5,
            ab_handle_grabbed: false,
            playing: false,
            loop_playback: true,
            loop_range: None,
            loop_drag: None,
            fps: 12.0,
            play_accum: 0.0,
            show_settings: false,
            show_manager: false,
            show_viewcmd: false,
            rebinding: None,
            compute_draft: None,
            pending_compute_create: false,
            show_stats: true,
            stats_region: None,
            stats_gen: 0,
            stats_sel_start: None,
            stats_sel_now: None,
            stats_sel_pane: None,
            stats_sel_area: Rect::NOTHING,

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
            manager_drag: None,
            pending_remove: None,
            pending_reload: None,
            pending_reload_all: false,
            decoder: BackgroundDecoder::new(threads),
            inflight: HashSet::new(),
            decoding_all: false,
            clock: 0,
            render_scratch: Vec::new(),
        };
        app.open_inputs(inputs);
        app.apply_view_state(view);
        app
    }

    /// Apply a viewpoint parsed from the command line (see `cli::ViewState`).
    /// Called once after the startup files are opened. Only the fields that were
    /// present on the command line change anything; the rest keep their defaults.
    pub(super) fn apply_view_state(&mut self, vs: cli::ViewState) {
        if let Some(c) = vs.cols {
            self.config.max_columns = c.clamp(1, 8);
        }
        if let Some(m) = vs.mode {
            self.mode = match m {
                cli::ViewMode::Grid => Mode::Grid,
                cli::ViewMode::Single => Mode::Single,
                cli::ViewMode::Ab => Mode::Ab,
            };
        }
        let n = self.panes.len();
        if let Some(p) = vs.pane {
            if n > 0 {
                self.current = p.min(n - 1);
            }
        }
        if let Some((a, b, split)) = vs.ab {
            if n > 0 {
                self.slot_a = a.min(n - 1);
                self.slot_b = b.min(n - 1);
            }
            self.ab_split = split.clamp(0.02, 0.98);
        }
        if let Some(f) = vs.frame {
            // The sequence length isn't discovered yet, so we can't land on `f`
            // now — record it and let `drive_seek` walk discovery up to it.
            self.shared_frame = f;
            self.pending_seek = Some(f);
        }
        // Per-pane tone / detail (each list positional over the panes). These
        // are per-pane, so unsync those panes' Transformations (which default to
        // synced) — otherwise the restored per-pane tone wouldn't take effect.
        if let Some(tones) = &vs.tones {
            for (p, t) in self.panes.iter_mut().zip(tones) {
                p.contrast = match t {
                    cli::Tone::Linear => ContrastMode::Linear,
                    cli::Tone::LinearClip => ContrastMode::LinearClip,
                    cli::Tone::LutAlpha => ContrastMode::LutAlpha,
                };
                p.sync_tone = false;
                p.tex = None; // re-render with the restored mapping
            }
        }
        if let Some(details) = &vs.details {
            for (p, d) in self.panes.iter_mut().zip(details) {
                p.details = *d;
                p.sync_tone = false;
                p.tex = None;
            }
        }
        if let Some((lo, hi)) = vs.loop_range {
            self.loop_range = Some((lo, hi));
        }
        // A restored zoom/centre is an explicit view, so suppress the auto-fit
        // that would otherwise run on first draw.
        if vs.zoom.is_some() || vs.center.is_some() {
            if let Some(z) = vs.zoom {
                self.shared_view.zoom = z.clamp(1e-4, 512.0);
            }
            if let Some((x, y)) = vs.center {
                self.shared_view.center = Vec2::new(x, y);
            }
            self.shared_view.needs_fit = false;
        }
    }

    /// Build a `cim …` command line that reopens the current files at the
    /// current shared view. Captures the layout, columns, shared zoom/pan, the
    /// timeline frame, the focused pane and (in A/B) the operands + split.
    ///
    /// Only the *shared* view is captured — panes with their own view (sync off)
    /// fall back to it. Sequences are listed as their individual files (the
    /// compact `PREFIX%0Nd,…` token isn't reconstructed).
    pub(super) fn view_command(&self) -> String {
        let mut parts: Vec<String> = vec!["cim".into()];
        for p in &self.panes {
            // Re-emit a numbered sequence as its compact token so a replay
            // reopens it as one sequence (not a pane per file).
            match &p.source {
                Source::File(path) => parts.push(quote_path(path)),
                Source::Sequence { token, .. } => parts.push(quote_arg(token)),
                // A computed image isn't reproducible from a CLI path; skip it.
                Source::Computed => {}
            }
        }
        let mode = match self.mode {
            Mode::Grid => "grid",
            Mode::Single => "single",
            Mode::Ab => "ab",
        };
        parts.push(format!("--mode {mode}"));
        parts.push(format!("--cols {}", self.config.max_columns));
        let v = self.shared_view;
        parts.push(format!("--zoom {:.4}", v.zoom));
        parts.push(format!("--center {:.2},{:.2}", v.center.x, v.center.y));
        if self.timeline_len() > 1 {
            parts.push(format!("--frame {}", self.shared_frame));
        }
        // Per-pane tone / detail (effective — shared when tone-synced), in pane
        // order, so a replay reproduces them.
        if !self.panes.is_empty() {
            let tones: Vec<&str> = (0..self.panes.len())
                .map(|i| match self.contrast_of(i) {
                    ContrastMode::Linear => "linear",
                    ContrastMode::LinearClip => "linearclip",
                    ContrastMode::LutAlpha => "lutalpha",
                })
                .collect();
            parts.push(format!("--tone {}", tones.join(",")));
            let details: Vec<&str> = (0..self.panes.len())
                .map(|i| if self.details_of(i) { "1" } else { "0" })
                .collect();
            parts.push(format!("--detail {}", details.join(",")));
        }
        if let Some((lo, hi)) = self.loop_range {
            parts.push(format!("--loop {lo},{hi}"));
        }
        if !self.panes.is_empty() {
            parts.push(format!("--pane {}", self.current.min(self.panes.len() - 1)));
            if self.mode == Mode::Ab {
                parts.push(format!(
                    "--ab {},{},{:.3}",
                    self.slot_a, self.slot_b, self.ab_split
                ));
            }
        }
        parts.join(" ")
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

    /// Open plain paths (from the file dialog or a drag-and-drop) — each becomes
    /// its own pane. Sequences only come from the command line (`open_inputs`).
    pub(super) fn open_paths(&mut self, paths: Vec<PathBuf>) {
        self.open_inputs(paths.into_iter().map(cli::Input::Single).collect());
    }

    /// Open a list of CLI inputs: a `Single` becomes one media, a `Sequence`
    /// becomes a single numbered-file sequence media (one pane, not one per file).
    pub(super) fn open_inputs(&mut self, inputs: Vec<cli::Input>) {
        for input in inputs {
            let (loaded, source) = match input {
                cli::Input::Single(p) => (media::load(&p), Source::File(p)),
                cli::Input::Sequence { token, files } => (
                    media::load_sequence(&files, token.clone()),
                    Source::Sequence { token, files },
                ),
            };
            match loaded {
                Ok(m) => self.add_pane(m, source),
                Err(e) => self.error_popup = Some(format!("Failed to open:\n{e}")),
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

    /// Push a freshly loaded media as a new pane with default per-pane state.
    fn add_pane(&mut self, media: Media, source: Source) {
        let id = self.next_id;
        self.next_id += 1;
        // >8-bit sources need auto-contrast to be legible; 8-bit displays 1:1,
        // so it defaults to a plain identity map.
        let contrast = if media.hi_depth() {
            ContrastMode::LinearClip
        } else {
            ContrastMode::Linear
        };
        let tone = ToneOptions::default();
        // Transformations sync is on by default; the first opened media seeds the
        // shared set (so its depth-appropriate tone becomes the group default).
        if self.panes.is_empty() {
            self.shared_contrast = contrast;
            self.shared_tone = tone;
            self.shared_details = false;
        }
        self.panes.push(Pane {
            id,
            source,
            media,
            tex: None,
            transform: ViewTransform::default(),
            frame: 0,
            sync_spatial: true,
            sync_temporal: true,
            sync_tone: true,
            visible: true,
            contrast,
            tone,
            show_opts: false,
            details: false,
            overlay: None,
            overlay_tex: None,
            region_tone: false,
            stats: None,
            hist: None,
            compute: None,
            error: None,
            eager: false,
        });
    }

    pub(super) fn remove_media(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let removed_id = self.panes[i].id;
        self.decoder.forget(removed_id); // drop its persistent reader
        self.panes.remove(i);
        // Drop any overlay (own or shared) that pointed at the removed mask, and
        // clear cached overlay textures that referenced it.
        if self
            .shared_overlay
            .is_some_and(|o| o.src_id == removed_id)
        {
            self.shared_overlay = None;
        }
        for p in &mut self.panes {
            if p.overlay.is_some_and(|o| o.src_id == removed_id) {
                p.overlay = None;
            }
            p.overlay_tex = None;
        }
        let n = self.panes.len();
        let fix = |v: &mut usize| {
            if *v > i {
                *v -= 1;
            }
            *v = (*v).min(n.saturating_sub(1));
        };
        fix(&mut self.current);
        fix(&mut self.control);
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
        // A Compute pane has no file to reload; refresh it from current memory.
        if matches!(self.panes[i].source, Source::Computed) {
            self.recompute_pane(i);
            return;
        }
        let loaded = match &self.panes[i].source {
            Source::File(p) => media::load(p),
            Source::Sequence { token, files } => media::load_sequence(files, token.clone()),
            Source::Computed => unreachable!(),
        };
        match loaded {
            Ok(m) => {
                let id = self.panes[i].id;
                self.decoder.forget(id); // reopen the file for its fresh contents
                // Drop stale in-flight decodes aimed at the old contents.
                self.inflight.retain(|(pid, _)| *pid != id);
                self.panes[i].media = m;
                self.panes[i].tex = None; // re-render the kept frame from fresh data
                self.panes[i].stats = None; // recompute region stats from fresh data
                self.panes[i].hist = None; // recompute histogram from fresh data
                self.panes[i].error = None;
                // If this is a mask, invalidate overlay textures whose effective
                // source is it, so they rebuild from the reloaded contents.
                let shared_src = self.shared_overlay.map(|o| o.src_id);
                for p in &mut self.panes {
                    let eff = if p.sync_tone {
                        shared_src
                    } else {
                        p.overlay.map(|o| o.src_id)
                    };
                    if eff == Some(id) {
                        p.overlay_tex = None;
                    }
                }
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

    // ---- compute panes ---------------------------------------------------

    /// Panes usable as a Compute source for `kind`: any non-compute pane, but
    /// the reductions (mean/std) also need ≥2 frames. `Diff` accepts stills.
    pub(super) fn compute_sources(&self, kind: Reduce) -> Vec<(u64, String)> {
        self.panes
            .iter()
            .filter(|p| p.compute.is_none())
            .filter(|p| matches!(kind, Reduce::Diff) || p.media.frame_count() > 1)
            .map(|p| (p.id, p.media.name().to_string()))
            .collect()
    }

    fn pane_idx(&self, id: u64) -> Option<usize> {
        self.panes.iter().position(|p| p.id == id)
    }

    /// Realize a configured `ComputeDraft` into a new Compute pane and compute it
    /// once. The floating draft panel is gone; its controls now live on the pane.
    fn open_compute(&mut self, draft: ComputeDraft) {
        let was_empty = self.panes.is_empty();
        self.add_pane(
            media::Media::still("Compute".into(), media::placeholder_frame()),
            Source::Computed,
        );
        let i = self.panes.len() - 1;
        self.panes[i].compute = Some(Compute {
            kind: draft.kind,
            source_id: draft.source_id,
            source_b: draft.source_b,
            auto: false,
            last_sig: 0,
            saving: false,
            save_name: "computed.tif".into(),
            status: String::new(),
        });
        self.current = i;
        if was_empty {
            self.shared_view.needs_fit = true;
        }
        self.recompute_pane(i);
    }

    /// Mean/std reduction of a source's resident frames → (frame, name, status).
    fn compute_reduce(
        &self,
        source_id: Option<u64>,
        kind: Reduce,
    ) -> Result<(media::FrameData, String, String), String> {
        let src_id = source_id.ok_or_else(|| "Pick a source sequence".to_string())?;
        let src = self
            .panes
            .iter()
            .find(|p| p.id == src_id)
            .ok_or_else(|| "Source no longer available".to_string())?;
        let base = src.media.name().to_string();
        let cnt = src.media.frame_count();
        let frames: Vec<std::sync::Arc<media::FrameData>> =
            (0..cnt).filter_map(|f| src.media.resident(f)).collect();
        let used = frames.len();
        let fr =
            media::reduce_frames(&frames, kind).ok_or_else(|| "No source frames in memory".to_string())?;
        let name = format!("{} · {}", kind.label(), base);
        let status = format!("{} of {used} frame(s) in memory", kind.label());
        Ok((fr, name, status))
    }

    /// Per-pixel difference of two sources' *current* frames → (frame, name,
    /// status). Both current frames must be resident and share size/channels.
    fn compute_diff(
        &self,
        a_id: Option<u64>,
        b_id: Option<u64>,
    ) -> Result<(media::FrameData, String, String), String> {
        let a_id = a_id.ok_or_else(|| "Pick source A".to_string())?;
        let b_id = b_id.ok_or_else(|| "Pick source B".to_string())?;
        let ia = self.pane_idx(a_id).ok_or_else(|| "Source A no longer available".to_string())?;
        let ib = self.pane_idx(b_id).ok_or_else(|| "Source B no longer available".to_string())?;
        let (fa, fb) = (self.frame_disp(ia), self.frame_disp(ib));
        let a = self.panes[ia]
            .media
            .resident(fa)
            .ok_or_else(|| "A's current frame not in memory".to_string())?;
        let b = self.panes[ib]
            .media
            .resident(fb)
            .ok_or_else(|| "B's current frame not in memory".to_string())?;
        let fr = media::diff_frames(&a, &b)
            .ok_or_else(|| "A and B differ in size / channels".to_string())?;
        let name = format!(
            "Diff · {} − {}",
            self.panes[ia].media.name(),
            self.panes[ib].media.name()
        );
        let status = format!("Diff of frame {} − {}", fa + 1, fb + 1);
        Ok((fr, name, status))
    }

    /// Recompute a Compute pane from current memory, replacing its displayed
    /// still. Float results default to Linear+Clip so they're legible. The input
    /// signature is recorded either way, so auto-refresh doesn't spin on failure.
    pub(super) fn recompute_pane(&mut self, idx: usize) {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return;
        };
        let (kind, a, b) = (c.kind, c.source_id, c.source_b);
        let result = match kind {
            Reduce::Diff => self.compute_diff(a, b),
            _ => self.compute_reduce(a, kind),
        };
        match result {
            Ok((fr, name, status)) => {
                let hi = fr.hi_depth();
                self.panes[idx].media = media::Media::still(name, fr);
                self.panes[idx].tex = None;
                self.panes[idx].hist = None; // recompute for the new result
                self.panes[idx].contrast = if hi {
                    ContrastMode::LinearClip
                } else {
                    ContrastMode::Linear
                };
                self.set_compute_status(idx, status);
            }
            Err(msg) => self.set_compute_status(idx, msg),
        }
        let sig = self.compute_sig(idx);
        if let Some(c) = self.panes[idx].compute.as_mut() {
            c.last_sig = sig;
        }
    }

    /// A cheap signature of a Compute pane's inputs, so auto-refresh recomputes
    /// only when they change: the shown frames for `Diff`, the source's resident
    /// count for the reductions (which grows as playback decodes more frames).
    fn compute_sig(&self, idx: usize) -> u64 {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return 0;
        };
        let frame_sig = |id: Option<u64>| -> u64 {
            id.and_then(|id| self.pane_idx(id))
                .map(|i| self.frame_disp(i) as u64 + 1)
                .unwrap_or(0)
        };
        match c.kind {
            Reduce::Diff => (frame_sig(c.source_id) << 32) ^ frame_sig(c.source_b),
            _ => c
                .source_id
                .and_then(|id| self.pane_idx(id))
                .map(|i| self.panes[i].media.resident_count() as u64)
                .unwrap_or(0),
        }
    }

    /// Recompute every auto-refresh Compute pane whose inputs changed this frame.
    pub(super) fn refresh_auto_compute(&mut self) {
        for i in 0..self.panes.len() {
            let Some(c) = self.panes[i].compute.as_ref() else {
                continue;
            };
            if c.auto && self.compute_sig(i) != c.last_sig {
                self.recompute_pane(i);
            }
        }
    }

    /// Write the computed image to `name` (relative to the working dir), leaving
    /// the result in memory. Format follows the extension (.tif/.png/.jpg).
    pub(super) fn save_computed(&mut self, idx: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.set_compute_status(idx, "Enter a file name".into());
            return;
        }
        let Some(frame) = self.panes[idx].media.resident(0) else {
            self.set_compute_status(idx, "Nothing computed to save".into());
            return;
        };
        match media::save_frame(&frame, Path::new(name)) {
            Ok(()) => {
                if let Some(c) = self.panes[idx].compute.as_mut() {
                    c.saving = false;
                }
                self.set_compute_status(idx, format!("Saved {name}"));
            }
            Err(e) => self.set_compute_status(idx, format!("Save failed: {e}")),
        }
    }

    fn set_compute_status(&mut self, idx: usize, msg: String) {
        if let Some(c) = self.panes[idx].compute.as_mut() {
            c.status = msg;
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

    // ---- effective Transformations (own, or shared when `sync_tone`) ------

    pub(super) fn contrast_of(&self, i: usize) -> ContrastMode {
        if self.panes[i].sync_tone {
            self.shared_contrast
        } else {
            self.panes[i].contrast
        }
    }

    pub(super) fn tone_of(&self, i: usize) -> ToneOptions {
        if self.panes[i].sync_tone {
            self.shared_tone
        } else {
            self.panes[i].tone
        }
    }

    pub(super) fn details_of(&self, i: usize) -> bool {
        if self.panes[i].sync_tone {
            self.shared_details
        } else {
            self.panes[i].details
        }
    }

    pub(super) fn overlay_of(&self, i: usize) -> Option<OverlaySpec> {
        if self.panes[i].sync_tone {
            self.shared_overlay
        } else {
            self.panes[i].overlay
        }
    }

    /// Invalidate the textures of every tone-synced pane (after the shared
    /// Transformations change), so they re-render with the new mapping — base
    /// image and tinted overlay both.
    pub(super) fn invalidate_synced_tone(&mut self) {
        for p in &mut self.panes {
            if p.sync_tone {
                p.tex = None;
                p.overlay_tex = None;
            }
        }
    }

    /// Set a pane's tone-sync flag. Turning it **off** snapshots the shared
    /// Transformations (tone + overlay) into the pane so nothing jumps; either
    /// way the pane re-renders.
    pub(super) fn set_sync_tone(&mut self, i: usize, on: bool) {
        if self.panes[i].sync_tone == on {
            return;
        }
        if !on {
            self.panes[i].contrast = self.shared_contrast;
            self.panes[i].tone = self.shared_tone;
            self.panes[i].details = self.shared_details;
            self.panes[i].overlay = self.shared_overlay;
        }
        self.panes[i].sync_tone = on;
        self.panes[i].tex = None;
        self.panes[i].overlay_tex = None;
    }

    /// Length of the shared timeline: the **control** media drives the loop.
    /// Other synced sequences clamp/hold against this length.
    pub(super) fn timeline_len(&self) -> usize {
        self.panes
            .get(self.control)
            .map(|p| p.media.frame_count())
            .unwrap_or(1)
            .max(1)
    }

    /// Whether the timeline-driving media's true end is known. Until it is, the
    /// timeline holds at the last discovered frame rather than wrapping early.
    pub(super) fn current_at_end(&self) -> bool {
        self.panes
            .get(self.control)
            .is_none_or(|p| p.media.at_end())
    }

    /// Any loaded media has more than one (discovered) frame — i.e. there is a
    /// sequence to play, so the transport bar should be shown.
    pub(super) fn any_sequence(&self) -> bool {
        self.panes.iter().any(|p| p.media.frame_count() > 1)
    }

    /// The inclusive `[lo, hi]` frame window playback loops over, clamped to the
    /// current known length `len`. `loop_range == None` → the whole sequence.
    pub(super) fn loop_bounds(&self, len: usize) -> (usize, usize) {
        let last = len.saturating_sub(1);
        match self.loop_range {
            Some((lo, hi)) => {
                let hi = hi.min(last);
                (lo.min(hi), hi)
            }
            None => (0, last),
        }
    }

    /// Keep `control` pointing at a sequence: clamp it in range, and if it isn't
    /// a multi-frame media, repoint to the first one that is (leaving a valid
    /// user choice untouched).
    pub(super) fn ensure_control(&mut self) {
        if self.panes.is_empty() {
            self.control = 0;
            return;
        }
        let before = self.control;
        self.control = self.control.min(self.panes.len() - 1);
        let is_seq = |p: &Pane| p.media.frame_count() > 1;
        if !self.panes.get(self.control).is_some_and(|p| is_seq(p)) {
            if let Some(i) = self.panes.iter().position(is_seq) {
                self.control = i;
            }
        }
        // A loop sub-range belongs to a specific sequence; drop it if control
        // moved to a different one.
        if self.control != before {
            self.loop_range = None;
        }
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

    /// The resident-frame memory ceiling in bytes, from the configured budget
    /// (at least 1 MiB so eviction always has a target below the total).
    pub(super) fn cache_budget_bytes(&self) -> usize {
        self.config.cache_budget_mb.max(1) * 1024 * 1024
    }

    // ---- statistics region ----------------------------------------------

    /// Set (or clear) the shared image-space stats region. Bumps `stats_gen` so
    /// cached stats recompute; clearing also drops region-tone off every pane.
    /// Region-tone textures are invalidated so their bounds re-derive.
    pub(super) fn set_stats_region(&mut self, reg: Option<Rect>) {
        self.stats_region = reg;
        self.stats_gen = self.stats_gen.wrapping_add(1);
        for p in &mut self.panes {
            p.stats = None;
            if reg.is_none() && p.region_tone {
                p.region_tone = false;
                p.tex = None;
            } else if reg.is_some() && p.region_tone {
                p.tex = None; // bounds change with the new region
            }
        }
    }

    /// Turn region-driven tone on/off for every pane at once (the button is a
    /// single control replicated across panes), invalidating their textures.
    pub(super) fn apply_region_tone(&mut self, on: bool) {
        for p in &mut self.panes {
            if p.region_tone != on {
                p.region_tone = on;
                p.tex = None;
            }
        }
    }

    /// Ensure pane `idx` has current statistics for the shared region and its
    /// displayed frame, recomputing only when the frame or region changed.
    pub(super) fn ensure_region_stats(&mut self, idx: usize) {
        let Some(reg) = self.stats_region else {
            self.panes[idx].stats = None;
            return;
        };
        let f = self.frame_disp(idx);
        let key = (f, self.stats_gen);
        if self.panes[idx].stats.as_ref().map(|s| s.key) == Some(key) {
            return;
        }
        let Some(frame) = self.panes[idx].media.resident(f) else {
            return; // not decoded yet; keep any previous stats until it lands
        };
        let Some((x0, y0, x1, y1)) = pixel_bounds(reg, frame.size) else {
            self.panes[idx].stats = None;
            return;
        };
        let data = frame.region_stats(x0, y0, x1, y1, 256);
        self.panes[idx].stats = Some(RegionStatsCache { key, data });
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
        self.drive_seek();

        // Discover sequence length lazily: eager "Load all" batches drive to the
        // end, otherwise just keep one page ahead of the cursor.
        self.drive_eager();
        self.ensure_lookahead();
        self.poll_decoding_all();
        self.enforce_cache_budget();

        // Keep `control` on a sequence, then clamp the shared timeline to it.
        self.ensure_control();
        let tl = self.timeline_len();
        if self.shared_frame >= tl {
            self.shared_frame = tl - 1;
        }

        // Auto-refresh Compute panes whose inputs advanced (e.g. during playback).
        self.refresh_auto_compute();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(2.0);
            self.draw_toolbar(ui);
            ui.add_space(2.0);
        });

        // Full-width transport bar, pinned to the bottom. Shown whenever any
        // loaded media is a sequence (not just the focused one), so selecting a
        // still doesn't drop the bar and shift the whole layout. It follows the
        // `control` sequence.
        if self.any_sequence() {
            egui::TopBottomPanel::bottom("framebar").show(ctx, |ui| {
                ui.add_space(4.0);
                self.draw_frame_bar(ui);
                ui.add_space(4.0);
            });
        }

        // No frame margin: the image area runs flush to the window edges
        // (top under the toolbar, left and right).
        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                self.draw_central(ui, ctx);
            });

        // The floating "new compute" config panel, if one is being set up.
        self.draw_compute_draft(ctx);

        if self.show_manager {
            self.draw_manager(ctx);
        }
        if self.show_export {
            self.draw_export(ctx);
        }
        if self.show_settings {
            self.draw_settings(ctx);
        }
        if self.show_viewcmd {
            self.draw_viewcmd(ctx);
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
        if std::mem::take(&mut self.pending_compute_create) {
            if let Some(draft) = self.compute_draft.take() {
                self.open_compute(draft);
            }
        }

        // Encode one frame per frame while an export is running.
        if self.export_run.is_some() || self.cancel_export {
            self.export_tick();
        }

        // Keep animating, but pace repaints to what's actually happening rather
        // than busy-spinning at monitor rate (pure waste over VNC / no-GPU).
        // Playback needs its own frame interval; a pending background decode or a
        // running export (which encodes on a worker thread — we just poll its
        // progress) only needs an occasional wake-up. Idle with nothing pending:
        // no repaint is requested at all.
        if self.playing {
            let dt = (1.0 / self.fps.max(1.0)).clamp(1.0 / 120.0, 0.1);
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(dt));
        } else if self.export_run.is_some() || self.cancel_export || !self.inflight.is_empty() {
            ctx.request_repaint_after(DECODE_POLL);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.save();
    }
}

// ---- shared free helpers -------------------------------------------------

/// Header rows for a cell of `width`: **two** (Compute stacks under
/// Transformations) when it's too narrow to fit both buttons plus a little
/// title on one line, else **one**.
fn header_rows(width: f32) -> f32 {
    if width < MODIFY_W + COMPUTE_W + 44.0 {
        2.0
    } else {
        1.0
    }
}

/// Total header height for a cell of `width` (one or two `HEADER_H` rows).
fn header_h_for(width: f32) -> f32 {
    header_rows(width) * HEADER_H
}

/// The image sub-rect of a cell, between its header (one or two rows) and footer.
fn image_area(cell: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(cell.min.x, cell.min.y + header_h_for(cell.width()) + 2.0),
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

/// Lerp `out` toward `base` per byte: `out = base*(1-t) + out*t`. Used to blend
/// a tone operator's RGBA result back toward the plain linear image.
fn blend_rgba(out: &mut [u8], base: &[u8], t: f32) {
    let t = t.clamp(0.0, 1.0);
    for (o, &b) in out.iter_mut().zip(base) {
        *o = (b as f32 * (1.0 - t) + *o as f32 * t).round().clamp(0.0, 255.0) as u8;
    }
}

/// Clamp an image-space region to a frame's pixel grid, returning the integer
/// half-open bounds `[x0, x1) × [y0, y1)`, or `None` if it doesn't cover at
/// least one pixel (e.g. the region lies entirely outside this frame — pages
/// can differ in size).
fn pixel_bounds(reg: Rect, size: [usize; 2]) -> Option<(usize, usize, usize, usize)> {
    let (w, h) = (size[0], size[1]);
    let x0 = (reg.min.x.floor().max(0.0) as usize).min(w);
    let y0 = (reg.min.y.floor().max(0.0) as usize).min(h);
    let x1 = (reg.max.x.ceil().max(0.0) as usize).min(w);
    let y1 = (reg.max.y.ceil().max(0.0) as usize).min(h);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// Render the tone options for `mode` as label/value rows inside a 2-column
/// `Grid` (each row ends with `end_row`). Extend by adding a `match` arm or a
/// row: each mode reads/writes its own sub-struct of `ToneOptions`, so options
/// never collide across modes.
fn draw_tone_options(
    ui: &mut egui::Ui,
    _pane_id: u64,
    mode: ContrastMode,
    tone: &mut ToneOptions,
) {
    match mode {
        ContrastMode::LinearClip => {
            ui.label("Clip %");
            ui.add(
                egui::DragValue::new(&mut tone.clip.percent)
                    .speed(0.005)
                    .range(0.0..=49.0)
                    .max_decimals(3),
            )
            .on_hover_text("Percentile clipped at each tail before the stretch");
            ui.end_row();
        }
        ContrastMode::LutAlpha => {
            ui.label("Blend");
            ui.add(egui::Slider::new(&mut tone.lut_alpha.blend, 0.0..=1.0).fixed_decimals(2))
                .on_hover_text("Mix from the linear image (0) to full LUT_ALPHA (1)");
            ui.end_row();
            // Add more LUT_ALPHA options here: one row + a field on LutAlphaOptions.
        }
        // Plain Linear has no options — emit no row (no wasted space).
        ContrastMode::Linear => {}
    }
}

/// Draw a region's per-channel histogram into `rect` (Visualise-panel style:
/// sqrt-scaled line curves over a dark base).
fn draw_region_hist(painter: &egui::Painter, rect: Rect, stats: &RegionStats) {
    painter.rect_filled(rect, 0.0, Color32::from_gray(16));
    let peak = stats
        .hist
        .bins
        .iter()
        .flat_map(|c| c.iter().copied())
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let colors: &[Color32] = if stats.hist.mono {
        &[Color32::from_gray(210)]
    } else {
        &[
            Color32::from_rgb(230, 90, 90),
            Color32::from_rgb(90, 210, 90),
            Color32::from_rgb(100, 140, 240),
        ]
    };
    for (ci, chan) in stats.hist.bins.iter().enumerate() {
        let nb = chan.len().max(2);
        let mut pts = Vec::with_capacity(nb);
        for (v, &count) in chan.iter().enumerate() {
            let x = rect.left() + (v as f32 / (nb - 1) as f32) * rect.width();
            let hh = (count as f32 / peak).sqrt();
            let y = rect.bottom() - hh * rect.height();
            pts.push(Pos2::new(x, y));
        }
        painter.add(egui::Shape::line(pts, Stroke::new(1.0, colors[ci])));
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

/// Update an index after moving the pane at `from` to index `to`
/// (`remove(from)` then `insert(to)`) — the reorder used by the Media manager.
fn remap_move(v: &mut usize, from: usize, to: usize) {
    if *v == from {
        *v = to;
    } else if from < *v && *v <= to {
        *v -= 1;
    } else if to <= *v && *v < from {
        *v += 1;
    }
}

/// The Media-manager row (its pane vec index) that a drop at screen-`y` targets:
/// the row directly under the cursor, else the nearest one (so drops in the gaps
/// or past either end still resolve).
fn drop_target(rows: &[(usize, egui::Rangef)], y: f32) -> Option<usize> {
    if let Some(&(idx, _)) = rows.iter().find(|(_, band)| band.contains(y)) {
        return Some(idx);
    }
    rows.iter()
        .min_by(|a, b| (a.1.center() - y).abs().total_cmp(&(b.1.center() - y).abs()))
        .map(|&(idx, _)| idx)
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

/// Render a path for a shell command line, double-quoting it when it contains
/// whitespace so the generated `cim …` command pastes back correctly.
fn quote_path(p: &Path) -> String {
    quote_arg(&p.display().to_string())
}

/// Double-quote a command-line argument when it contains whitespace.
fn quote_arg(s: &str) -> String {
    if s.chars().any(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
