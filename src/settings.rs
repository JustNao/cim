//! Configuration: rebindable keybindings, column count, texture filtering.
//! Persisted as JSON in the platform config dir.

use std::collections::BTreeMap;
use std::path::PathBuf;

use eframe::egui::Key;
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
    PlayPause,
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
            Action::PlayPause => "play_pause".into(),
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
            Action::PlayPause => "Play / pause sequences".into(),
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
            Action::PlayPause,
        ];
        v.extend((0..12).map(Action::SelectMedia));
        v
    }
}

/// Maps action ids -> key names. Serialized as a plain string map.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybindings {
    #[serde(flatten)]
    map: BTreeMap<String, String>,
}

impl Default for Keybindings {
    fn default() -> Self {
        let mut map = BTreeMap::new();
        let mut set = |a: Action, k: Key| {
            map.insert(a.id(), k.name().to_string());
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
        set(Action::PlayPause, Key::Space);
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
        Self { map }
    }
}

impl Keybindings {
    pub fn key_for(&self, action: Action) -> Option<Key> {
        self.map.get(&action.id()).and_then(|s| Key::from_name(s))
    }

    pub fn set(&mut self, action: Action, key: Key) {
        // Keep bindings unique: clear any other action holding this key.
        let name = key.name().to_string();
        let this = action.id();
        self.map.retain(|k, v| k == &this || v != &name);
        self.map.insert(this, name);
    }

    pub fn clear(&mut self, action: Action) {
        self.map.remove(&action.id());
    }
}

/// Per-pane tone-mapping mode, chosen in the media manager. `LutAlpha` routes
/// the rendered image through the proprietary LUT_ALPHA auto-contrast (see
/// `crate::imageproc`); the other two are built-in mappings.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
pub enum ContrastMode {
    /// Full-range mapping with a 0.01% percentile clip (robust auto-contrast).
    /// The default.
    #[default]
    LinearClip,
    /// Proprietary LUT_ALPHA auto-contrast, applied to the rendered image.
    LutAlpha,
    /// Straight full-range mapping (native range → [0, 255], no clip).
    Linear,
}

impl ContrastMode {
    /// The modes in dropdown order.
    pub const ORDER: [ContrastMode; 3] = [
        ContrastMode::LinearClip,
        ContrastMode::LutAlpha,
        ContrastMode::Linear,
    ];

    /// Short label for the media-manager dropdown.
    pub fn label(self) -> &'static str {
        match self {
            ContrastMode::LinearClip => "Linear + Clip",
            ContrastMode::LutAlpha => "LUT_ALPHA",
            ContrastMode::Linear => "Linear",
        }
    }

    /// Whether the built-in render should apply the percentile clip.
    pub fn clips(self) -> bool {
        matches!(self, ContrastMode::LinearClip)
    }
}

/// Options for the **Linear + Clip** tone.
#[derive(Clone, Copy, PartialEq)]
pub struct ClipOptions {
    /// Percentile clipped at *each* tail before the full-range stretch, in
    /// percent (0.01 = the robust default; 0 = plain min/max).
    pub percent: f32,
}

impl Default for ClipOptions {
    fn default() -> Self {
        Self { percent: 0.01 }
    }
}

/// Options for the proprietary **LUT_ALPHA** tone. To add a knob: add a field
/// here, a widget row in `draw_tone_options`, and read it where the operator
/// runs (`app/decode.rs::prepare`).
#[derive(Clone, Copy, PartialEq)]
pub struct LutAlphaOptions {
    /// Mix between the plain linear image (0.0) and the LUT_ALPHA result (1.0),
    /// applied Rust-side after the operator. 1.0 = the operator's full output.
    pub blend: f32,
}

impl Default for LutAlphaOptions {
    fn default() -> Self {
        Self { blend: 1.0 }
    }
}

/// Per-pane tone options: one sub-struct per mode, so switching modes keeps each
/// mode's own settings. Extend a mode by growing its sub-struct (see above).
#[derive(Clone, Copy, PartialEq, Default)]
pub struct ToneOptions {
    pub clip: ClipOptions,
    pub lut_alpha: LutAlphaOptions,
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
    /// Replicate the hovered pixel onto the other panes as a red dot (the pane
    /// under the cursor is skipped — its own cursor marks the spot).
    #[serde(default = "default_true")]
    pub cursor_dot: bool,
    /// Path to the optional proprietary image-processing library (`.so`/`.dll`)
    /// loaded at runtime for the LUT_ALPHA / Details operators. `None` (or a
    /// missing file) leaves those features disabled. See `crate::imageproc`.
    #[serde(default)]
    pub ops_library_path: Option<PathBuf>,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            max_columns: 3,
            ui_scale: default_ui_scale(),
            cache_budget_mb: default_cache_budget_mb(),
            cursor_dot: true,
            ops_library_path: None,
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
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
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
