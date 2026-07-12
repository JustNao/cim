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
mod compute;
mod lifecycle;
mod util;
mod watch;

use util::*;
mod decode;
mod export_ui;
mod input;
mod panels;
mod profile;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Id, Key, PointerButton, Pos2, Rect, Sense, Stroke,
    TextureHandle, TextureId, TextureOptions, Vec2,
};

use crate::cli;
use crate::decoder::{BackgroundDecoder, Decoded};
use crate::export::{self, Encoder, ExportLayout, ExportPane, ExportPlan, ExportSource, GridCell};
use crate::media::{self, HistData, Media, Reduce, RegionStats};
use crate::settings::{Action, Chord, Config, ContrastMode, ToneOptions};
use crate::view::ViewTransform;
use export_ui::ExportRun;

const HEADER_H: f32 = 24.0;
const FOOTER_H: f32 = 20.0;
const GAP: f32 = 0.0;
const HANDLE_HIT: f32 = 24.0; // px around the A/B divider that grabs it
const MODIFY_W: f32 = 108.0; // width of the header "Transformations" button

/// How often to repaint while background decodes are pending (and we're not
/// playing or exporting): often enough to pick up landed frames and keep the
/// loading spinner turning, but far below monitor rate so we don't busy-spin —
/// the dominant idle cost over VNC / software rendering. ~30 fps.
const DECODE_POLL: std::time::Duration = std::time::Duration::from_millis(33);

/// How long a transient status notification (top toolbar, far right) stays up
/// before it auto-clears.
const STATUS_TTL: f64 = 10.0;

/// How often a **watched** pane's source file(s) are stat-ed for changes (also
/// the idle wake-up interval while any pane is watching). A `stat` is
/// microseconds, so this is negligible next to a single decode; it's kept slow
/// on purpose to stay friendly to the paced-repaint model over VNC.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// A watched file must stay unchanged (same mtime + size) for this long after a
/// change before it's reloaded — a debounce so a file still being written
/// externally isn't read half-finished (each further write resets the timer).
const WATCH_DEBOUNCE: f64 = 0.4;

/// Identity of a source's on-disk contents for change detection: the latest
/// modification time across its file(s) and their total byte length.
type FileSig = (std::time::SystemTime, u64);

/// How many frames ahead of the shown one playback pre-decodes for each on-screen
/// pane (`prefetch_playback`), so it overlaps decode with display instead of
/// stalling on decode latency when it reaches a not-yet-resident frame.
const PLAY_PREFETCH: usize = 3;

/// Opening more sequences than this at once triggers a resource-warning
/// confirmation (heavy CPU / memory, worst over VNC on a shared machine).
const SEQ_WARN_LIMIT: usize = 8;

/// Outline / accent colour for the right-drag statistics region (cyan, so it
/// reads distinct from the amber export-region rectangle).
const REGION_COL: Color32 = Color32::from_rgb(90, 210, 230);

/// Colour of the editable intensity-profile line (shift + right-drag) and its
/// endpoint handles — amber, matching the request.
const LINE_COL: Color32 = Color32::from_rgb(255, 191, 0);

/// Screen-space grab radius (px) for the profile line's endpoints / body.
const LINE_HANDLE: f32 = 8.0;

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
    /// Tone signature the texture was rendered with (`CimApp::tone_sig`); with
    /// `shown` it tells a still-current texture from a stale one. The overlay
    /// texture doesn't tone-map, so it leaves this at 0.
    sig: u64,
    /// Nearest-decimation factor this texture was rendered at (`CimApp::want_step`
    /// / `stage_step`): 1 = full resolution, ≥2 = every N-th source pixel for a
    /// minified pane. Part of the texture's identity alongside `(shown, sig)` so a
    /// zoom change that alters it re-renders and re-commits. Heavy (proprietary
    /// operator) renders and overlays always use 1.
    step: usize,
}

/// A single-channel media from another pane (a boolean mask or a grayscale
/// image/sequence), tinted and drawn over a pane. Config only (no texture), so it
/// can be shared across tone-synced panes; the tinted texture is cached
/// separately per pane in `Pane.overlay_tex`. The source must match the target's
/// pixel size (enforced when selected, §9).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct OverlaySpec {
    src_id: u64, // stable id of the pane supplying the overlay
    color: Color32,
    opacity: f32, // 0..1
}

/// An editable line drawn over the images with **shift + right-drag**, stored in
/// IMAGE space (like `stats_region`) so it replicates on every pane and can be
/// moved from any of them. Each media's pixel intensities sampled along it are
/// plotted in the **Line profile** tab.
#[derive(Clone, Copy)]
pub(super) struct LineProfile {
    a: Pos2, // image-space endpoints
    b: Pos2,
}

/// Which part of the profile line a shift+right drag is manipulating.
#[derive(Clone, Copy, PartialEq)]
enum LineGrab {
    Start,     // dragging endpoint A
    End,       // dragging endpoint B
    Body,      // translating the whole line
    New(Pos2), // drawing a fresh line (A pinned at the given image-space anchor)
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
    /// A numbered still sequence: the compact `PREFIX%0Xu SUFFIX,START,END` token
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
    /// False while the pane is still being configured (the in-pane form is
    /// shown); set once a compute succeeds, after which the result image shows
    /// with the Refresh / Save / Auto-refresh controls top-left.
    computed: bool,
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

/// One opened media plus its per-pane view/timeline state.
struct Pane {
    id: u64, // stable across reorder/close; matches background-decode results
    source: Source, // how to reload it / re-emit it in a replay command
    media: Media,
    tex: Option<CachedTex>,
    /// Next frame's texture, rendered while the pane keeps displaying `tex`, so
    /// every on-screen pane can flip to the new frame **together** (see
    /// `refresh_textures`). Committed into `tex` by an atomic swap once all shown
    /// panes are ready — the swap keeps the old texture handle here for reuse, so
    /// playback doesn't allocate a texture per frame.
    pending: Option<CachedTex>,
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
    /// Display rotation in **degrees** (-180..180), about the image centre.
    /// Applied at draw time (the texture stays unrotated) and to the export;
    /// per-pane, independent of the Transformations sync.
    rotation: f32,
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
    /// Background bulk-load mode for this pane (frame-bar / export buttons).
    eager: Eager,
    /// Auto-reload: watch the source file(s) on disk and reload when they change
    /// (the ◉ toggle in the header, left of ⟳ Reload). Never set for a Compute
    /// pane (it has no file — use its own Auto-refresh).
    watch: bool,
    /// Signature of the currently-loaded on-disk contents, the baseline changes
    /// are measured against. `None` until the first successful stat establishes
    /// it (so enabling the watch never triggers an immediate reload).
    watch_loaded: Option<FileSig>,
    /// A changed-but-not-yet-settled signature and when it was first seen, for the
    /// `WATCH_DEBOUNCE` quiescence check; reset each time the signature changes
    /// again (i.e. while the file is still being written).
    watch_seen: Option<(FileSig, f64)>,
}

/// A pane's background bulk-load mode.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Eager {
    /// Not bulk-loading.
    #[default]
    Off,
    /// "Load all": decode every known frame and drive the frontier to the end.
    /// Downgraded to `Offsets` if the frame cache fills, so length discovery
    /// still finishes (headers alone) instead of stalling.
    Full,
    /// "Load offsets": drive the frontier to the end with **metadata-only**
    /// probes (discover the true length via headers), decoding no pixels — so
    /// the timeline reaches its end without filling the cache.
    Offsets,
}

pub struct CimApp {
    config: Config,
    /// The config as last written to disk. `config` is edited live; the two
    /// differ while there are unsaved Settings changes (surfaced as a warning),
    /// and the config is written only on an explicit **Save settings** — never
    /// on exit.
    saved_config: Config,
    panes: Vec<Pane>,
    next_id: u64,

    // Shared view/timeline that every synced pane follows.
    shared_view: ViewTransform,
    shared_frame: usize,
    /// During playback, the candidate next shared frame being pre-rendered while
    /// the panes still show `shared_frame`. The timeline only advances to it once
    /// **every** on-screen pane has that frame ready (`refresh_textures` commits
    /// the swap and applies it here), so the frame counter never runs ahead of the
    /// image and all panes flip in step. `None` when idle / paused / seeking.
    play_prefetch: Option<usize>,
    /// Shared "Transformations" (tone mode + options + details) that every pane
    /// with `sync_tone` follows, so editing one synced pane's Transformations
    /// popup updates them all.
    shared_contrast: ContrastMode,
    shared_tone: ToneOptions,
    shared_details: bool,
    /// Shared display rotation in degrees (rides the same `sync_tone`).
    shared_rotation: f32,
    /// Shared mask overlay (rides the same `sync_tone` as the tone).
    shared_overlay: Option<OverlaySpec>,
    /// A requested timeline frame not yet reachable because the sequence's
    /// length is still being discovered (e.g. from `--frame` at launch). While
    /// set, discovery is driven forward until this frame exists, then the
    /// timeline jumps to it. Cleared by any manual frame navigation.
    pending_seek: Option<usize>,
    /// Edit buffer for the typeable frame-index field in the frame bar. Mirrors
    /// `shared_frame` unless the field currently has focus (the user is typing a
    /// target), so a jump can be committed on Enter without stepping through the
    /// intervening frames.
    frame_edit: String,

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
    /// Fast-forward stride (≥1, default 1): decode only 1 of every `fast_forward`
    /// frames; the `fast_forward - 1` between are skimmed by a metadata-only header
    /// probe (never decoded), to skim a huge sequence quickly. Affects **both**
    /// "Load all" (`drive_eager`) and **playback** (`advance_playback` steps by
    /// `fast_forward`; `prefetch_playback` / `ensure_lookahead` skim to match).
    /// `1` = decode every frame (no skimming).
    fast_forward: usize,

    show_settings: bool,
    show_manager: bool,
    /// The "View command" window: shows a `cim …` line that reopens the current
    /// files at the current view, for copying / sharing.
    show_viewcmd: bool,
    rebinding: Option<Action>,
    /// The toolbar "Compute" button was clicked: add a new Compute pane after the
    /// draw (deferred to avoid growing `panes` mid-draw).
    pending_compute_create: bool,
    /// A Compute pane's in-pane **Compute** / **Refresh** button was clicked.
    /// Deferred so the recompute (which nulls the pane's texture — its frame data
    /// changed but its `(frame, sig)` identity didn't) runs at the *top* of the
    /// next update, before `refresh_textures`, so the fresh result re-renders and
    /// commits in the same lock-step group as the other panes — no black flash.
    pending_recompute: Option<usize>,

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

    // ---- intensity-profile line (shift + right-drag) --------------------
    /// The editable profile line in IMAGE space, replicated across every pane;
    /// `None` until one is drawn.
    line_profile: Option<LineProfile>,
    /// In-progress shift+right drag: which part is moving, the pane it started
    /// on, that pane's coordinate area (for screen↔image mapping), and the last
    /// image-space pointer (used to translate the body by its delta).
    line_grab: Option<LineGrab>,
    line_grab_pane: Option<usize>,
    line_grab_area: Rect,
    line_drag_last: Option<Pos2>,

    /// Edit buffer for the Transformations popup's typeable rotation angle, and
    /// the pane id currently being edited (so the buffer isn't overwritten with
    /// the live value mid-typing). Mirrors the frame-bar `frame_edit` pattern.
    rotation_edit: String,
    rotation_edit_pane: Option<u64>,

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
    /// Transient notification shown top-right in the toolbar (e.g. "Settings
    /// saved"). Any assignment to it auto-expires after `STATUS_TTL`: `update`
    /// shadows the last value in `status_shadow` to detect a fresh message and
    /// stamps `status_at`, so every current/future `self.status = …` gets the
    /// timeout for free.
    status: String,
    status_shadow: String,
    status_at: f64,
    /// Global error not tied to a sequence — rendered as a modal popup.
    error_popup: Option<String>,
    last_area: Rect,
    /// The hovered pane's cursor position in **image space**, recomputed each
    /// frame in `draw_central`. Replicated across every pane (a red dot + the
    /// per-pane pixel value) so the same source pixel can be read everywhere.
    cursor_img: Option<Vec2>,
    /// The pane the cursor is over (the source of `cursor_img`). The red dot is
    /// **not** drawn on it — its own OS cursor already marks the spot.
    cursor_pane: Option<usize>,
    drag_src: Option<usize>,
    /// Alt-drag rotation in progress: (pane idx, screen pivot = image centre,
    /// pointer angle at grab, pane rotation° at grab). Photoshop-style — the pane
    /// spins to follow the cursor around its centre.
    rotate_drag: Option<(usize, Pos2, f32, f32)>,
    /// Row being dragged to reorder in the ☰ Media manager (a pane vec index).
    manager_drag: Option<usize>,
    pending_remove: Option<usize>,
    pending_reload: Option<usize>,
    pending_reload_all: bool,
    /// Media loaded but not yet added as panes, held while the ">8 sequences"
    /// resource warning is up. Confirmed → `commit_open`; declined → quit.
    pending_open: Option<Vec<(Media, Source)>>,
    /// View state deferred alongside `pending_open` (the startup CLI path), so
    /// `--frame`/`--mode`/… still apply once the user confirms the open.
    pending_view: Option<cli::ViewState>,

    decoder: BackgroundDecoder,
    /// Auto decode-thread count (scaled to CPU cores, capped), used when
    /// `config.decode_threads == 0`. Computed once at startup so the per-frame
    /// resolve doesn't re-query the OS.
    auto_decode_threads: usize,
    /// Thread count the live `decoder` pool was built with. When the resolved
    /// setting changes, the pool is rebuilt to match (`update`).
    decode_threads_active: usize,
    /// The `cpp_lib_dir` value the operator libraries were last (auto-)loaded
    /// from. When the setting changes, `update` retries loading from the new
    /// folder (`load_cpp_libs`) so a corrected path applies without a restart.
    cpp_dir_active: String,
    inflight: HashSet<(u64, usize)>,
    /// Off-thread tone renderer for panes using the heavy operators (LUT_ALPHA /
    /// details); `render_inflight` holds the pane ids with a render in flight so
    /// at most one runs per pane at a time (rapid tone/frame changes coalesce).
    renderer: crate::renderer::RenderPool,
    render_inflight: HashSet<u64>,
    /// Pipeline timing profiler and its window toggle — only populated / shown
    /// when launched with `CIM_DEBUG=1` (see `crate::debug`).
    metrics: crate::debug::Metrics,
    show_debug: bool,
    /// True while a "Load all" / "Load offsets" batch is still running, so the
    /// status line can be cleared once every queued frame/probe has landed.
    decoding_all: bool,
    /// Set when a running "Load all" hit the frame-cache budget and had to fall
    /// back to offsets-only (headers) for the remaining frames — so not the whole
    /// sequence is resident in memory.
    load_cache_exhausted: bool,
    /// A "Load all" was started from the **export** panel: on completion, if the
    /// cache was too small (`load_cache_exhausted`), warn with a modal.
    export_load_pending: bool,
    /// A non-error modal notice (e.g. the export cache-too-small warning).
    warn_popup: Option<String>,
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
        // No drop shadows under windows / popups (e.g. the Compute pane form).
        style.visuals.window_shadow = egui::epaint::Shadow::NONE;
        style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
        cc.egui_ctx.set_style(style);

        // Embed a small font (a subset of DejaVu Sans covering the Braille block)
        // so glyphs the bundled fonts lack — notably the ⠿ drag-handle grip —
        // render instead of showing tofu. Appended as a *fallback*, so the
        // default proportional/monospace faces are still preferred.
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cimicons".to_owned(),
            egui::FontData::from_static(include_bytes!("../../assets/cimicons.ttf")),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cimicons".to_owned());
        }
        cc.egui_ctx.set_fonts(fonts);

        let auto_decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 6))
            .unwrap_or(4);

        let config = Config::load();
        // 0 = auto (scale with cores); an explicit setting caps it for shared hosts.
        let threads = if config.decode_threads == 0 {
            auto_decode_threads
        } else {
            config.decode_threads.clamp(1, 16)
        };
        // Load the optional proprietary operator libraries from the configured
        // folder (or, when unset, by their hard-coded names via LD_LIBRARY_PATH).
        // Each operator is independent; a missing library just leaves its feature
        // disabled and never blocks startup.
        let cpp_dir = cpp_lib_dir(&config);
        crate::imageproc::init(cpp_dir.as_deref());
        let cpp_dir_active = config.cpp_lib_dir.clone();
        let mut app = Self {
            saved_config: config.clone(),
            config,
            panes: Vec::new(),
            next_id: 0,
            shared_view: ViewTransform::default(),
            shared_frame: 0,
            play_prefetch: None,
            shared_contrast: ContrastMode::Linear,
            shared_tone: ToneOptions::default(),
            shared_details: false,
            shared_rotation: 0.0,
            shared_overlay: None,
            pending_seek: None,
            frame_edit: String::new(),
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
            fps: 25.0,
            play_accum: 0.0,
            fast_forward: 1,
            show_settings: false,
            show_manager: false,
            show_viewcmd: false,
            rebinding: None,
            pending_compute_create: false,
            pending_recompute: None,
            show_stats: true,
            stats_region: None,
            stats_gen: 0,
            stats_sel_start: None,
            stats_sel_now: None,
            stats_sel_pane: None,
            stats_sel_area: Rect::NOTHING,

            line_profile: None,
            line_grab: None,
            line_grab_pane: None,
            line_grab_area: Rect::NOTHING,
            line_drag_last: None,
            rotation_edit: String::new(),
            rotation_edit_pane: None,

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
            status_shadow: String::new(),
            status_at: 0.0,
            error_popup: None,
            last_area: Rect::NOTHING,
            cursor_img: None,
            cursor_pane: None,
            drag_src: None,
            rotate_drag: None,
            manager_drag: None,
            pending_remove: None,
            pending_reload: None,
            pending_reload_all: false,
            pending_open: None,
            pending_view: None,
            decoder: BackgroundDecoder::new(threads),
            auto_decode_threads,
            decode_threads_active: threads,
            cpp_dir_active,
            inflight: HashSet::new(),
            // One render worker: serialises the proprietary operators (whose
            // thread-safety we can't assume) while still keeping all of that work
            // off the UI thread. Raise this once LUT_ALPHA / DETAILS_ENHANCED are
            // known to be reentrant, to render several panes in parallel.
            renderer: crate::renderer::RenderPool::new(),
            render_inflight: HashSet::new(),
            metrics: crate::debug::Metrics::default(),
            show_debug: false,
            decoding_all: false,
            load_cache_exhausted: false,
            export_load_pending: false,
            warn_popup: None,
            clock: 0,
            render_scratch: Vec::new(),
        };
        app.open_inputs(inputs);
        if app.pending_open.is_some() {
            // The open is held behind the ">8 sequences" warning; apply the view
            // once the user confirms and the panes actually exist.
            app.pending_view = Some(view);
        } else {
            app.apply_view_state(view);
        }
        app
    }
    /// Resolve the effective background decode-thread count: the configured value
    /// (clamped) or, when it's `0`, the auto count scaled to CPU cores. Read each
    /// update so a Settings change rebuilds the pool.
    pub(super) fn resolve_decode_threads(&self) -> usize {
        if self.config.decode_threads == 0 {
            self.auto_decode_threads
        } else {
            self.config.decode_threads.clamp(1, 16)
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

    /// A pane whose target frame lies **beyond everything it has discovered** — a
    /// sequence loaded (or synced) behind an already-advanced timeline, so its
    /// target sits many pages past its frontier. Such a pane must discover
    /// forward before it can show the target; until then it **holds its last
    /// committed frame** and discovers with a metadata-only probe, rather than
    /// full-decoding and flipping through every page in between.
    ///
    /// Deliberately narrow so it never touches normal use: only while **paused**
    /// (playback discovers frame-by-frame at the frontier), only for a
    /// still-discovering sequence, and only when the target is at or past the
    /// frontier. During single-stepping the shared frame is clamped to the
    /// control length (`update`), so the control pane is never "catching up" —
    /// this is for the *other*, shorter/newer synced panes.
    pub(super) fn catching_up(&self, i: usize) -> bool {
        // A decode/probe error stops discovery (the pane shows its error) — don't
        // keep re-probing (which would also busy-spin the immediate repaint).
        if self.playing || self.panes[i].media.at_end() || self.panes[i].error.is_some() {
            return false;
        }
        let want = if self.panes[i].sync_temporal {
            self.shared_frame
        } else {
            self.panes[i].frame
        };
        want >= self.panes[i].media.frame_count()
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

    /// Effective display rotation (degrees) — the shared angle when tone-synced,
    /// else the pane's own.
    pub(super) fn rotation_of(&self, i: usize) -> f32 {
        if self.panes[i].sync_tone {
            self.shared_rotation
        } else {
            self.panes[i].rotation
        }
    }

    /// Set pane `i`'s effective rotation (degrees): writes the shared angle when
    /// tone-synced (so every synced pane turns together), else the pane's own.
    pub(super) fn set_rotation(&mut self, i: usize, deg: f32) {
        if self.panes[i].sync_tone {
            self.shared_rotation = deg;
        } else {
            self.panes[i].rotation = deg;
        }
    }

    /// Whether pane `i`'s currently shown frame is single-channel 16-bit — the
    /// only input the proprietary operators accept. Used (with
    /// `imageproc::lut_alpha_available` / `details_available`) to gate the
    /// LUT_ALPHA mode and the Details toggle in the popup. A not-yet-resident
    /// frame reads as unsupported until it loads.
    pub(super) fn pane_is_op_input(&self, i: usize) -> bool {
        let f = self.frame_disp(i);
        self.panes[i]
            .media
            .resident(f)
            .map(|fr| fr.is_op_input())
            .unwrap_or(false)
    }

    pub(super) fn overlay_of(&self, i: usize) -> Option<OverlaySpec> {
        if self.panes[i].sync_tone {
            self.shared_overlay
        } else {
            self.panes[i].overlay
        }
    }

    /// Load any not-yet-loaded proprietary operator library from the configured
    /// folder, without a restart; when that makes one newly available, re-render
    /// every pane so it takes effect and note it in the toolbar. Safe at runtime:
    /// `imageproc::load_missing` never *unloads* a library, so it can't dangle the
    /// function pointers held by live render/export instances — panes that had the
    /// operator disabled simply built no instances, and re-rendering now creates
    /// fresh ones from the new library. A no-op when nothing new loads, so it's
    /// cheap to call on every folder change. (Repointing an already-loaded operator
    /// at a different folder still needs a restart.)
    pub(super) fn load_cpp_libs(&mut self) {
        let before = (
            crate::imageproc::lut_alpha_available(),
            crate::imageproc::details_available(),
        );
        let dir = cpp_lib_dir(&self.config);
        let after = crate::imageproc::load_missing(dir.as_deref());
        if after == before {
            return; // nothing new loaded — don't thrash re-renders
        }
        for p in &mut self.panes {
            p.tex = None;
            p.pending = None;
            p.overlay_tex = None;
        }
        self.status = match after {
            (true, true) => "Operator libraries loaded".into(),
            (true, false) => "LUT_ALPHA operator loaded".into(),
            (false, true) => "Details operator loaded".into(),
            (false, false) => return,
        };
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
            self.panes[i].rotation = self.shared_rotation;
            self.panes[i].overlay = self.shared_overlay;
        }
        self.panes[i].sync_tone = on;
        // The pane re-renders via `tone_sig` (its effective tone changed) while
        // holding its last committed `tex`; nulling it would flash black for a
        // heavy LUT_ALPHA/details render. Only the tinted overlay is dropped.
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

    /// Jump the shared timeline to `target`. Within the discovered length this is
    /// instant. Past the frontier of a still-discovering sequence it arms a
    /// `pending_seek` so `drive_seek` rides the frontier as fast as it can — with
    /// the panes frozen (see `refresh_textures`) so none of the intervening frames
    /// are ever rendered — then snaps to `target`.
    pub(super) fn seek_to(&mut self, target: usize) {
        if self.panes.is_empty() {
            return;
        }
        self.play_prefetch = None; // a jump abandons any in-flight playback step
        let len = self.timeline_len();
        if target < len {
            self.pending_seek = None;
            self.shared_frame = target;
        } else if self.current_at_end() {
            // Past a fully-discovered end: clamp to the last real frame.
            self.pending_seek = None;
            self.shared_frame = len.saturating_sub(1);
        } else {
            // Beyond the frontier: discover forward without drawing each step.
            self.playing = false;
            self.pending_seek = Some(target);
        }
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

    /// Pixel size of pane `i` if it can serve as an overlay source — i.e. its
    /// current frame is **single-channel** (a boolean mask or a grayscale image /
    /// sequence). `None` if the frame isn't resident yet or has multiple channels.
    pub(super) fn overlay_source_size(&self, i: usize) -> Option<[usize; 2]> {
        let fr = self.panes[i].media.resident(self.frame_disp(i))?;
        (fr.channels == 1).then_some(fr.size)
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

    /// The visible pane nearest `from` by index distance (lower index wins a
    /// tie), or `None` when every pane is hidden.
    fn nearest_visible(&self, from: usize) -> Option<usize> {
        (0..self.panes.len())
            .filter(|&i| self.panes[i].visible)
            .min_by_key(|&i| (from.abs_diff(i), i))
    }

    /// If the focused pane (`current`) is hidden, move focus to the nearest
    /// still-shown media. Called right after a pane is hidden so the selection
    /// doesn't stay on a media that's no longer on screen; a no-op when the
    /// current pane is still visible or nothing is visible.
    pub(super) fn reselect_if_hidden(&mut self) {
        if self.panes.get(self.current).is_some_and(|p| p.visible) {
            return;
        }
        if let Some(i) = self.nearest_visible(self.current) {
            self.current = i;
        }
    }

    /// Panes whose image is actually on screen right now, given the current mode:
    /// Single shows only `current`, Grid the visible cells, A/B the two slots.
    /// Used to gate background decode (lookahead) so loaded-but-hidden sequences
    /// don't keep decoding and starving the pane the user is looking at.
    pub(super) fn displayed_indices(&self) -> Vec<usize> {
        if self.panes.is_empty() {
            return Vec::new();
        }
        let n = self.panes.len();
        match self.mode {
            Mode::Single => vec![self.current.min(n - 1)],
            Mode::Grid => self.visible_indices(),
            Mode::Ab => {
                let a = self.slot_a.min(n - 1);
                let b = self.slot_b.min(n - 1);
                if a == b {
                    vec![a]
                } else {
                    vec![a, b]
                }
            }
        }
    }

    /// The resident-frame memory ceiling in bytes, from the configured budget
    /// (at least 1 MiB so eviction always has a target below the total).
    pub(super) fn cache_budget_bytes(&self) -> usize {
        self.config.cache_budget_mb.max(1) * 1024 * 1024
    }

    // ---- statistics region ----------------------------------------------

    /// Set (or clear) the shared image-space stats region. Bumps `stats_gen` so
    /// cached stats recompute; clearing also drops region-tone off every pane.
    /// A region-tone pane re-derives its bounds via `tone_sig` (which folds in
    /// `stats_gen`), so no texture is nulled.
    pub(super) fn set_stats_region(&mut self, reg: Option<Rect>) {
        self.stats_region = reg;
        self.stats_gen = self.stats_gen.wrapping_add(1);
        for p in &mut self.panes {
            p.stats = None;
            if reg.is_none() && p.region_tone {
                p.region_tone = false;
            }
            // A region-tone pane re-renders on its own: `stats_gen` (and the
            // region_tone flag) feed `tone_sig`, so `stage` re-derives the bounds
            // and commits while the pane holds its last committed frame — no black.
        }
    }

    /// Turn region-driven tone on/off for every pane at once (the button is a
    /// single control replicated across panes); each re-renders via `tone_sig`.
    pub(super) fn apply_region_tone(&mut self, on: bool) {
        for p in &mut self.panes {
            if p.region_tone != on {
                p.region_tone = on;
                // Re-renders via `tone_sig` (region_tone changed) while holding
                // the last committed frame — nulling `tex` would flash black.
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
        // Whole-update CPU cost (CIM_DEBUG profiler); recorded at the end.
        let frame_start = crate::debug::enabled().then(std::time::Instant::now);

        // Global UI scale (buttons/text).
        let scale = self.config.ui_scale.clamp(0.5, 3.0);
        if (ctx.zoom_factor() - scale).abs() > 1e-3 {
            ctx.set_zoom_factor(scale);
        }

        self.clock = self.clock.wrapping_add(1);

        // Linux (esp. Wayland) frequently ignores `with_maximized(true)` from the
        // `ViewportBuilder` at window creation, so the window opens at the
        // restored size. Re-assert it once the window actually exists (first
        // frame) — Windows already honoured the builder, and re-sending is a
        // no-op there.
        if self.clock == 1 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }

        // Rebuild the decode pool if the thread setting changed (live-applied like
        // the other config). Orphaned in-flight jobs won't land on the new pool,
        // so clear `inflight` to let them be re-requested; the old pool's
        // persistent readers are dropped and reopen on demand.
        let want_threads = self.resolve_decode_threads();
        if want_threads != self.decode_threads_active {
            self.decoder = BackgroundDecoder::new(want_threads);
            self.inflight.clear();
            self.decode_threads_active = want_threads;
        }

        // Auto-load the proprietary operator libraries when the configured folder
        // changes (edited/pasted/Browsed in Settings), so a corrected path applies
        // without a restart. `load_cpp_libs` only *adds* a not-yet-loaded library
        // (never unloads one — safe against live render/export instances) and no-ops
        // when nothing new loads, so retrying per distinct path value is harmless.
        if self.config.cpp_lib_dir != self.cpp_dir_active {
            self.cpp_dir_active = self.config.cpp_lib_dir.clone();
            self.load_cpp_libs();
        }

        self.pump_decoder();
        self.pump_render(ctx);
        self.handle_input(ctx);
        self.advance_playback(ctx);
        self.drive_seek();

        // Discover sequence length lazily: eager "Load all" batches drive to the
        // end, otherwise just keep one page ahead of the cursor.
        self.drive_eager();
        self.ensure_lookahead();
        self.prefetch_playback();
        self.poll_decoding_all();
        self.enforce_cache_budget();

        // Keep `control` on a sequence, then clamp the shared timeline to it.
        self.ensure_control();
        let tl = self.timeline_len();
        if self.shared_frame >= tl {
            self.shared_frame = tl - 1;
        }
        // A pre-render target can't outrun the (possibly just-clamped) length.
        if self.play_prefetch.is_some_and(|f| f >= tl) {
            self.play_prefetch = None;
        }

        // Auto-reload watched panes whose source files changed and settled. Runs
        // before `refresh_textures` (like the compute recompute) so a reloaded
        // frame re-renders and commits in step rather than flashing.
        self.poll_watches(ctx.input(|i| i.time));

        // Recompute Compute panes *before* staging textures: a compute button
        // click (deferred to here) and any auto-refresh both null the pane's
        // texture (its frame data changed), so doing it now lets the fresh result
        // re-render and commit in the same lock-step group as the other panes
        // below — the pane is never drawn black between the two.
        if let Some(i) = self.pending_recompute.take() {
            if i < self.panes.len() {
                self.recompute_pane(i);
            }
        }
        // Auto-refresh Compute panes whose inputs advanced (e.g. during playback).
        self.refresh_auto_compute();

        // Stage the on-screen panes' textures and, when they're all ready, flip
        // them (and commit a playback step) together. Runs last so it sees the
        // settled frame/tone state, just before drawing reads the textures.
        self.refresh_textures(ctx);

        // Expire a transient status notification after `STATUS_TTL`. Shadowing
        // the last value detects a fresh message, so every `self.status = …`
        // site (current and future) inherits the timeout without extra work.
        let now = ctx.input(|i| i.time);
        if self.status != self.status_shadow {
            self.status_shadow = self.status.clone();
            self.status_at = now;
        }
        if !self.status.is_empty() {
            let remaining = STATUS_TTL - (now - self.status_at);
            if remaining <= 0.0 {
                self.status.clear();
                self.status_shadow.clear();
            } else {
                // Wake up to clear it even when the app is otherwise idle.
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(remaining));
            }
        }

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

        if self.show_manager {
            self.draw_manager(ctx);
        }
        // The Line profile tab shows only while a line exists; drawing one opens
        // it, clearing it (or "Clear line") closes it.
        if self.line_profile.is_some() {
            self.draw_profile(ctx);
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
        if self.show_debug {
            self.draw_debug(ctx);
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

        if self.warn_popup.is_some() {
            let msg = self.warn_popup.clone().unwrap();
            let mut dismiss = false;
            egui::Window::new("⚠ Warning")
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
                self.warn_popup = None;
            }
        }

        // Resource warning before opening more than `SEQ_WARN_LIMIT` sequences.
        // The media are already loaded (cheaply) and held in `pending_open`;
        // confirming adds them, declining quits the app.
        if self.pending_open.is_some() {
            let existing = self.panes.iter().filter(|p| p.media.is_sequence()).count();
            let opening = self
                .pending_open
                .as_ref()
                .map(|b| b.iter().filter(|(m, _)| m.is_sequence()).count())
                .unwrap_or(0);
            let total = existing + opening;
            let mut decision: Option<bool> = None; // Some(true)=open, Some(false)=quit
            egui::Window::new("⚠ Many sequences")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "This will open {total} sequences at once.\n\nDecoding and \
                         playing many sequences in parallel is heavy on CPU and memory \
                         and can degrade performance — especially over a remote (VNC) \
                         session on a machine shared with other users."
                    ));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Open anyway").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("Quit").clicked() {
                            decision = Some(false);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    if let Some(batch) = self.pending_open.take() {
                        self.commit_open(batch);
                    }
                }
                Some(false) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                None => {}
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
            self.add_compute_pane();
        }

        // Encode one frame per frame while an export is running.
        if self.export_run.is_some() || self.cancel_export {
            self.export_tick();
        }

        if let Some(t) = frame_start {
            self.metrics.frame.record(t.elapsed());
            // A debug window with live timings should refresh even when idle.
            if self.show_debug {
                ctx.request_repaint_after(DECODE_POLL);
            }
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
        } else if self.pending_seek.is_some() || (0..self.panes.len()).any(|i| self.catching_up(i)) {
            // Actively riding the frontier (a length-discovery seek, or a pane
            // catching up to an advanced timeline): each probe grows the length
            // by one, so repaint immediately — discovery runs as fast as probes
            // land instead of one per 30 fps decode-poll tick.
            ctx.request_repaint();
        } else if self.export_run.is_some()
            || self.cancel_export
            || !self.inflight.is_empty()
            || !self.render_inflight.is_empty()
        {
            ctx.request_repaint_after(DECODE_POLL);
        } else if self.panes.iter().any(|p| p.watch) {
            // Nothing else pending, but a pane is watching a file: wake up
            // occasionally to stat it (see `poll_watches`). Slow enough to be
            // negligible over VNC.
            ctx.request_repaint_after(WATCH_POLL);
        }
    }
}

// ---- shared free helpers -------------------------------------------------

/// Total header height for a cell. A single row now that the header holds only
/// the Transformations button (Compute moved to the toolbar).
fn header_h_for(_width: f32) -> f32 {
    HEADER_H
}

/// The image sub-rect of a cell, flush between its header and footer bars (no
/// margin, so nothing shows through between the image and those strips).
fn image_area(cell: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(cell.min.x, cell.min.y + header_h_for(cell.width())),
        Pos2::new(cell.max.x, cell.max.y - FOOTER_H),
    )
}

/// The footer strip at the bottom of a cell.
fn footer_area(cell: Rect) -> Rect {
    Rect::from_min_max(Pos2::new(cell.min.x, cell.max.y - FOOTER_H), cell.max)
}

/// The image sub-rect of the A/B view: the whole `area` minus the shared footer
/// strip at the bottom (both wipe sides share this rect). Kept in one place so
/// the live drawing and the export composition can't drift on how much the
/// footer reserves.
fn ab_image_rect(area: Rect) -> Rect {
    Rect::from_min_max(area.min, Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0))
}

fn uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

/// The configured proprietary-operator library directory as a path. When the
/// setting is blank the default is a `LIBS` folder next to the cim executable
/// (`<cim location>/LIBS`); only if the executable path can't be resolved do we
/// fall back to `None` (bare-name `LD_LIBRARY_PATH` resolution).
pub(super) fn cpp_lib_dir(config: &Config) -> Option<PathBuf> {
    let dir = config.cpp_lib_dir.trim();
    if !dir.is_empty() {
        return Some(PathBuf::from(dir));
    }
    default_cpp_lib_dir()
}

/// Wrap a degree value into the (-180, 180] range used by the rotation control.
fn wrap180(mut d: f32) -> f32 {
    d %= 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d <= -180.0 {
        d += 360.0;
    }
    d
}

/// `<cim executable directory>/LIBS`, used when no library folder is configured.
fn default_cpp_lib_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("LIBS"))
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

/// Draw a histogram's per-channel curves into `rect` over a dark base: one grey
/// curve when mono, else R/G/B, each sqrt-scaled so the tails stay legible.
/// Shared by the pane Transformations histogram (`draw_histogram`) and the
/// region-stats mini histogram (`draw_stats_panel`).
///
/// For a mono histogram it also draws a full-height tick at the most-frequent
/// value (the peak / mode) and returns that value, so the caller can label it
/// under the graph next to min/max; returns `None` for multi-channel histograms.
fn draw_hist_curves(painter: &egui::Painter, rect: Rect, hist: &HistData) -> Option<f32> {
    painter.rect_filled(rect, 0.0, Color32::from_gray(16));
    let peak = hist
        .bins
        .iter()
        .flat_map(|c| c.iter().copied())
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let colors: &[Color32] = if hist.mono {
        &[Color32::from_gray(210)]
    } else {
        &[
            Color32::from_rgb(230, 90, 90),
            Color32::from_rgb(90, 210, 90),
            Color32::from_rgb(100, 140, 240),
        ]
    };
    for (ci, chan) in hist.bins.iter().enumerate() {
        let nb = chan.len().max(2);
        let mut pts = Vec::with_capacity(nb);
        for (v, &count) in chan.iter().enumerate() {
            // Skip empty bins: a zero-count bin would pin the curve to the
            // baseline, so a channel with no values in some sub-range (or sparse
            // data with gaps) would show sharp drops. Plotting only populated
            // bins connects each straight to the next non-zero value instead.
            if count == 0 {
                continue;
            }
            let x = rect.left() + (v as f32 / (nb - 1) as f32) * rect.width();
            let hh = (count as f32 / peak).sqrt();
            let y = rect.bottom() - hh * rect.height();
            pts.push(Pos2::new(x, y));
        }
        painter.add(egui::Shape::line(pts, Stroke::new(1.0_f32, colors[ci])));
    }

    // For a single grey (mono) curve, mark the most-frequent value — the peak
    // (mode) — with a full-height vertical line, and return its value so the
    // caller can print it under the graph. Multi-channel histograms would need
    // one per channel and get cluttered, so it's mono-only.
    if !hist.mono {
        return None;
    }
    let chan = hist.bins.first()?;
    let (peak_bin, &cnt) = chan.iter().enumerate().max_by_key(|&(_, &c)| c)?;
    if cnt == 0 {
        return None;
    }
    let nb = chan.len().max(2);
    let frac = peak_bin as f32 / (nb - 1) as f32;
    let x = rect.left() + frac * rect.width();
    painter.line_segment(
        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(1.5_f32, Color32::from_rgb(240, 200, 80)),
    );
    Some(hist.min + frac * (hist.max - hist.min))
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