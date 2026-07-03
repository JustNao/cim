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
            Action::ToggleView => "Toggle grid / single view".into(),
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
            Action::ToggleVis => "Toggle visualisation panel".into(),
            Action::ToggleExport => "Toggle export panel".into(),
            Action::PlayPause => "Play / pause sequences".into(),
            Action::SelectMedia(i) => format!("Select media {}", i + 1),
        }
    }

    /// The full ordered list of bindable actions (12 media slots).
    pub fn all() -> Vec<Action> {
        let mut v = vec![
            Action::ToggleView,
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
#[derive(Clone, Serialize, Deserialize)]
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

/// Texture magnification filter used when zooming past 100%.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interpolation {
    /// Nearest-neighbour: exact square samples, no blending.
    Nearest,
    /// Bilinear: linear interpolation between the 4 nearest texels.
    Bilinear,
}

impl Default for Interpolation {
    fn default() -> Self {
        Interpolation::Nearest
    }
}

/// Global visualisation settings. Grows over time. (Intensity clip is per-pane,
/// toggled in the media manager, so it lives on the pane, not here.)
#[derive(Clone, Serialize, Deserialize)]
pub struct VisSettings {
    #[serde(default)]
    pub interp: Interpolation,
}

impl Default for VisSettings {
    fn default() -> Self {
        Self {
            interp: Interpolation::Nearest,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub max_columns: usize,
    #[serde(default)]
    pub vis: VisSettings,
    /// Global UI zoom factor for buttons/text (egui zoom_factor).
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    pub keybindings: Keybindings,
}

fn default_ui_scale() -> f32 {
    1.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_columns: 3,
            vis: VisSettings::default(),
            ui_scale: default_ui_scale(),
            keybindings: Keybindings::default(),
        }
    }
}

impl Config {
    fn path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("dev", "cim", "cim")?;
        Some(dirs.config_dir().join("config.json"))
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
