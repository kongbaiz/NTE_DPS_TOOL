use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CONFIG_DIRECTORY: &str = "NTE DPS Tool";
const CONFIG_FILENAME: &str = "config.json";
pub const WINDOW_SCALE_MIN: f32 = 0.7;
pub const WINDOW_SCALE_MAX: f32 = 1.5;

const PASSTHROUGH_HOTKEYS: [PassthroughHotkey; 4] = [
    PassthroughHotkey::Home,
    PassthroughHotkey::Insert,
    PassthroughHotkey::F8,
    PassthroughHotkey::F9,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughHotkey {
    #[default]
    Home,
    Insert,
    F8,
    F9,
}

impl PassthroughHotkey {
    pub fn all() -> &'static [Self] {
        &PASSTHROUGH_HOTKEYS
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Insert => "Insert",
            Self::F8 => "F8",
            Self::F9 => "F9",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub opacity: f32,
    pub dark_mode: bool,
    pub always_on_top: bool,
    pub server_damage_calibration: bool,
    pub passthrough_hotkey: PassthroughHotkey,
    pub main_window_scale: f32,
    pub abyss_window_scale: f32,
    pub hit_detail_window_scale: f32,
    pub team_hit_detail_window_scale: f32,
    pub console_window_scale: f32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            opacity: 0.92,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
            passthrough_hotkey: PassthroughHotkey::default(),
            main_window_scale: 1.0,
            abyss_window_scale: 1.0,
            hit_detail_window_scale: 1.0,
            team_hit_detail_window_scale: 1.0,
            console_window_scale: 1.0,
        }
    }
}

impl UiConfig {
    pub fn sanitized(mut self) -> Self {
        self.opacity = if self.opacity.is_finite() {
            self.opacity.clamp(0.35, 1.0)
        } else {
            Self::default().opacity
        };
        self.main_window_scale = sanitized_window_scale(self.main_window_scale);
        self.abyss_window_scale = sanitized_window_scale(self.abyss_window_scale);
        self.hit_detail_window_scale = sanitized_window_scale(self.hit_detail_window_scale);
        self.team_hit_detail_window_scale =
            sanitized_window_scale(self.team_hit_detail_window_scale);
        self.console_window_scale = sanitized_window_scale(self.console_window_scale);
        self
    }
}

fn sanitized_window_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(WINDOW_SCALE_MIN, WINDOW_SCALE_MAX)
    } else {
        1.0
    }
}

pub fn config_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIRECTORY)
        .join(CONFIG_FILENAME)
}

pub fn load() -> (UiConfig, Option<String>) {
    let path = config_path();
    if !path.is_file() {
        let config = UiConfig::default();
        let warning = save(&path, &config)
            .err()
            .map(|error| format!("默认 UI 配置创建失败（{}）：{error}", path.display()));
        return (config, warning);
    }
    match fs::read_to_string(&path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<UiConfig>(&text).map_err(|error| error.to_string()))
    {
        Ok(config) => (config.sanitized(), None),
        Err(error) => (
            UiConfig::default(),
            Some(format!("UI 配置加载失败（{}）：{error}", path.display())),
        ),
    }
}

pub fn save(path: &Path, config: &UiConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&config.clone().sanitized())
        .map_err(|error| error.to_string())?;
    // Atomic write so a crash mid-write cannot leave a truncated/corrupt config.json.
    crate::io_util::atomic_write_text(path, &format!("{text}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_invalid_opacity() {
        assert_eq!(
            UiConfig {
                opacity: 2.0,
                ..UiConfig::default()
            }
            .sanitized()
            .opacity,
            1.0
        );
        assert_eq!(
            UiConfig {
                opacity: f32::NAN,
                ..UiConfig::default()
            }
            .sanitized()
            .opacity,
            UiConfig::default().opacity
        );
    }

    #[test]
    fn sanitizes_invalid_window_scale() {
        assert_eq!(
            UiConfig {
                main_window_scale: 2.0,
                ..UiConfig::default()
            }
            .sanitized()
            .main_window_scale,
            WINDOW_SCALE_MAX
        );
        assert_eq!(
            UiConfig {
                console_window_scale: f32::NAN,
                ..UiConfig::default()
            }
            .sanitized()
            .console_window_scale,
            1.0
        );
    }
}
