//! Configuration: rebindable keybindings, column count, texture filtering.
//! Persisted as JSON in the platform config dir.

use std::collections::BTreeMap;
use std::path::PathBuf;

use eframe::egui::{self, Key};
use serde::{Deserialize, Serialize};

/// Everything a key can be bound to. `SelectMedia(i)` jumps straight to media i.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    ToggleView,
    ViewGrid,
    ViewSingle,
    ViewAb,
    NextMedia,
    PrevMedia,
    NextFrame,
    PrevFrame,
    ResetView,
    ActualSize,
    ZoomIn,
    ZoomOut,
    LoadAll,
    OpenFiles,
    ToggleSettings,
    ToggleManager,
    ToggleVis,
    ToggleExport,
    OpenCompute,
    PlayPause,
    ReloadMedia,
    ReloadAll,
    HideMedia,
    ToggleChrome,
    SelectMedia(usize),
}

impl Action {
    /// Stable string id used as the JSON key.
    pub fn id(&self) -> String {
        match self {
            Action::ToggleView => "toggle_view".into(),
            Action::ViewGrid => "view_grid".into(),
            Action::ViewSingle => "view_single".into(),
            Action::ViewAb => "view_ab".into(),
            Action::NextMedia => "next_media".into(),
            Action::PrevMedia => "prev_media".into(),
            Action::NextFrame => "next_frame".into(),
            Action::PrevFrame => "prev_frame".into(),
            Action::ResetView => "reset_view".into(),
            Action::ActualSize => "actual_size".into(),
            Action::ZoomIn => "zoom_in".into(),
            Action::ZoomOut => "zoom_out".into(),
            Action::LoadAll => "load_all".into(),
            Action::OpenFiles => "open_files".into(),
            Action::ToggleSettings => "toggle_settings".into(),
            Action::ToggleManager => "toggle_manager".into(),
            Action::ToggleVis => "toggle_vis".into(),
            Action::ToggleExport => "toggle_export".into(),
            Action::OpenCompute => "open_compute".into(),
            Action::PlayPause => "play_pause".into(),
            Action::ReloadMedia => "reload_media".into(),
            Action::ReloadAll => "reload_all".into(),
            Action::HideMedia => "hide_media".into(),
            Action::ToggleChrome => "toggle_chrome".into(),
            Action::SelectMedia(i) => format!("select_media_{}", i + 1),
        }
    }

    /// Human label for the settings UI.
    pub fn label(&self) -> String {
        match self {
            Action::ToggleView => "Toggle grid / single / A-B view".into(),
            Action::ViewGrid => "Switch to grid view".into(),
            Action::ViewSingle => "Switch to single view".into(),
            Action::ViewAb => "Switch to A/B view".into(),
            Action::NextMedia => "Next media".into(),
            Action::PrevMedia => "Previous media".into(),
            Action::NextFrame => "Next frame".into(),
            Action::PrevFrame => "Previous frame".into(),
            Action::ResetView => "Fit to view".into(),
            Action::ActualSize => "Actual size (100%)".into(),
            Action::ZoomIn => "Zoom in".into(),
            Action::ZoomOut => "Zoom out".into(),
            Action::LoadAll => "Load all frames into memory".into(),
            Action::OpenFiles => "Open files…".into(),
            Action::ToggleSettings => "Toggle settings".into(),
            Action::ToggleManager => "Toggle media manager".into(),
            Action::ToggleVis => "Toggle Transformations popup (focused pane)".into(),
            Action::ToggleExport => "Toggle export panel".into(),
            Action::OpenCompute => "Add a Compute pane".into(),
            Action::PlayPause => "Play / pause sequences".into(),
            Action::ReloadMedia => "Reload focused media from disk".into(),
            Action::ReloadAll => "Reload all media from disk".into(),
            Action::HideMedia => "Hide focused media".into(),
            Action::ToggleChrome => "Show/hide all UI bars (image-only view)".into(),
            Action::SelectMedia(i) => format!("Select media {}", i + 1),
        }
    }

    /// The full ordered list of bindable actions (12 media slots).
    pub fn all() -> Vec<Action> {
        let mut v = vec![
            Action::ToggleView,
            Action::ViewGrid,
            Action::ViewSingle,
            Action::ViewAb,
            Action::NextMedia,
            Action::PrevMedia,
            Action::NextFrame,
            Action::PrevFrame,
            Action::ResetView,
            Action::ActualSize,
            Action::ZoomIn,
            Action::ZoomOut,
            Action::LoadAll,
            Action::OpenFiles,
            Action::ToggleSettings,
            Action::ToggleManager,
            Action::ToggleVis,
            Action::ToggleExport,
            Action::OpenCompute,
            Action::PlayPause,
            Action::ReloadMedia,
            Action::ReloadAll,
            Action::HideMedia,
            Action::ToggleChrome,
        ];
        v.extend((0..12).map(Action::SelectMedia));
        v
    }
}

/// A bindable shortcut: a key plus optional modifiers (Ctrl / Shift / Alt).
/// `ctrl` maps to egui's cross-platform `command` (Ctrl on Windows/Linux, ⌘ on
/// macOS). Serialised as a `Ctrl+Shift+Key` string, so a pre-modifier config
/// storing a bare key name (e.g. `"Tab"`) still parses as a no-modifier chord.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: Key,
}

impl Chord {
    /// A key with no modifiers.
    pub fn plain(key: Key) -> Self {
        Self { ctrl: false, shift: false, alt: false, key }
    }

    /// Build from a captured key-press event's modifier state.
    pub fn from_modifiers(key: Key, m: egui::Modifiers) -> Self {
        Self { ctrl: m.command, shift: m.shift, alt: m.alt, key }
    }

    /// Canonical `Ctrl+Shift+Alt+Key` string (used for both storage and display).
    pub fn name(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        s.push_str(self.key.name());
        s
    }

    /// Parse the canonical string. A bare key name (no `+`) means no modifiers,
    /// so an older single-key config still loads.
    pub fn from_name(s: &str) -> Option<Self> {
        let (mut ctrl, mut shift, mut alt) = (false, false, false);
        let mut key = None;
        for part in s.split('+') {
            let p = part.trim();
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "command" => ctrl = true,
                "shift" => shift = true,
                "alt" | "option" => alt = true,
                _ => key = Key::from_name(p),
            }
        }
        Some(Self { ctrl, shift, alt, key: key? })
    }

    /// True when this exact chord (its key **and** its modifier set) fired this
    /// frame — an exact match, so `R` and `Ctrl+R` are distinct.
    pub fn pressed(&self, i: &egui::InputState) -> bool {
        i.key_pressed(self.key)
            && i.modifiers.command == self.ctrl
            && i.modifiers.shift == self.shift
            && i.modifiers.alt == self.alt
    }
}

/// Maps action ids -> chord strings (`Ctrl+Shift+Key`). Serialized as a plain
/// string map, so pre-modifier configs (bare key names) still load.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybindings {
    #[serde(flatten)]
    map: BTreeMap<String, String>,
}

impl Default for Keybindings {
    fn default() -> Self {
        let mut map = BTreeMap::new();
        let mut set = |a: Action, k: Key| {
            map.insert(a.id(), Chord::plain(k).name());
        };
        set(Action::ToggleView, Key::Tab);
        set(Action::ViewGrid, Key::G);
        set(Action::ViewSingle, Key::U);
        set(Action::ViewAb, Key::B);
        set(Action::NextMedia, Key::ArrowRight);
        set(Action::PrevMedia, Key::ArrowLeft);
        set(Action::NextFrame, Key::ArrowUp);
        set(Action::PrevFrame, Key::ArrowDown);
        set(Action::ResetView, Key::F);
        set(Action::ActualSize, Key::Num0);
        set(Action::ZoomIn, Key::Plus);
        set(Action::ZoomOut, Key::Minus);
        set(Action::LoadAll, Key::L);
        set(Action::OpenFiles, Key::O);
        set(Action::ToggleSettings, Key::S);
        set(Action::ToggleManager, Key::M);
        set(Action::ToggleVis, Key::V);
        set(Action::ToggleExport, Key::E);
        set(Action::OpenCompute, Key::C);
        set(Action::PlayPause, Key::Space);
        set(Action::ReloadMedia, Key::R);
        set(Action::HideMedia, Key::H);
        set(Action::ToggleChrome, Key::T);
        // Media 1..=9 -> digit keys; 10..12 left unbound by default (rebindable).
        let digits = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        for (i, k) in digits.into_iter().enumerate() {
            set(Action::SelectMedia(i), k);
        }
        // A modified default, to show chords work: Ctrl+R reloads every media.
        // (Inserted after the `set` closure's last use so it doesn't clash on `map`.)
        map.insert(
            Action::ReloadAll.id(),
            Chord { ctrl: true, shift: false, alt: false, key: Key::R }.name(),
        );
        Self { map }
    }
}

impl Keybindings {
    pub fn chord_for(&self, action: Action) -> Option<Chord> {
        self.map.get(&action.id()).and_then(|s| Chord::from_name(s))
    }

    pub fn set(&mut self, action: Action, chord: Chord) {
        // Keep bindings unique: clear any other action holding this exact chord.
        let name = chord.name();
        let this = action.id();
        self.map.retain(|k, v| k == &this || v != &name);
        self.map.insert(this, name);
    }

    pub fn clear(&mut self, action: Action) {
        self.map.remove(&action.id());
    }

    /// Rename legacy action ids in a loaded config so old bindings carry over
    /// (`toggle_headers` — the removed auto-hide-headers toggle — became
    /// `toggle_chrome`, the show/hide-all-UI toggle).
    fn migrate(&mut self) {
        if let Some(v) = self.map.remove("toggle_headers") {
            self.map.entry(Action::ToggleChrome.id()).or_insert(v);
        }
    }
}

/// Per-pane tone-mapping mode, chosen in the media manager. `LutAlpha` routes
/// the rendered image through the proprietary LUT_ALPHA auto-contrast (see
/// `crate::imageproc`); `Linear` is the built-in full-range map, with an
/// optional percentile clip toggled per pane (see [`ClipOptions`]).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
pub enum ContrastMode {
    /// Full-range mapping (native range → [0, 255]). A per-tail percentile clip
    /// is applied when [`ClipOptions::enabled`] is set (on by default for
    /// >8-bit sources). The default mode.
    #[default]
    Linear,
    /// Proprietary LUT_ALPHA auto-contrast, applied to the rendered image.
    LutAlpha,
    /// False-colour a **mono** image through a palette (see [`ToneOptions::palette`]).
    /// Uses the same window/clip bounds as Linear; multi-channel frames fall back
    /// to the plain render. A display-only tone (no proprietary operators).
    Colormap,
}

impl ContrastMode {
    /// The modes in dropdown order.
    pub const ORDER: [ContrastMode; 3] =
        [ContrastMode::Linear, ContrastMode::LutAlpha, ContrastMode::Colormap];

    /// Short label for the media-manager dropdown.
    pub fn label(self) -> &'static str {
        match self {
            ContrastMode::Linear => "Linear",
            ContrastMode::LutAlpha => "LUT_ALPHA",
            ContrastMode::Colormap => "Colormap",
        }
    }
}

/// The optional percentile clip on the **Linear** tone: a toggle plus the
/// per-tail percentile. When `enabled`, the render clips `percent`% off each
/// tail before the full-range stretch (robust auto-contrast); when off, it maps
/// the plain native range. `enabled` defaults on (seeded per source depth in
/// `add_pane`: on for >8-bit, off for 8-bit which displays 1:1).
#[derive(Clone, Copy, PartialEq)]
pub struct ClipOptions {
    /// Whether the percentile clip is applied at all.
    pub enabled: bool,
    /// Percentile clipped at *each* tail before the full-range stretch, in
    /// percent (0.01 = the robust default; 0 = plain min/max).
    pub percent: f32,
}

impl Default for ClipOptions {
    fn default() -> Self {
        Self { enabled: true, percent: 0.01 }
    }
}

/// Per-pane tone options. Extend by growing this struct and reading it in
/// `stage`/`tone_bounds`/`tone_sig`/`view_command`/`export_pane`. (LUT_ALPHA has
/// no options; it runs the operator at full strength.)
#[derive(Clone, Copy, PartialEq, Default)]
pub struct ToneOptions {
    pub clip: ClipOptions,
    /// Lock this pane's display bounds to the **Control** media's `[lo, hi]`
    /// (its clip / full-range map) instead of computing its own, so panes share
    /// identical bounds — real intensity differences then show as brightness
    /// rather than being hidden by per-pane auto-normalisation. Off by default;
    /// overrides this pane's own clip when on. Ignored by LUT_ALPHA, which does
    /// its own contrast.
    pub share_clip: bool,
    /// Palette for the Colormap tone (ignored by other modes).
    pub palette: crate::palette::Palette,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub max_columns: usize,
    /// Global UI zoom factor for buttons/text (egui zoom_factor).
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    /// Soft ceiling, in MiB, on decoded frames kept resident across all
    /// sequences before the least-recently-viewed ones are evicted.
    #[serde(default = "default_cache_budget_mb")]
    pub cache_budget_mb: usize,
    /// Number of background image-decoding worker threads shared by all
    /// sequences. `0` = auto (scale with CPU cores, capped). Lower it to leave
    /// CPU for other users when several instances share one server / VNC host.
    #[serde(default = "default_decode_threads")]
    pub decode_threads: usize,
    /// Replicate the hovered pixel onto the other panes as a red dot (the pane
    /// under the cursor is skipped — its own cursor marks the spot).
    #[serde(default = "default_true")]
    pub cursor_dot: bool,
    /// Directory holding the proprietary C++ operator shared libraries (`.so`).
    /// Empty = resolve them by bare name via the system loader search path
    /// (`LD_LIBRARY_PATH`). Applied at startup (see `crate::imageproc::init`).
    #[serde(default)]
    pub cpp_lib_dir: String,
    pub keybindings: Keybindings,
}

fn default_ui_scale() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

fn default_cache_budget_mb() -> usize {
    1536 // 1.5 GiB
}

fn default_decode_threads() -> usize {
    0 // auto: scale with CPU cores (see CimApp::resolve_decode_threads)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_columns: 3,
            ui_scale: default_ui_scale(),
            cache_budget_mb: default_cache_budget_mb(),
            decode_threads: default_decode_threads(),
            cursor_dot: true,
            cpp_lib_dir: String::new(),
            keybindings: Keybindings::default(),
        }
    }
}

impl Config {
    fn path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("dev", "cim", "cim")?;
        // On Linux the XDG config dir is `~/.config/cim`, so the file lands at
        // `~/.config/cim/cim.json`. Other platforms keep `config.json`.
        let name = if cfg!(target_os = "linux") {
            "cim.json"
        } else {
            "config.json"
        };
        Some(dirs.config_dir().join(name))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let mut c: Config = serde_json::from_str(&s).unwrap_or_default();
                c.keybindings.migrate();
                c
            }
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }
}
