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
                    cli::Tone::LutAlpha => ContrastMode::LutAlpha,
                };
                p.sync_tone = false;
                // Restored tone re-renders via `tone_sig`; no `tex` nulling (it
                // would flash black for a heavy LUT_ALPHA/details pane).
            }
        }
        // Per-pane Linear clip (`--clip`): a toggle + percentile. Like --tone this
        // is per-pane, so unsync the panes it sets.
        if let Some(clips) = &vs.clips {
            for (p, c) in self.panes.iter_mut().zip(clips) {
                match c {
                    cli::ClipSpec::Off => p.tone.clip.enabled = false,
                    cli::ClipSpec::On(pct) => {
                        p.tone.clip.enabled = true;
                        p.tone.clip.percent = *pct;
                    }
                }
                p.sync_tone = false;
            }
        }
        if let Some(details) = &vs.details {
            for (p, d) in self.panes.iter_mut().zip(details) {
                p.details = *d;
                p.sync_tone = false;
            }
        }
        // Per-pane rotation. Like --tone/--detail these are per-pane, so unsync
        // the panes they set (otherwise a synced pane would ignore its own angle
        // and follow the shared one); the following --tsync re-syncs and re-seeds.
        if let Some(rots) = &vs.rotations {
            for (p, &r) in self.panes.iter_mut().zip(rots) {
                p.rotation = wrap180(r);
                p.sync_tone = false;
            }
        }
        // Transformations sync flags, applied *after* per-pane tone/detail/rotation
        // (which unsync the panes they set). Re-seed the shared set from the first
        // synced pane so panes that follow it show the captured look.
        if let Some(sync) = &vs.tsync {
            if let Some(k) = sync.iter().position(|&s| s) {
                if let Some(p) = self.panes.get(k) {
                    self.shared_contrast = p.contrast;
                    self.shared_tone = p.tone;
                    self.shared_details = p.details;
                    self.shared_rotation = p.rotation;
                }
            }
            for (p, &s) in self.panes.iter_mut().zip(sync) {
                p.sync_tone = s;
                // Effective tone changed → re-renders via `tone_sig`; no nulling.
            }
        }
        if let Some(vis) = &vs.visible {
            for (p, &v) in self.panes.iter_mut().zip(vis) {
                p.visible = v;
            }
        }
        if let Some(c) = vs.control {
            if n > 0 {
                self.control = c.min(n - 1);
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
    /// timeline frame, per-pane tone/detail/visibility/Transformations-sync, the
    /// focused and control panes, the loop range and (in A/B) the operands +
    /// split. Anything left at its default is omitted to keep the line short.
    ///
    /// Only the *shared* view is captured — panes with their own view (sync off)
    /// fall back to it. Sequences are listed as their individual files (the
    /// compact `PREFIX%0Xu…,…` token isn't reconstructed).
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
        // Only emit a flag when it differs from the app's default, so the line
        // stays short. Layout:
        if self.mode != Mode::Grid {
            let mode = match self.mode {
                Mode::Grid => "grid",
                Mode::Single => "single",
                Mode::Ab => "ab",
            };
            parts.push(format!("--mode {mode}"));
        }
        if self.config.max_columns != Config::default().max_columns {
            parts.push(format!("--cols {}", self.config.max_columns));
        }
        // Zoom / centre are the point of the command (they capture where you are
        // in the image), so they're always emitted.
        let v = self.shared_view;
        parts.push(format!("--zoom {:.4}", v.zoom));
        parts.push(format!("--center {:.2},{:.2}", v.center.x, v.center.y));
        if self.timeline_len() > 1 && self.shared_frame != 0 {
            parts.push(format!("--frame {}", self.shared_frame));
        }
        let n = self.panes.len();
        if n > 0 {
            // Per-pane tone mode (effective — shared when tone-synced). The mode
            // is Linear for every pane unless LUT_ALPHA is chosen, so omit `--tone`
            // when no pane uses LUT_ALPHA.
            let tones: Vec<&str> = (0..n)
                .map(|i| match self.contrast_of(i) {
                    ContrastMode::Linear => "linear",
                    ContrastMode::LutAlpha => "lutalpha",
                })
                .collect();
            if (0..n).any(|i| tones[i] != "linear") {
                parts.push(format!("--tone {}", tones.join(",")));
            }
            // Per-pane Linear clip (effective): `off` or the per-tail percentile.
            // Omit when every pane is at its depth-appropriate default (on at
            // 0.01% for >8-bit, off for 8-bit).
            let clips: Vec<String> = (0..n)
                .map(|i| {
                    let clip = self.tone_of(i).clip;
                    if clip.enabled {
                        format!("{}", (clip.percent * 1000.0).round() / 1000.0)
                    } else {
                        "off".into()
                    }
                })
                .collect();
            let clip_default = |i: usize| -> &str {
                if self.panes[i].media.hi_depth() {
                    "0.01"
                } else {
                    "off"
                }
            };
            if (0..n).any(|i| clips[i].as_str() != clip_default(i)) {
                parts.push(format!("--clip {}", clips.join(",")));
            }
            // Details / show / Transformations-sync — omit when all at default
            // (details off, all visible, all synced).
            if (0..n).any(|i| self.details_of(i)) {
                let details: Vec<&str> = (0..n)
                    .map(|i| if self.details_of(i) { "1" } else { "0" })
                    .collect();
                parts.push(format!("--detail {}", details.join(",")));
            }
            if self.panes.iter().any(|p| !p.visible) {
                let show: Vec<&str> = self
                    .panes
                    .iter()
                    .map(|p| if p.visible { "1" } else { "0" })
                    .collect();
                parts.push(format!("--show {}", show.join(",")));
            }
            if self.panes.iter().any(|p| !p.sync_tone) {
                let ts: Vec<&str> = self
                    .panes
                    .iter()
                    .map(|p| if p.sync_tone { "1" } else { "0" })
                    .collect();
                parts.push(format!("--tsync {}", ts.join(",")));
            }
            // Per-pane effective rotation — omit when every pane is unrotated.
            if (0..n).any(|i| self.rotation_of(i) != 0.0) {
                let rots: Vec<String> = (0..n)
                    .map(|i| {
                        let r = self.rotation_of(i);
                        format!("{}", (r * 100.0).round() / 100.0)
                    })
                    .collect();
                parts.push(format!("--rotate {}", rots.join(",")));
            }
        }
        if let Some((lo, hi)) = self.loop_range {
            parts.push(format!("--loop {lo},{hi}"));
        }
        if n > 0 {
            if self.current != 0 {
                parts.push(format!("--pane {}", self.current.min(n - 1)));
            }
            if self.control != 0 {
                parts.push(format!("--control {}", self.control.min(n - 1)));
            }
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
    ///
    /// Media are loaded first (cheap — metadata / page 0 only, decoding is lazy),
    /// then gated: if the result would leave **more than `SEQ_WARN_LIMIT`
    /// sequences** open at once, the loaded media are held in `pending_open` and a
    /// resource-warning confirmation is shown instead of adding the panes now (see
    /// the popup in `update`). Otherwise they're added immediately.
    pub(super) fn open_inputs(&mut self, inputs: Vec<cli::Input>) {
        let mut loaded: Vec<(Media, Source)> = Vec::new();
        for input in inputs {
            let (res, source) = match input {
                cli::Input::Single(p) => (media::load(&p), Source::File(p)),
                cli::Input::Sequence { token, files } => (
                    media::load_sequence(&files, token.clone()),
                    Source::Sequence { token, files },
                ),
            };
            match res {
                Ok(m) => loaded.push((m, source)),
                Err(e) => self.error_popup = Some(format!("Failed to open:\n{e}")),
            }
        }

        // Count sequences (multi-frame media) that would be open after this —
        // panes already up, plus any batch already waiting behind the warning, plus
        // the ones now loading. Including `pending_open` keeps a second drop gated
        // instead of slipping panes in while the big batch still waits.
        let open_seqs = self.panes.iter().filter(|p| p.media.is_sequence()).count();
        let waiting_seqs = self
            .pending_open
            .as_ref()
            .map(|b| b.iter().filter(|(m, _)| m.is_sequence()).count())
            .unwrap_or(0);
        let opening = loaded.iter().filter(|(m, _)| m.is_sequence()).count();
        if open_seqs + waiting_seqs + opening > SEQ_WARN_LIMIT {
            // Hold the load behind the warning; `commit_open` finishes it on
            // confirm. Merge with any batch already waiting (rapid drops).
            match &mut self.pending_open {
                Some(pend) => pend.extend(loaded),
                None => self.pending_open = Some(loaded),
            }
            return;
        }
        self.commit_open(loaded);
    }

    /// Add a batch of already-loaded media as panes and re-settle the view
    /// selectors. Shared by the immediate path and the confirmed ">8 sequences"
    /// path (`update`), so both run the same post-open fixups.
    fn commit_open(&mut self, loaded: Vec<(Media, Source)>) {
        for (m, source) in loaded {
            self.add_pane(m, source);
        }
        let n = self.panes.len();
        self.current = self.current.min(n.saturating_sub(1));
        self.slot_a = self.slot_a.min(n.saturating_sub(1));
        self.slot_b = self.slot_b.min(n.saturating_sub(1));
        if n >= 2 && self.slot_a == self.slot_b {
            self.slot_b = self.slot_a + 1;
        }
        self.shared_view.needs_fit = true;
        // A view state deferred at startup (behind the warning) applies now that
        // the panes exist.
        if let Some(v) = self.pending_view.take() {
            self.apply_view_state(v);
        }
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

    /// Push a freshly loaded media as a new pane with default per-pane state.
    fn add_pane(&mut self, media: Media, source: Source) {
        let id = self.next_id;
        self.next_id += 1;
        // Always the built-in Linear map; the clip toggle carries the auto-
        // contrast. >8-bit sources need it to be legible, so clip defaults on;
        // 8-bit displays 1:1, so clip defaults off (a plain identity map).
        let contrast = ContrastMode::Linear;
        let mut tone = ToneOptions::default();
        tone.clip.enabled = media.hi_depth();
        // Transformations sync is on by default; the first opened media seeds the
        // shared set (so its depth-appropriate tone becomes the group default).
        if self.panes.is_empty() {
            self.shared_contrast = contrast;
            self.shared_tone = tone;
            self.shared_details = false;
            self.shared_rotation = 0.0;
        }
        self.panes.push(Pane {
            id,
            source,
            media,
            tex: None,
            pending: None,
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
            rotation: 0.0,
            overlay: None,
            overlay_tex: None,
            region_tone: false,
            stats: None,
            hist: None,
            compute: None,
            error: None,
            eager: Eager::Off,
            watch: false,
            watch_loaded: None,
            watch_seen: None,
        });
    }

    pub(super) fn remove_media(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let removed_id = self.panes[i].id;
        self.decoder.forget(removed_id); // drop its persistent reader
        self.renderer.forget(removed_id); // drop its render thread + operator instances
        self.render_inflight.remove(&removed_id);
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
                self.renderer.forget(id); // rebuild the render thread + instances for fresh contents
                self.render_inflight.remove(&id);
                // Drop stale in-flight decodes aimed at the old contents.
                self.inflight.retain(|(pid, _)| *pid != id);
                self.panes[i].media = m;
                self.panes[i].tex = None; // re-render the kept frame from fresh data
                self.panes[i].pending = None; // drop any staged frame from old data
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
                // Re-baseline any file watch to the freshly-loaded contents so it
                // doesn't immediately fire again on the change we just picked up.
                self.panes[i].watch_loaded = Self::source_file_sig(&self.panes[i].source);
                self.panes[i].watch_seen = None;
            }
            Err(e) => self.panes[i].error = Some(format!("Reload failed: {e}")),
        }
    }

    /// On-disk signature of a pane's source: the latest mtime across its file(s)
    /// and their total length. `None` for a Compute pane (no file) or when any
    /// file can't be stat-ed right now (e.g. mid-rename) — in which case the
    /// watch simply waits for the next poll rather than reloading torn contents.
    fn source_file_sig(source: &Source) -> Option<FileSig> {
        let paths: &[PathBuf] = match source {
            Source::File(p) => std::slice::from_ref(p),
            Source::Sequence { files, .. } => files.as_slice(),
            Source::Computed => return None,
        };
        let mut latest: Option<std::time::SystemTime> = None;
        let mut total = 0u64;
        for p in paths {
            let m = std::fs::metadata(p).ok()?;
            total += m.len();
            let mt = m.modified().ok()?;
            latest = Some(match latest {
                Some(l) if l >= mt => l,
                _ => mt,
            });
        }
        latest.map(|l| (l, total))
    }

    /// Poll every watched pane's source file(s) and reload those whose contents
    /// have changed and then settled (unchanged for `WATCH_DEBOUNCE`). Runs before
    /// `refresh_textures`, so the reloaded frame re-renders and commits in step
    /// with the other panes instead of flashing. Cheap: one `stat` per file, and
    /// only fires the (heavier) reload once a change has quiesced.
    pub(super) fn poll_watches(&mut self, now: f64) {
        let mut to_reload: Vec<usize> = Vec::new();
        for i in 0..self.panes.len() {
            if !self.panes[i].watch {
                continue;
            }
            let Some(sig) = Self::source_file_sig(&self.panes[i].source) else {
                continue; // unreadable this tick (mid-write/rename) — try again later
            };
            // Establish the baseline on the first successful stat.
            let Some(loaded) = self.panes[i].watch_loaded else {
                self.panes[i].watch_loaded = Some(sig);
                self.panes[i].watch_seen = None;
                continue;
            };
            if sig == loaded {
                self.panes[i].watch_seen = None; // unchanged (or reverted)
                continue;
            }
            // Changed from the loaded contents: wait for it to stop changing.
            match self.panes[i].watch_seen {
                Some((seen, t0)) if seen == sig => {
                    if now - t0 >= WATCH_DEBOUNCE {
                        self.panes[i].watch_seen = None;
                        to_reload.push(i);
                    }
                }
                // First sighting of this signature (or it changed again) — (re)arm.
                _ => self.panes[i].watch_seen = Some((sig, now)),
            }
        }
        for i in to_reload {
            self.reload(i); // re-baselines watch_loaded to the fresh contents
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

    /// Add a new, *unconfigured* Compute pane (from the toolbar "Compute"
    /// button). It shows the in-pane config form (mode + source pickers + a
    /// Compute button); the result appears once that button computes it.
    fn add_compute_pane(&mut self) {
        let was_empty = self.panes.is_empty();
        self.add_pane(
            media::Media::still("Compute".into(), media::placeholder_frame()),
            Source::Computed,
        );
        let i = self.panes.len() - 1;
        // Default source A to the previously focused pane when it can be one.
        let prev = self.current.min(i.saturating_sub(1));
        let default_src = self
            .panes
            .get(prev)
            .filter(|p| p.compute.is_none())
            .map(|p| p.id);
        self.panes[i].compute = Some(Compute {
            kind: Reduce::Mean,
            source_id: default_src,
            source_b: None,
            computed: false,
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
                self.panes[idx].pending = None; // drop any frame staged from old inputs
                self.panes[idx].hist = None; // recompute for the new result
                self.panes[idx].contrast = ContrastMode::Linear;
                self.panes[idx].tone.clip.enabled = hi; // clip >8-bit results

                if let Some(c) = self.panes[idx].compute.as_mut() {
                    c.computed = true; // switch from the config form to the result
                }
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

/// `<cim executable directory>/LIBS`, used when no library folder is configured.
fn default_cpp_lib_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("LIBS"))
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
    painter.rect_stroke(r, 0.0, Stroke::new(2.0_f32, Color32::from_rgb(240, 200, 80)));
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
        ContrastMode::Linear => {
            // Clip toggle + its per-tail percentile (greyed out when the toggle
            // is off). On for >8-bit by default, off for 8-bit (set in add_pane).
            ui.label("Clip");
            ui.horizontal(|ui| {
                ui.add(egui::Checkbox::without_text(&mut tone.clip.enabled))
                    .on_hover_text("Clip a percentile off each tail before the stretch");
                ui.add_enabled(
                    tone.clip.enabled,
                    egui::DragValue::new(&mut tone.clip.percent)
                        .speed(0.005)
                        .range(0.0..=49.0)
                        .max_decimals(3)
                        .suffix(" %"),
                )
                .on_hover_text("Percentile clipped at each tail before the stretch");
            });
            ui.end_row();
        }
        // LUT_ALPHA has no options. Add a knob here: one row + a field on
        // `ToneOptions`.
        ContrastMode::LutAlpha => {}
    }
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

/// Rotate screen point `p` about `pivot` by `theta` radians (screen y is down,
/// so a positive angle turns clockwise on screen).
fn rotate_around(p: Pos2, pivot: Pos2, theta: f32) -> Pos2 {
    if theta == 0.0 {
        return p;
    }
    let (s, c) = theta.sin_cos();
    let d = p - pivot;
    pivot + Vec2::new(d.x * c - d.y * s, d.x * s + d.y * c)
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

/// Format a rotation angle for the Transformations text box: whole degrees
/// plainly, otherwise one decimal.
fn fmt_angle(v: f32) -> String {
    if v.fract().abs() < 0.05 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.1}")
    }
}

/// Image-space centre (in pixels) of a frame of the given size.
fn center_vec(size: [usize; 2]) -> Vec2 {
    Vec2::new(size[0] as f32 / 2.0, size[1] as f32 / 2.0)
}

/// Paint texture `id` into `rect`, rotated by `theta` radians about the rect's
/// centre. `theta == 0` takes the plain axis-aligned path; otherwise a two-triangle
/// textured mesh with the four corners rotated (clipped by the painter's clip rect,
/// so the image still can't spill past its pane).
fn paint_rotated(painter: &egui::Painter, id: TextureId, rect: Rect, theta: f32) {
    if theta == 0.0 {
        painter.image(id, rect, uv(), Color32::WHITE);
        return;
    }
    let pivot = rect.center();
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let uvs = [
        Pos2::new(0.0, 0.0),
        Pos2::new(1.0, 0.0),
        Pos2::new(1.0, 1.0),
        Pos2::new(0.0, 1.0),
    ];
    let mut mesh = egui::Mesh::with_texture(id);
    for (corner, uv) in corners.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: rotate_around(corner, pivot, theta),
            uv,
            color: Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// Shortest distance from point `p` to the segment `a`–`b` (screen space), used
/// to hit-test the profile line's body.
fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
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
