use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::actions::Action;
use crate::gallery::BackgroundColor;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub keybindings: Vec<KeyBinding>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub tile_count: usize,
    pub cache_size: usize,
    pub slideshow_interval_secs: f64,
    pub background_color: BackgroundColor,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            tile_count: 9,
            cache_size: 50,
            slideshow_interval_secs: 5.0,
            background_color: BackgroundColor::Black,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyBinding {
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<String>,
    pub action: String,
    #[serde(default = "empty_toml_table")]
    pub args: toml::Value,
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            keybindings: default_keybindings(),
        }
    }
}

fn default_keybindings() -> Vec<KeyBinding> {
    let raw = r#"
[[keybindings]]
key = "ArrowRight"
modifiers = []
action = "Navigate"
args = { direction = "next" }

[[keybindings]]
key = "ArrowLeft"
modifiers = []
action = "Navigate"
args = { direction = "prev" }

[[keybindings]]
key = "ArrowDown"
modifiers = []
action = "NavigateTilingRow"
args = { direction = "next" }

[[keybindings]]
key = "ArrowUp"
modifiers = []
action = "NavigateTilingRow"
args = { direction = "prev" }

[[keybindings]]
key = "L"
modifiers = []
action = "Navigate"
args = { direction = "next" }

[[keybindings]]
key = "H"
modifiers = []
action = "Navigate"
args = { direction = "prev" }

[[keybindings]]
key = "J"
modifiers = []
action = "NavigateTilingRow"
args = { direction = "next" }

[[keybindings]]
key = "K"
modifiers = []
action = "NavigateTilingRow"
args = { direction = "prev" }

[[keybindings]]
key = "T"
modifiers = []
action = "SwitchMode"
args = { mode = "toggle" }

[[keybindings]]
key = "S"
modifiers = []
action = "ToggleSlideshow"
args = {}

[[keybindings]]
key = "Q"
modifiers = []
action = "Quit"
args = {}

[[keybindings]]
key = "R"
modifiers = []
action = "CycleRating"
args = { values = [1, 2, 3, 4, 5] }

[[keybindings]]
key = "Escape"
modifiers = []
action = "SwitchMode"
args = { mode = "tiling" }

[[keybindings]]
key = "Enter"
modifiers = []
action = "SwitchMode"
args = { mode = "single" }

[[keybindings]]
key = "OpenBracket"
modifiers = []
action = "ApplyFilter"
args = { filter = "RotateLeft" }

[[keybindings]]
key = "CloseBracket"
modifiers = []
action = "ApplyFilter"
args = { filter = "RotateRight" }

[[keybindings]]
key = "Plus"
modifiers = []
action = "ZoomTiling"
args = { delta = 1 }

[[keybindings]]
key = "Minus"
modifiers = []
action = "ZoomTiling"
args = { delta = -1 }

[[keybindings]]
key = "PageDown"
modifiers = []
action = "Navigate"
args = { direction = "next", count = 10 }

[[keybindings]]
key = "PageUp"
modifiers = []
action = "Navigate"
args = { direction = "prev", count = 10 }

[[keybindings]]
key = "E"
modifiers = []
action = "ToggleFilterSidebar"
args = {}

[[keybindings]]
key = "B"
modifiers = []
action = "CycleBackground"
args = {}

[[keybindings]]
key = "I"
modifiers = []
action = "ToggleHistogram"
args = {}

[[keybindings]]
key = "F"
modifiers = []
action = "ToggleFullscreen"
args = {}

[[keybindings]]
key = "Z"
modifiers = []
action = "ZoomSingleFit"
args = {}

[[keybindings]]
key = "Num1"
modifiers = []
action = "ZoomSingleToOne"
args = {}

[[keybindings]]
key = "N"
modifiers = []
action = "ToggleAnnotations"
args = {}
"#;
    #[derive(Deserialize)]
    struct Wrapper {
        keybindings: Vec<KeyBinding>,
    }
    let w: Wrapper = toml::from_str(raw).expect("default keybindings are valid");
    w.keybindings
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            log::info!("No config file found at {path:?}, using defaults");
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {path:?}"))?;
        toml::from_str(&text).with_context(|| format!("parsing config {path:?}"))
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("galerie-montsouris")
            .join("config.toml")
    }

    pub fn build_keybinding_map(&self) -> Result<KeyBindingMap> {
        let mut map = HashMap::new();
        for kb in &self.keybindings {
            let key = parse_key(&kb.key)
                .with_context(|| format!("unknown key {:?} in keybindings", kb.key))?;
            let modifiers = parse_modifiers(&kb.modifiers);
            let action = Action::from_binding(&kb.action, &kb.args)
                .with_context(|| format!("action {:?} has invalid args", kb.action))?;
            map.insert(KeyCombo { key, modifiers }, action);
        }
        Ok(map)
    }
}

pub type KeyBindingMap = HashMap<KeyCombo, Action>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub key: egui::Key,
    pub modifiers: ModifierSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ModifierSet {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl ModifierSet {
    #[allow(dead_code)]
    pub fn matches(&self, m: &egui::Modifiers) -> bool {
        self.ctrl == m.ctrl && self.shift == m.shift && self.alt == m.alt
    }
}

fn parse_modifiers(mods: &[String]) -> ModifierSet {
    let mut s = ModifierSet::default();
    for m in mods {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => s.ctrl = true,
            "shift" => s.shift = true,
            "alt" => s.alt = true,
            _ => log::warn!("unknown modifier: {m}"),
        }
    }
    s
}

fn parse_key(name: &str) -> Option<egui::Key> {
    use egui::Key::*;
    Some(match name {
        "ArrowDown" => ArrowDown,
        "ArrowLeft" => ArrowLeft,
        "ArrowRight" => ArrowRight,
        "ArrowUp" => ArrowUp,
        "Escape" => Escape,
        "Tab" => Tab,
        "Backspace" => Backspace,
        "Enter" | "Return" => Enter,
        "Space" => Space,
        "Insert" => Insert,
        "Delete" => Delete,
        "Home" => Home,
        "End" => End,
        "PageUp" => PageUp,
        "PageDown" => PageDown,
        "A" => A,
        "B" => B,
        "C" => C,
        "D" => D,
        "E" => E,
        "F" => F,
        "G" => G,
        "H" => H,
        "I" => I,
        "J" => J,
        "K" => K,
        "L" => L,
        "M" => M,
        "N" => N,
        "O" => O,
        "P" => P,
        "Q" => Q,
        "R" => R,
        "S" => S,
        "T" => T,
        "U" => U,
        "V" => V,
        "W" => W,
        "X" => X,
        "Y" => Y,
        "Z" => Z,
        "Num0" => Num0,
        "Num1" => Num1,
        "Num2" => Num2,
        "Num3" => Num3,
        "Num4" => Num4,
        "Num5" => Num5,
        "Num6" => Num6,
        "Num7" => Num7,
        "Num8" => Num8,
        "Num9" => Num9,
        "F1" => F1,
        "F2" => F2,
        "F3" => F3,
        "F4" => F4,
        "F5" => F5,
        "F6" => F6,
        "F7" => F7,
        "F8" => F8,
        "F9" => F9,
        "F10" => F10,
        "F11" => F11,
        "F12" => F12,
        "Minus" => Minus,
        "Plus" | "Equals" => Plus,
        "Comma" => Comma,
        "Period" => Period,
        "Semicolon" => Semicolon,
        "Colon" => Colon,
        "Slash" => Slash,
        "Backslash" => Backslash,
        "OpenBracket" => OpenBracket,
        "CloseBracket" => CloseBracket,
        "Backtick" | "Grave" => Backtick,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg = Config::default();
        assert_eq!(cfg.general.tile_count, 9);
        assert!(!cfg.keybindings.is_empty());
    }

    #[test]
    fn toml_round_trip() {
        let toml_text = r#"
[general]
tile_count = 4
cache_size = 20
slideshow_interval_secs = 3.0

[[keybindings]]
key = "ArrowRight"
modifiers = []
action = "Navigate"
args = { direction = "next" }
"#;
        let cfg: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.general.tile_count, 4);
        assert_eq!(cfg.keybindings.len(), 1);
    }

    #[test]
    fn zoom_tiling_keybinding_parses() {
        let toml_text = r#"
[[keybindings]]
key = "Plus"
modifiers = []
action = "ZoomTiling"
args = { delta = 1 }

[[keybindings]]
key = "Minus"
modifiers = []
action = "ZoomTiling"
args = { delta = -1 }
"#;
        let cfg: Config = toml::from_str(toml_text).unwrap();
        let map = cfg.build_keybinding_map().unwrap();
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn build_keybinding_map_defaults() {
        let cfg = Config::default();
        let map = cfg.build_keybinding_map().unwrap();
        assert!(!map.is_empty());
    }

    #[test]
    fn parse_key_roundtrip() {
        assert!(parse_key("ArrowRight").is_some());
        assert!(parse_key("Q").is_some());
        assert!(parse_key("NotAKey").is_none());
    }
}
