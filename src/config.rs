use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CONFIG_DIRECTORY: &str = "NTE DPS Tool";
const CONFIG_FILENAME: &str = "config.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub version: u32,
    pub opacity: f32,
    pub dark_mode: bool,
    pub always_on_top: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            version: 1,
            opacity: 0.92,
            dark_mode: false,
            always_on_top: true,
        }
    }
}

impl UiConfig {
    pub fn sanitized(mut self) -> Self {
        self.version = 1;
        self.opacity = if self.opacity.is_finite() {
            self.opacity.clamp(0.35, 1.0)
        } else {
            Self::default().opacity
        };
        self
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string_pretty(&config.clone().sanitized())
        .map_err(|error| error.to_string())?;
    fs::write(path, format!("{text}\n")).map_err(|error| error.to_string())
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
}
