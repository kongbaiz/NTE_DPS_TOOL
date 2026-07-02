use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::i18n::Language;

const CONFIG_DIRECTORY: &str = "NTE DPS Tool";
const CONFIG_FILENAME: &str = "config.json";
/// Smallest inner size (logical points) each window may be dragged down to. Enforced both when
/// sanitizing a persisted size and at runtime via `with_min_inner_size`, so free resize can never
/// collapse a window below a usable layout. Roughly 0.6–0.7× of each window's base size.
pub const MAIN_WINDOW_MIN_SIZE: [f32; 2] = [420.0, 300.0];
pub const CONSOLE_WINDOW_MIN_SIZE: [f32; 2] = [640.0, 420.0];
pub const HIT_DETAIL_WINDOW_MIN_SIZE: [f32; 2] = [720.0, 480.0];
pub const TEAM_HIT_DETAIL_WINDOW_MIN_SIZE: [f32; 2] = [640.0, 440.0];
pub const ABYSS_WINDOW_MIN_SIZE: [f32; 2] = [680.0, 460.0];
/// Upper bound on a persisted window dimension, guarding against a corrupt config pushing a
/// window off every monitor.
const WINDOW_SIZE_MAX: f32 = 6000.0;
pub const TIMELINE_BUCKET_SECONDS_DEFAULT: f32 = 1.0;
pub const TIMELINE_BUCKET_SECONDS_MIN: f32 = 0.2;
pub const TIMELINE_BUCKET_SECONDS_MAX: f32 = 10.0;

const PASSTHROUGH_HOTKEYS: [PassthroughHotkey; 4] = [
    PassthroughHotkey::Home,
    PassthroughHotkey::Insert,
    PassthroughHotkey::F8,
    PassthroughHotkey::F9,
];
const DPS_TIME_MODES: [DpsTimeMode; 2] = [DpsTimeMode::TimeStopAdjusted, DpsTimeMode::RealTime];
const TIMELINE_DPS_VIEW_MODES: [TimelineDpsViewMode; 2] =
    [TimelineDpsViewMode::Team, TimelineDpsViewMode::Characters];

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DpsTimeMode {
    #[default]
    TimeStopAdjusted,
    RealTime,
}

impl DpsTimeMode {
    pub fn all() -> &'static [Self] {
        &DPS_TIME_MODES
    }

    /// English key; wrap with [`crate::storage::i18n::t`] at the display site.
    pub fn label(self) -> &'static str {
        match self {
            Self::TimeStopAdjusted => "Exclude Time Stop",
            Self::RealTime => "Real Time",
        }
    }

    /// English key; wrap with [`crate::storage::i18n::t`] at the display site.
    pub fn description(self) -> &'static str {
        match self {
            Self::TimeStopAdjusted => "Output time is not counted during ultimate/extra time-stop",
            Self::RealTime => "Output time accrues over the capture time span",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineDpsViewMode {
    #[default]
    Team,
    Characters,
}

impl TimelineDpsViewMode {
    pub fn all() -> &'static [Self] {
        &TIMELINE_DPS_VIEW_MODES
    }

    /// English key; wrap with [`crate::storage::i18n::t`] at the display site.
    pub fn label(self) -> &'static str {
        match self {
            Self::Team => "Whole Team",
            Self::Characters => "By Character",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HudConfig {
    pub show_title: bool,
    pub show_team_dps: bool,
    pub show_duration: bool,
    pub show_total_damage: bool,
    pub show_character_rows: bool,
    pub show_damage_taken: bool,
    pub show_abyss_half: bool,
    pub show_passthrough_state: bool,
    pub show_mini_timeline: bool,
}

impl Default for HudConfig {
    fn default() -> Self {
        Self {
            show_title: false,
            show_team_dps: true,
            show_duration: true,
            show_total_damage: true,
            show_character_rows: true,
            show_damage_taken: false,
            show_abyss_half: false,
            show_passthrough_state: false,
            show_mini_timeline: false,
        }
    }
}

impl HudConfig {
    /// Pared-down overlay: just team DPS and a short character ranking. Pairs
    /// with [`Self::default`] ("标准") and [`Self::detailed`] ("详细") as the
    /// one-click HUD presets in settings.
    pub fn minimal() -> Self {
        Self {
            show_title: false,
            show_team_dps: true,
            show_duration: false,
            show_total_damage: false,
            show_character_rows: true,
            show_damage_taken: false,
            show_abyss_half: false,
            show_passthrough_state: false,
            show_mini_timeline: false,
        }
    }

    /// Everything on, for a full diagnostic readout.
    pub fn detailed() -> Self {
        Self {
            show_title: true,
            show_team_dps: true,
            show_duration: true,
            show_total_damage: true,
            show_character_rows: true,
            show_damage_taken: true,
            show_abyss_half: true,
            show_passthrough_state: true,
            show_mini_timeline: true,
        }
    }

    /// Retained as the config-sanitize hook even though no field currently needs
    /// clamping, so callers (and `UiConfig::sanitized`) stay stable.
    pub fn sanitized(self) -> Self {
        self
    }

    pub fn has_summary_row(&self) -> bool {
        self.show_team_dps || self.show_duration || self.show_total_damage || self.show_damage_taken
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Active UI language. Absent in older configs → defaults to Simplified Chinese
    /// (the historical UI language) so upgrades are not disrupted.
    pub language: Language,
    pub opacity: f32,
    pub dark_mode: bool,
    pub always_on_top: bool,
    pub server_damage_calibration: bool,
    /// Manual capture-NIC override (the Npcap device `name`, e.g. `\Device\NPF_{GUID}`). `None`
    /// keeps automatic detection; `Some(name)` pins capture to that interface as a VPN fallback.
    pub manual_capture_device: Option<String>,
    pub dps_time_mode: DpsTimeMode,
    pub timeline_bucket_seconds: f32,
    pub timeline_dps_view_mode: TimelineDpsViewMode,
    pub hud: HudConfig,
    pub passthrough_hotkey: PassthroughHotkey,
    /// Last inner size (logical points) each window was dragged to, restored on the next launch.
    /// Absent (older configs, or the retired `*_window_scale` keys) → the window opens at its base
    /// size. Replaces the removed fixed-ratio `−／＋` scale.
    #[serde(default)]
    pub main_window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub abyss_window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub hit_detail_window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub team_hit_detail_window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub console_window_size: Option<[f32; 2]>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: Language::default(),
            opacity: 0.92,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
            manual_capture_device: None,
            dps_time_mode: DpsTimeMode::default(),
            timeline_bucket_seconds: TIMELINE_BUCKET_SECONDS_DEFAULT,
            timeline_dps_view_mode: TimelineDpsViewMode::default(),
            hud: HudConfig::default(),
            passthrough_hotkey: PassthroughHotkey::default(),
            main_window_size: None,
            abyss_window_size: None,
            hit_detail_window_size: None,
            team_hit_detail_window_size: None,
            console_window_size: None,
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
        self.main_window_size = sanitize_window_size(self.main_window_size, MAIN_WINDOW_MIN_SIZE);
        self.abyss_window_size =
            sanitize_window_size(self.abyss_window_size, ABYSS_WINDOW_MIN_SIZE);
        self.hit_detail_window_size =
            sanitize_window_size(self.hit_detail_window_size, HIT_DETAIL_WINDOW_MIN_SIZE);
        self.team_hit_detail_window_size = sanitize_window_size(
            self.team_hit_detail_window_size,
            TEAM_HIT_DETAIL_WINDOW_MIN_SIZE,
        );
        self.console_window_size =
            sanitize_window_size(self.console_window_size, CONSOLE_WINDOW_MIN_SIZE);
        self.timeline_bucket_seconds =
            sanitize_timeline_bucket_seconds(self.timeline_bucket_seconds);
        self.manual_capture_device = self
            .manual_capture_device
            .take()
            .filter(|name| !name.trim().is_empty());
        self.hud = self.hud.sanitized();
        self
    }
}

pub fn sanitize_timeline_bucket_seconds(seconds: f32) -> f32 {
    if seconds.is_finite() {
        seconds.clamp(TIMELINE_BUCKET_SECONDS_MIN, TIMELINE_BUCKET_SECONDS_MAX)
    } else {
        TIMELINE_BUCKET_SECONDS_DEFAULT
    }
}

/// Clamps a persisted window size to `[min, WINDOW_SIZE_MAX]` per axis. A non-finite or absent
/// size becomes `None`, letting the caller fall back to the window's base size.
fn sanitize_window_size(size: Option<[f32; 2]>, min: [f32; 2]) -> Option<[f32; 2]> {
    let [width, height] = size?;
    if !width.is_finite() || !height.is_finite() {
        return None;
    }
    Some([
        width.clamp(min[0], WINDOW_SIZE_MAX),
        height.clamp(min[1], WINDOW_SIZE_MAX),
    ])
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
        let warning = save(&path, &config).err().map(|error| {
            crate::storage::i18n::tf(
                "Failed to create default UI config ({}): {}",
                &[&path.display().to_string(), &error],
            )
        });
        return (config, warning);
    }
    match fs::read_to_string(&path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<UiConfig>(&text).map_err(|error| error.to_string()))
    {
        Ok(config) => (config.sanitized(), None),
        Err(error) => (
            UiConfig::default(),
            Some(crate::storage::i18n::tf(
                "Failed to load UI config ({}): {}",
                &[&path.display().to_string(), &error],
            )),
        ),
    }
}

pub fn save(path: &Path, config: &UiConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&config.clone().sanitized())
        .map_err(|error| error.to_string())?;
    // Atomic write so a crash mid-write cannot leave a truncated/corrupt config.json.
    crate::storage::io_util::atomic_write_text(path, &format!("{text}\n"))
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
    fn sanitizes_invalid_window_size() {
        // Below the per-window minimum is clamped up to it.
        assert_eq!(
            UiConfig {
                main_window_size: Some([10.0, 10.0]),
                ..UiConfig::default()
            }
            .sanitized()
            .main_window_size,
            Some(MAIN_WINDOW_MIN_SIZE)
        );
        // Absurdly large is clamped down to the ceiling.
        assert_eq!(
            UiConfig {
                console_window_size: Some([99999.0, 99999.0]),
                ..UiConfig::default()
            }
            .sanitized()
            .console_window_size,
            Some([WINDOW_SIZE_MAX, WINDOW_SIZE_MAX])
        );
        // Non-finite falls back to "use the base size".
        assert_eq!(
            UiConfig {
                console_window_size: Some([f32::NAN, 640.0]),
                ..UiConfig::default()
            }
            .sanitized()
            .console_window_size,
            None
        );
    }

    #[test]
    fn sanitizes_invalid_timeline_bucket_seconds() {
        assert_eq!(
            UiConfig {
                timeline_bucket_seconds: 0.05,
                ..UiConfig::default()
            }
            .sanitized()
            .timeline_bucket_seconds,
            TIMELINE_BUCKET_SECONDS_MIN
        );
        assert_eq!(
            UiConfig {
                timeline_bucket_seconds: 99.0,
                ..UiConfig::default()
            }
            .sanitized()
            .timeline_bucket_seconds,
            TIMELINE_BUCKET_SECONDS_MAX
        );
        assert_eq!(
            UiConfig {
                timeline_bucket_seconds: f32::NAN,
                ..UiConfig::default()
            }
            .sanitized()
            .timeline_bucket_seconds,
            TIMELINE_BUCKET_SECONDS_DEFAULT
        );
    }

    #[test]
    fn sanitizes_blank_manual_capture_device() {
        assert_eq!(
            UiConfig {
                manual_capture_device: Some("   ".to_owned()),
                ..UiConfig::default()
            }
            .sanitized()
            .manual_capture_device,
            None
        );
        assert_eq!(
            UiConfig {
                manual_capture_device: Some(r"\Device\NPF_{abc}".to_owned()),
                ..UiConfig::default()
            }
            .sanitized()
            .manual_capture_device,
            Some(r"\Device\NPF_{abc}".to_owned())
        );
    }

    #[test]
    fn hud_presets_are_distinct() {
        assert_ne!(HudConfig::minimal(), HudConfig::default());
        assert_ne!(HudConfig::detailed(), HudConfig::default());
        assert!(HudConfig::detailed().show_mini_timeline);
        assert!(!HudConfig::minimal().show_total_damage);
    }
}
