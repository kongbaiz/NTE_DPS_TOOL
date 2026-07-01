use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Color32, RichText, Stroke};

use crate::engine::abyss_data::{
    AbyssFloor, AbyssMonsterDataset, AbyssMonsterEntry, abyss_line_hp_total,
    abyss_monster_total_hp, line_hp_by_wave, predict_wave_clear_times,
    required_dps_for_target_time,
};
use crate::engine::capture::{
    CaptureDevice, CaptureHandle, RawCaptureBuffer, import_capture_json, import_pcapng,
    list_devices, start_capture,
};
use crate::engine::model::{
    AbyssEvent, AbyssHalf, COMBAT_SEGMENT_GAP_SECONDS, CaptureQualitySource, CaptureQualitySummary,
    CharacterInfo, CharacterStats, CombatSegment, CombatSessionAbyssHalfSummary,
    CombatSessionCharacterSummary, CombatSessionSkillSummary, CombatState, EngineEvent,
    HitDirectionSummary, PartyCombatState, SkillBreakdown, SkillBreakdownRow,
    TEAM_DPS_EXPORT_VERSION, TEAM_DPS_MAX_MEMBERS, TeamDps, TeamDpsExport, TeamDpsMember,
    TimelineMarkerKind, TimelineSeries, summarize_combat_segments, summarize_hit_directions,
};
use crate::engine::parser::{CHARACTER_DATA_PATH, find_data_file, load_characters};
use crate::platform::file_drop::NativeFileDrop;
use crate::platform::hotkey::{HotkeyEvent, HotkeyHandle};
use crate::platform::network::{GameNetwork, detect_game_device, detect_game_network};
use crate::platform::window_attributes::{
    WindowAttributeConfig, apply_rounding_to_process_windows, apply_window_attributes,
    clear_process_windows_topmost, restore_visible_process_windows_topmost, set_window_topmost,
};
use crate::storage::capture_logs::{self, CaptureLogStats};
use crate::storage::config::{
    self, DpsTimeMode, HudConfig, PassthroughHotkey, TIMELINE_BUCKET_SECONDS_MAX,
    TIMELINE_BUCKET_SECONDS_MIN, TimelineDpsViewMode, UiConfig,
};
use crate::storage::history::{self, HistoryComparison, HistoryRecord};
use crate::storage::i18n::{self, Language, t, tf};
use crate::storage::io_util::{atomic_write_file, atomic_write_text};
use crate::storage::resource::{read_resource_bytes, read_resource_text};
use crate::support::character_editor::{
    CHARACTER_ATTRIBUTES, CharacterEditForm, CharacterEditorState, json_string_field,
};
use crate::support::diagnostics::{
    DiagnosticReport, DiagnosticSnapshot, DiagnosticStatus, run_capture_diagnostics,
};
use crate::support::encrypted_ini::{
    EncryptedIniKey, EncryptedIniRecord, encrypt_encrypted_ini_records,
    encrypted_ini_search_matches, encrypted_ini_text_fingerprint, parse_encrypted_ini_text,
};
use crate::support::resource_audit::{
    ResourceAuditCategory, ResourceAuditItem, ResourceAuditSeverity, ResourceAuditSummary,
    audit_runtime_resources,
};

const MAX_UI_EVENTS_PER_FRAME: usize = 2_048;
const MAX_UI_EVENTS_WHILE_SCROLLING: usize = 256;
const UI_EVENT_BUDGET: Duration = Duration::from_millis(4);
const DETAIL_CACHE_REFRESH_DELAY: Duration = Duration::from_millis(200);
const MAX_PAUSED_EVENTS: usize = 50_000;
const MAX_ENGINE_QUEUE_BACKLOG: usize = 20_000;
/// Absolute ceiling on the engine→UI queue. Best-effort shedding within the per-frame budget is
/// skipped while a detail list is scrolling, so without this the unbounded channel could grow
/// indefinitely under a sustained flood. This cap is enforced every frame regardless of scrolling
/// or pause state so memory stays bounded.
const MAX_ENGINE_QUEUE_HARD_CAP: usize = 100_000;
const MAX_DETAIL_HITS: usize = 10_000;
const DETAIL_HIT_ROW_HEIGHT: f32 = 40.0;
const MAIN_TITLE_BAR_HEIGHT: f32 = 40.0;
const MAIN_CONTROLS_SINGLE_ROW_HEIGHT: f32 = 34.0;
const TITLE_BAR_BUTTON_SIZE: egui::Vec2 = egui::vec2(28.0, 28.0);
const TITLE_BAR_TOGGLE_SIZE: egui::Vec2 = egui::vec2(64.0, 28.0);
/// Default inner size each window opens at before the user has dragged it (and no persisted size
/// exists). `main` is also used by `main.rs` for the initial root viewport size. Windows are now
/// freely resizable from their edges (`window_resize_grips`); these are just the starting sizes.
pub(crate) const MAIN_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(600.0, 420.0);
const ABYSS_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(1040.0, 720.0);
const HIT_DETAIL_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(1120.0, 760.0);
const TEAM_HIT_DETAIL_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(980.0, 660.0);
const CONSOLE_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(980.0, 640.0);
const PASSTHROUGH_HOTKEY_COMBO_WIDTH: f32 = 150.0;
const CHARACTER_ATTRIBUTE_COMBO_WIDTH: f32 = 150.0;
const CHARACTER_EDITOR_CARD_HEIGHT: f32 = 68.0;
const CHARACTER_EDITOR_AVATAR_SIZE: f32 = 48.0;
/// Width the HUD window shrinks to; height is computed per row count so the
/// window hugs the readout with no empty translucent area.
const HUD_WINDOW_WIDTH: f32 = 380.0;
const UI_CONFIG_SAVE_DELAY: Duration = Duration::from_millis(350);
const UI_CONFIG_SAVE_RETRY_DELAY: Duration = Duration::from_secs(2);
const STATUS_TOAST_DURATION: Duration = Duration::from_secs(4);
const ABYSS_STAT_NAMES_ZH_CN_PATH: &str = "res/data/abyss/monster_stat_names_zh_cn.json";
const INLINE_CONTROL_HEIGHT: f32 = 28.0;
const INLINE_CONTROL_TEXT_SIZE: f32 = 13.0;

#[derive(Clone, Copy)]
pub(crate) enum DebugImportKind {
    Pcapng,
    CaptureJson,
    EncryptedIni,
}

impl DebugImportKind {
    /// English key; wrap with [`crate::storage::i18n::t`] at the display site.
    fn label(self) -> &'static str {
        match self {
            Self::Pcapng => "PCAPNG",
            Self::CaptureJson => "Capture JSON",
            Self::EncryptedIni => "Encrypted INI",
        }
    }
}

struct ActiveImport {
    kind: DebugImportKind,
    path: PathBuf,
    started_at: Instant,
    viewport: egui::ViewportId,
}

#[derive(Clone, Copy)]
struct PendingDebugImport {
    kind: DebugImportKind,
    delay: u8,
    viewport: egui::ViewportId,
}

pub(crate) enum ConfirmationAction {
    StartLive,
    ResetSession,
    ImportPcapng(PathBuf),
    ImportCaptureJson(PathBuf),
    ClearEncryptedIni,
    ReloadEncryptedIni(PathBuf),
    DeleteHistory(String),
    ClearCaptureLogs,
}

#[derive(Clone, Copy)]
pub(crate) enum ErrorAction {
    RefreshNetwork,
    OpenPcapng,
    OpenCaptureJson,
    OpenEncryptedIni,
    OpenTeamDpsImport,
    OpenConsole,
}

/// Tabs of the console window. The first three are user-facing tools promoted
/// out of the old debug panel; `Packets`/`Diagnostics` are genuine capture
/// debugging and only get tab buttons in debug builds (`not(no_debug)`).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ConsoleTab {
    #[default]
    Settings,
    Timeline,
    Skills,
    History,
    Characters,
    EncryptedIni,
    Packets,
    Resources,
    Diagnostics,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum HitDetailFilter {
    #[default]
    All,
    Outgoing,
    Incoming,
    QteType(String),
}

impl HitDetailFilter {
    fn matches(&self, hit: &crate::engine::model::Hit) -> bool {
        match self {
            Self::All => true,
            Self::Outgoing => hit.direction != "incoming",
            Self::Incoming => hit.direction == "incoming",
            Self::QteType(attack_type) => {
                hit.direction != "incoming"
                    && (hit.attack_type.as_deref() == Some(attack_type)
                        || hit.follow_up_attack_type.as_deref() == Some(attack_type))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QteTypeFilterSummary {
    attack_type: String,
    hits: usize,
    damage: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HitDetailSource {
    Global,
    AbyssFirst,
    AbyssSecond,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HitDetailCacheKey {
    source: HitDetailSource,
    char_id: Option<u32>,
    filter: HitDetailFilter,
    skill_filter: String,
    limit: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct CachedHitRow {
    index: usize,
    is_incoming: bool,
    damage: f64,
    char_id: u32,
    hp_fraction: f32,
    timestamp: f64,
    byte_offset: usize,
    bit_shift: u8,
    target_hp_after: f64,
    target_max_hp: f64,
}

#[derive(Default)]
pub(crate) struct HitDetailCache {
    key: Option<HitDetailCacheKey>,
    generation: u64,
    source_len: usize,
    rows: Vec<CachedHitRow>,
    filtered_count: usize,
    max_damage: f64,
    dirty_since: Option<Instant>,
    last_scroll_offset: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkillSummaryCacheKey {
    source: HitDetailSource,
    generation: u64,
    char_id: u32,
}

#[derive(Default)]
struct SkillSummaryCache {
    key: Option<SkillSummaryCacheKey>,
    rows: Vec<SkillDamageSummary>,
    dirty_since: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TimelineCacheKey {
    source: HitDetailSource,
    generation: u64,
    subtract_time_stop: bool,
    bucket_millis: u32,
}

#[derive(Default)]
struct TimelineCache {
    key: Option<TimelineCacheKey>,
    series: TimelineSeries,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkillBreakdownCacheKey {
    source: HitDetailSource,
    generation: u64,
    char_id: Option<u32>,
}

#[derive(Default)]
struct SkillBreakdownCache {
    key: Option<SkillBreakdownCacheKey>,
    breakdown: SkillBreakdown,
}

const ATTRIBUTE_ICON_PATHS: [(&str, &str); 6] = [
    ("灵", "res/images/attributes/UI_avatarbg_Icon_01.png"),
    ("咒", "res/images/attributes/UI_avatarbg_Icon_06.png"),
    ("光", "res/images/attributes/UI_avatarbg_Icon_04.png"),
    ("魂", "res/images/attributes/UI_avatarbg_Icon_05.png"),
    ("暗", "res/images/attributes/UI_avatarbg_Icon_03.png"),
    ("相", "res/images/attributes/UI_avatarbg_Icon_02.png"),
];
const DAMAGE_DIGIT_IMAGE_DIR: &str = "res/images/font/tiaozi1";
const MONSTER_IMAGE_DIR: &str = "res/images/monsters";
const REACTION_TEXT_IMAGE_COUNT: u8 = 8;
// The `_L`/`_H`/`_Z` "non-trigger side" reaction digit sets (Guangling_L,
// Anhun_H, Zhouan_Z, lingzhou_Z) were removed along with their PNGs: reactions
// now always render the trigger-side series (see mixed_damage_digit_key), so
// those sets had no remaining users.
const DAMAGE_DIGIT_TEXTURE_SETS: [(&str, &str); 16] = [
    ("灵", "ling"),
    ("咒", "zhou"),
    ("光", "guang"),
    ("魂", "hun"),
    ("暗", "an"),
    ("相", "xiang"),
    ("物理", "wuli"),
    ("真实", "zhenshi"),
    ("Guangling_G", "Guangling_G"),
    ("Guangxiang_G", "Guangxiang_G"),
    ("Guangxiang_X", "Guangxiang_X"),
    ("Hunxiang_H", "Hunxiang_H"),
    ("Hunxiang_X", "Hunxiang_X"),
    ("Anhun_A", "Anhun_A"),
    ("Zhouan_A", "Zhouan_A"),
    ("lingzhou_L", "lingzhou_L"),
];

#[derive(Default)]
pub(crate) struct EncryptedIniLayoutCache {
    key: Option<EncryptedIniLayoutCacheKey>,
    galley: Option<Arc<egui::Galley>>,
}

impl EncryptedIniLayoutCache {
    fn clear(&mut self) {
        self.key = None;
        self.galley = None;
    }
}

#[derive(Clone, PartialEq, Eq)]
struct EncryptedIniLayoutCacheKey {
    text_len: usize,
    text_hash: u64,
    query: String,
    current_match_byte: Option<usize>,
    dark_mode: bool,
    text_color: Color32,
}

pub(crate) struct EncryptedIniLayoutRequest<'a> {
    text: &'a str,
    query: &'a str,
    matches: &'a [usize],
    current_match_byte: Option<usize>,
    wrap_width: f32,
    dark_mode: bool,
}

#[derive(Default)]
struct EncryptedIniEditorState {
    path: Option<PathBuf>,
    key: EncryptedIniKey,
    plaintext: String,
    search: String,
    search_match: Option<usize>,
    search_matches: Vec<usize>,
    search_cache_query: String,
    search_matches_dirty: bool,
    original_key: EncryptedIniKey,
    original_plaintext: String,
    records: Vec<EncryptedIniRecord>,
    line_ending: String,
    final_newline: bool,
    dirty: bool,
    message: String,
    layout_cache: EncryptedIniLayoutCache,
}

impl EncryptedIniEditorState {
    fn load(path: PathBuf) -> Result<Self, String> {
        let encrypted = std::fs::read_to_string(&path).map_err(|error| {
            tf(
                "Cannot read {}: {}",
                &[&path.display().to_string(), &error.to_string()],
            )
        })?;
        let (key, plaintext, records, line_ending, final_newline) =
            parse_encrypted_ini_text(&encrypted)?;
        let encrypted_lines = records.len();
        Ok(Self {
            path: Some(path),
            key,
            original_key: key,
            original_plaintext: plaintext.clone(),
            records,
            line_ending,
            final_newline,
            plaintext,
            search: String::new(),
            search_match: None,
            search_matches: Vec::new(),
            search_cache_query: String::new(),
            search_matches_dirty: false,
            dirty: false,
            message: tf(
                "Parsed {} lines of ciphertext using the {} key",
                &[&encrypted_lines.to_string(), &t(key.label())],
            ),
            layout_cache: EncryptedIniLayoutCache::default(),
        })
    }

    fn display_path(&self) -> String {
        self.path
            .as_ref()
            .map_or_else(|| t("No file open"), |path| path.display().to_string())
    }

    fn refresh_search_matches(&mut self) {
        if !self.search_matches_dirty && self.search_cache_query == self.search {
            return;
        }
        self.search_matches = encrypted_ini_search_matches(&self.plaintext, &self.search);
        self.search_cache_query = self.search.clone();
        self.search_matches_dirty = false;
        if self
            .search_match
            .is_some_and(|index| index >= self.search_matches.len())
        {
            self.search_match = None;
        }
    }
}

#[derive(Default)]
pub(crate) struct AbyssOverviewState {
    dataset: Option<AbyssMonsterDataset>,
    stat_display_names: HashMap<String, String>,
    load_error: Option<String>,
    selected_season: Option<u32>,
    selected_floor: Option<u32>,
    selected_monster_pack_id: Option<String>,
    expanded_season: Option<u32>,
    search: String,
    // Teams assigned to the upper (上行线) and lower (下行线) lines. Predicted
    // clear time for a line = that line's total HP / team.dps.
    upper_team: Option<TeamDps>,
    lower_team: Option<TeamDps>,
    upper_target_seconds: f64,
    lower_target_seconds: f64,
}

fn load_abyss_stat_display_names() -> HashMap<String, String> {
    let Some(path) = find_data_file(Path::new(ABYSS_STAT_NAMES_ZH_CN_PATH)) else {
        return HashMap::new();
    };
    let Ok(text) = read_resource_text(&path) else {
        return HashMap::new();
    };
    let Ok(names) = serde_json::from_str::<HashMap<String, String>>(&text) else {
        return HashMap::new();
    };

    let mut display_names = HashMap::with_capacity(names.len() * 2);
    for (key, value) in names {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        display_names.insert(key.to_ascii_lowercase(), value.to_owned());
        display_names.insert(key, value.to_owned());
    }
    display_names
}

impl AbyssOverviewState {
    /// Every monster id referenced by the loaded dataset, used to drive the
    /// background monster-portrait loader without sharing the whole state.
    fn monster_ids(&self) -> Vec<String> {
        let Some(dataset) = &self.dataset else {
            return Vec::new();
        };
        dataset
            .seasons
            .iter()
            .flat_map(|season| season.floors.iter())
            .flat_map(|floor| floor.monsters.iter())
            .map(|monster| monster.monster_id.clone())
            .collect()
    }

    fn load() -> Self {
        let stat_display_names = load_abyss_stat_display_names();
        match AbyssMonsterDataset::load() {
            Ok(dataset) => {
                let first = dataset.first_floor_key();
                Self {
                    dataset: Some(dataset),
                    stat_display_names,
                    load_error: None,
                    selected_season: first.map(|(season, _)| season),
                    selected_floor: first.map(|(_, floor)| floor),
                    selected_monster_pack_id: None,
                    expanded_season: first.map(|(season, _)| season),
                    search: String::new(),
                    upper_team: None,
                    lower_team: None,
                    upper_target_seconds: 90.0,
                    lower_target_seconds: 90.0,
                }
            }
            Err(error) => Self {
                load_error: Some(error.to_string()),
                stat_display_names,
                ..Default::default()
            },
        }
    }

    fn reload(&mut self) {
        // Reloading only refreshes the monster dataset; keep the user's search
        // and imported prediction teams.
        let search = self.search.clone();
        let upper_team = self.upper_team.take();
        let lower_team = self.lower_team.take();
        let upper_target_seconds = self.upper_target_seconds;
        let lower_target_seconds = self.lower_target_seconds;
        *self = Self::load();
        self.search = search;
        self.upper_team = upper_team;
        self.lower_team = lower_team;
        self.upper_target_seconds = upper_target_seconds;
        self.lower_target_seconds = lower_target_seconds;
    }

    fn swap_teams(&mut self) {
        std::mem::swap(&mut self.upper_team, &mut self.lower_team);
    }

    fn ensure_selection(&mut self) {
        let Some(dataset) = &self.dataset else {
            return;
        };
        let valid = self
            .selected_season
            .zip(self.selected_floor)
            .is_some_and(|(season, floor)| dataset.floor(season, floor).is_some());
        if !valid && let Some((season, floor)) = dataset.first_floor_key() {
            self.selected_season = Some(season);
            self.selected_floor = Some(floor);
        }
        if self
            .expanded_season
            .is_none_or(|season| dataset.season(season).is_none())
        {
            self.expanded_season = self.selected_season;
        }
        if let Some(pack_id) = &self.selected_monster_pack_id
            && dataset.monster(pack_id).is_none()
        {
            self.selected_monster_pack_id = None;
        }
    }
}

#[derive(Default)]
struct HistoryState {
    records: Vec<HistoryRecord>,
    selected_id: Option<String>,
    compare_left_id: Option<String>,
    compare_right_id: Option<String>,
    skipped_files: usize,
    message: String,
}

impl HistoryState {
    fn load() -> Self {
        let result = history::load_history();
        let mut state = Self {
            records: result.records,
            skipped_files: result.skipped_files,
            ..Default::default()
        };
        state.ensure_selection();
        state
    }

    fn reload(&mut self) {
        let selected_id = self.selected_id.clone();
        let compare_left_id = self.compare_left_id.clone();
        let compare_right_id = self.compare_right_id.clone();
        let result = history::load_history();
        self.records = result.records;
        self.skipped_files = result.skipped_files;
        self.selected_id = selected_id;
        self.compare_left_id = compare_left_id;
        self.compare_right_id = compare_right_id;
        self.ensure_selection();
    }

    fn ensure_selection(&mut self) {
        if self
            .selected_id
            .as_deref()
            .is_none_or(|id| !self.records.iter().any(|record| record.id == id))
        {
            self.selected_id = self.records.first().map(|record| record.id.clone());
        }
        if self
            .compare_left_id
            .as_deref()
            .is_none_or(|id| !self.records.iter().any(|record| record.id == id))
        {
            self.compare_left_id = self.records.first().map(|record| record.id.clone());
        }
        if self
            .compare_right_id
            .as_deref()
            .is_none_or(|id| !self.records.iter().any(|record| record.id == id))
        {
            self.compare_right_id = self.records.get(1).map(|record| record.id.clone());
        }
    }

    fn selected_record(&self) -> Option<&HistoryRecord> {
        let selected_id = self.selected_id.as_deref()?;
        self.records.iter().find(|record| record.id == selected_id)
    }

    fn compare_records(&self) -> Option<(&HistoryRecord, &HistoryRecord, HistoryComparison)> {
        let left_id = self.compare_left_id.as_deref()?;
        let right_id = self.compare_right_id.as_deref()?;
        if left_id == right_id {
            return None;
        }
        let left = self.records.iter().find(|record| record.id == left_id)?;
        let right = self.records.iter().find(|record| record.id == right_id)?;
        Some((left, right, history::compare_records(left, right)))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ResourceAuditSeverityFilter {
    #[default]
    All,
    Error,
    Warning,
}

impl ResourceAuditSeverityFilter {
    /// English key; wrap with [`crate::storage::i18n::t`] at the display site.
    fn label(self) -> &'static str {
        match self {
            Self::All => "All Levels",
            Self::Error => "Errors Only",
            Self::Warning => "Warnings Only",
        }
    }

    fn matches(self, severity: ResourceAuditSeverity) -> bool {
        match self {
            Self::All => true,
            Self::Error => severity == ResourceAuditSeverity::Error,
            Self::Warning => severity == ResourceAuditSeverity::Warning,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ResourceAuditCategoryFilter {
    #[default]
    All,
    Category(ResourceAuditCategory),
}

impl ResourceAuditCategoryFilter {
    /// English key; wrap with [`crate::storage::i18n::t`] at the display site.
    fn label(self) -> &'static str {
        match self {
            Self::All => "All Categories",
            Self::Category(category) => category.label(),
        }
    }

    fn matches(self, category: ResourceAuditCategory) -> bool {
        match self {
            Self::All => true,
            Self::Category(expected) => category == expected,
        }
    }
}

#[derive(Default)]
struct ResourceAuditState {
    summary: Option<ResourceAuditSummary>,
    loading: bool,
    message: String,
    severity_filter: ResourceAuditSeverityFilter,
    category_filter: ResourceAuditCategoryFilter,
}

struct StatusToast {
    text: String,
    shown_until: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HudSizeKey {
    rows: usize,
    show_title_strip: bool,
    show_status_row: bool,
    config: HudConfig,
}

#[derive(Clone, Copy)]
pub(crate) struct HudSummaryValues {
    total_damage: f64,
    team_dps: f64,
    duration: f64,
    damage_taken: f64,
}

#[derive(Clone, Copy)]
pub(crate) struct HudPaintColors {
    accent: Color32,
    text: Color32,
    muted: Color32,
}

#[derive(Clone, Copy)]
pub(crate) struct HudRowsLayout {
    left: f32,
    right: f32,
    top: f32,
    width: f32,
}

/// A decoded texture set handed from the background loader thread to the UI.
/// Texture decode (the 6 MB of PNGs) runs off the main thread so the window
/// paints immediately; the maps start empty and every draw-site lookup already
/// falls back when a key is missing, so placeholders show until these arrive.
enum TextureLoad {
    Avatars(HashMap<String, egui::TextureHandle>),
    Attributes(HashMap<String, egui::TextureHandle>),
    DamageDigits(HashMap<String, Vec<egui::TextureHandle>>),
    Reactions(HashMap<u8, Vec<egui::TextureHandle>>),
    Monsters(HashMap<String, egui::TextureHandle>),
}

/// Result of the startup capture-environment probe (Npcap device list + the
/// HTGame.exe NIC), computed on a background thread so `DpsApp::new` need not
/// block on device enumeration. `start_live` re-runs this on every capture
/// start, so this only seeds the initial status and device dropdown.
struct DeviceDetection {
    devices: Vec<CaptureDevice>,
    selected_device: usize,
    game_network: Option<GameNetwork>,
    local_ip: String,
    status: String,
    diagnostic: Option<String>,
}

pub struct DpsApp {
    characters: Arc<HashMap<u32, CharacterInfo>>,
    avatar_textures: HashMap<String, egui::TextureHandle>,
    attribute_textures: HashMap<String, egui::TextureHandle>,
    monster_textures: HashMap<String, egui::TextureHandle>,
    damage_digit_textures: HashMap<String, Vec<egui::TextureHandle>>,
    reaction_textures: HashMap<u8, Vec<egui::TextureHandle>>,
    state: CombatState,
    selected_abyss_half: AbyssHalf,
    abyss_compact_mode: bool,
    /// Chrome-less "战斗 HUD" overlay: hides the toolbar/cards and paints a
    /// background-less readout that floats directly on the game (see [`Self::hud_panel`]).
    hud_mode: bool,
    /// Row count and title-strip visibility the HUD window was last sized to.
    /// Drives shrinking the window to hug the HUD and restoring it on exit.
    hud_size_key: Option<HudSizeKey>,
    hud_config: HudConfig,
    abyss_overview: AbyssOverviewState,
    history: HistoryState,
    resource_audit: ResourceAuditState,
    abyss_overview_open: bool,
    abyss_overview_corner_applied: bool,
    hit_detail_char_id: Option<u32>,
    hit_detail_filter: HitDetailFilter,
    hit_detail_skill_filter: String,
    hit_detail_corner_applied: bool,
    team_hit_detail_open: bool,
    team_hit_detail_filter: HitDetailFilter,
    team_hit_detail_corner_applied: bool,
    character_hit_cache: HitDetailCache,
    team_hit_cache: HitDetailCache,
    skill_summary_cache: SkillSummaryCache,
    timeline_cache: TimelineCache,
    skill_breakdown_cache: SkillBreakdownCache,
    selected_timeline_char: Option<u32>,
    selected_skill_breakdown_char: Option<u32>,
    detail_last_scroll_activity: Option<Instant>,
    devices: Vec<CaptureDevice>,
    selected_device: usize,
    /// Manual capture-NIC override (Npcap device `name`). `None` = automatic detection;
    /// `Some(name)` pins capture to that interface as a VPN fallback. Persisted in `UiConfig`.
    manual_capture_device: Option<String>,
    local_ip: String,
    game_network: Option<GameNetwork>,
    filter: String,
    active_capture_filter: Option<String>,
    capture_quality_source: CaptureQualitySource,
    include_incoming: bool,
    server_damage_calibration: bool,
    dps_time_mode: DpsTimeMode,
    timeline_bucket_seconds: f32,
    timeline_dps_view_mode: TimelineDpsViewMode,
    capture: Option<CaptureHandle>,
    raw_capture: Option<RawCaptureBuffer>,
    replay_stop: Option<Arc<AtomicBool>>,
    replay_thread: Option<thread::JoinHandle<()>>,
    sender: Sender<EngineEvent>,
    receiver: Receiver<EngineEvent>,
    resource_audit_sender: Sender<ResourceAuditSummary>,
    resource_audit_receiver: Receiver<ResourceAuditSummary>,
    resource_audit_thread: Option<thread::JoinHandle<()>>,
    diagnostics_sender: Sender<DiagnosticReport>,
    diagnostics_receiver: Receiver<DiagnosticReport>,
    diagnostics_thread: Option<thread::JoinHandle<()>>,
    diagnostics_report: Option<DiagnosticReport>,
    diagnostics_running: bool,
    texture_load_receiver: Receiver<TextureLoad>,
    device_detection_receiver: Receiver<DeviceDetection>,
    awaiting_device_detection: bool,
    /// Cached size/count of `logs/nte_raw_*.pcapng`, scanned lazily for the
    /// capture-file section in settings (never per frame). `None` until first
    /// shown or after a refresh request.
    capture_log_stats: Option<CaptureLogStats>,
    paused_events: VecDeque<EngineEvent>,
    dropped_debug_packets: u64,
    status: String,
    last_status_toast: String,
    status_toast: Option<StatusToast>,
    diagnostic: Option<String>,
    last_error: Option<String>,
    last_error_action: Option<ErrorAction>,
    last_error_viewport: egui::ViewportId,
    console_open: bool,
    console_corner_applied: bool,
    console_tab: ConsoleTab,
    debug_only_hits: bool,
    debug_search: String,
    character_editor: CharacterEditorState,
    encrypted_ini_editor: EncryptedIniEditorState,
    paused: bool,
    /// Active UI language. Mirrors `UiConfig::language`; the settings dropdown writes
    /// it and calls [`crate::storage::i18n::set_language`] to swap the live locale.
    language: Language,
    dark_mode: bool,
    always_on_top: bool,
    mouse_passthrough: bool,
    passthrough_hotkey: PassthroughHotkey,
    opacity: f32,
    applied_opacity: Option<f32>,
    corner_applied_hwnd: Option<isize>,
    // Live inner size (logical points) of each window, updated every frame from the viewport's
    // `screen_rect` while it is open and persisted (debounced) so the window reopens at the size
    // the user last dragged it to. Replaces the retired −／＋ scale factor.
    main_window_size: egui::Vec2,
    abyss_window_size: egui::Vec2,
    hit_detail_window_size: egui::Vec2,
    team_hit_detail_window_size: egui::Vec2,
    console_window_size: egui::Vec2,
    /// Frames to skip main-window size tracking after a programmatic `InnerSize` (HUD exit), while
    /// Windows applies the resize asynchronously and `content_rect` still reports the old HUD size.
    /// Without this, tracking would clobber `main_window_size` with the small HUD size.
    main_size_restore_frames: u8,
    style_dark_mode_applied: Option<bool>,
    opacity_reapply_frames: u8,
    theme_transition_from: Option<Color32>,
    theme_transition_started_at: Option<f64>,
    pending_debug_import: Option<PendingDebugImport>,
    active_import: Option<ActiveImport>,
    pending_confirmation: Option<ConfirmationAction>,
    pending_confirmation_viewport: egui::ViewportId,
    saved_ui_config: UiConfig,
    pending_ui_config: Option<(UiConfig, Instant)>,
    ui_config_path: PathBuf,
    native_file_drop: NativeFileDrop,
    last_dropped_file: Option<(PathBuf, Instant)>,
    hotkey_receiver: Receiver<HotkeyEvent>,
    hotkey: HotkeyHandle,
}

// Submodules carved out of the original monolithic app.rs (Phase 1b).
// Method submodules attach `impl DpsApp` blocks; free-fn submodules are
// re-exported so every submodule and the tests share one flat `app`
// namespace.
mod abyss;
mod chrome;
mod console_view;
mod detail_panels;
mod diagnostics_ui;
mod editor;
mod history_ui;
mod hit_detail;
mod hud;
mod lifecycle;
mod main_view;
mod resources;
mod theme;
mod timeline;

pub(crate) use abyss::*;
pub(crate) use chrome::*;
pub(crate) use diagnostics_ui::*;
pub(crate) use editor::*;
pub(crate) use history_ui::*;
pub(crate) use hit_detail::*;
pub(crate) use hud::*;
pub(crate) use resources::*;
pub(crate) use theme::*;
pub(crate) use timeline::*;

impl eframe::App for DpsApp {
    /// Clear to transparent alpha. HUD mode relies on this so empty pixels
    /// disappear while painted text/images stay opaque; normal mode covers the
    /// clear with opaque panels.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.style_dark_mode_applied != Some(self.dark_mode) {
            configure_style(ctx, self.dark_mode);
            self.style_dark_mode_applied = Some(self.dark_mode);
        }
        self.note_detail_scroll_activity(ctx);
        self.drain_events();
        self.drain_resource_audit();
        self.drain_capture_diagnostics();
        self.drain_texture_loads();
        self.drain_device_detection();
        self.drain_hotkeys(ctx);
        self.process_file_drops(ctx, frame);
        self.process_debug_import_dialog(ctx);
        let force_opacity = self.opacity_reapply_frames > 0;
        apply_window_attributes(
            frame,
            WindowAttributeConfig {
                opacity: self.opacity,
                force_opacity,
                hud_overlay: self.hud_mode,
                passthrough: self.mouse_passthrough,
            },
            &mut self.applied_opacity,
            &mut self.corner_applied_hwnd,
        );
        self.opacity_reapply_frames = self.opacity_reapply_frames.saturating_sub(1);
        if self.capture.is_some() || self.replay_thread.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Shrink the window to hug the HUD on entry (so there's no big translucent
        // rectangle); restore the tracked normal size on exit. These are discrete programmatic
        // `InnerSize` commands, distinct from the interactive edge-drag resize (`window_resize_grips`).
        if self.hud_mode {
            let rows = self.hud_visible_row_count();
            let show_title = !self.mouse_passthrough;
            let show_status_row = self.hud_status_row_visible();
            let size_key = HudSizeKey {
                rows,
                show_title_strip: show_title,
                show_status_row,
                config: self.hud_config.clone(),
            };
            if self.hud_size_key.as_ref() != Some(&size_key) {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(hud_window_size(
                    rows,
                    show_title,
                    show_status_row,
                    &self.hud_config,
                )));
                self.hud_size_key = Some(size_key);
            }
        } else if self.hud_size_key.take().is_some() {
            // Leaving HUD mode: restore the normal window to the size the user last dragged it to.
            // Suppress size tracking until Windows applies the resize, so the transient HUD size is
            // not mistaken for a user drag and written back over `main_window_size`.
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(self.main_window_size));
            self.main_size_restore_frames = 8;
        }
        self.update_status_toast(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // HUD mode replaces the full title bar with a compact strip (drag · 穿透 ·
        // 退出) and strips the panel fill so only the game shows behind it.
        let show_hud_title = self.hud_mode && !self.mouse_passthrough;
        if !self.hud_mode || show_hud_title {
            let title_frame = if self.hud_mode {
                // No margin so the HUD's control rail can span the full window width.
                egui::Frame::new()
            } else {
                egui::Frame::new()
                    .fill(ctx.global_style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(10, 4))
            };
            egui::Panel::top("custom_title_bar")
                .exact_size(if self.hud_mode {
                    24.0
                } else {
                    MAIN_TITLE_BAR_HEIGHT
                })
                .frame(title_frame)
                .show_inside(ui, |ui| {
                    if self.hud_mode {
                        self.hud_title_bar(ui);
                    } else {
                        self.title_bar(ui);
                    }
                });
        }

        let central_fill = if self.hud_mode {
            Color32::TRANSPARENT
        } else {
            shadcn_background(self.dark_mode)
        };
        let central_margin = if self.hud_mode {
            egui::Margin::ZERO
        } else {
            egui::Margin {
                // Match the title bar's 10px side margins so the metric cards and
                // party rows line up with the window controls above and the
                // rightmost card keeps clearance from the window edge.
                left: 10,
                right: 10,
                top: 0,
                bottom: 8,
            }
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(central_fill)
                    .inner_margin(central_margin),
            )
            .show_inside(ui, |ui| {
                if self.hud_mode {
                    self.hud_panel(ui);
                } else {
                    self.animated_controls(ui);
                    if self.replay_thread.is_some() {
                        self.import_loading_content(ui);
                    } else {
                        if self.state.abyss.is_active() {
                            self.abyss_selector(ui);
                        }
                        self.animated_party_content(ui);
                    }
                }
            });

        // Native edge/corner drag-resize for the borderless main window, plus tracking its size so
        // it restores on the next launch. Skipped in HUD mode, where the window auto-hugs the HUD.
        if !self.hud_mode {
            if self.main_size_restore_frames > 0 {
                self.main_size_restore_frames -= 1;
            } else {
                track_window_size(&ctx, &mut self.main_window_size);
            }
            window_resize_grips(&ctx);
        }

        if self.console_open {
            self.console_panel(&ctx);
        }
        if let Some(char_id) = self.hit_detail_char_id {
            self.hit_detail_panel(&ctx, char_id);
        }
        if self.team_hit_detail_open {
            self.team_hit_detail_panel(&ctx);
        }
        if self.abyss_overview_open {
            self.abyss_overview_panel(&ctx);
        }
        if ctx.input(|input| !input.raw.hovered_files.is_empty()) {
            egui::Area::new(egui::Id::new("pcapng_drop_overlay"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(shadcn_card(self.dark_mode))
                        .stroke(Stroke::new(2.0, theme_accent(self.dark_mode)))
                        .inner_margin(egui::Margin::symmetric(28, 20))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(t("Release to import PCAPNG / JSON"))
                                    .size(18.0)
                                    .strong()
                                    .color(theme_accent(self.dark_mode)),
                            );
                        });
                });
        }
        self.show_status_toast(&ctx);
        self.paint_theme_transition(&ctx);
        self.show_viewport_dialogs(&ctx);
        self.persist_ui_config();
        if self.capture.is_none()
            && self.replay_thread.is_none()
            && let Some((_, save_at)) = &self.pending_ui_config
        {
            ctx.request_repaint_after(save_at.saturating_duration_since(Instant::now()));
        }
    }
}

impl Drop for DpsApp {
    fn drop(&mut self) {
        self.persist_ui_config_on_shutdown();
        self.stop_engine();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AbyssOverviewState, DpsApp, HitDetailFilter, QteTypeFilterSummary, UiConfigSavePlan,
        adjusted_cached_index, build_team_dps_export, cached_hit_row, character_color,
        compare_cached_team_hits, damage_digit_key_for_hit, damage_digit_resource_path,
        damage_number_digits_text, fill_missing_character_colors_from_avatars,
        follow_up_damage_digit_key_for_hit, hit_detail_filter_available, hit_type_label,
        is_party_member_row, mixed_damage_digit_key, parse_hex_color, qte_type_filter_label,
        reaction_text_key_for_hit, reaction_text_key_from_trigger_attack_type, resolve_cached_hit,
        skill_display_name, snapshot_team_from_stats, summarize_qte_type_filters,
    };
    use crate::engine::model::{
        CharacterInfo, CharacterStats, CombatSessionSkillSummary, CombatState, Hit, TeamDps,
        TeamDpsMember,
    };
    use crate::storage::config::UiConfig;
    use crate::support::encrypted_ini::{
        EncryptedIniKey, decrypt_encrypted_ini_text, encrypt_aes256_ecb,
        encrypt_encrypted_ini_records, encrypt_encrypted_ini_text, encrypted_ini_search_matches,
        encrypted_ini_text_fingerprint, parse_encrypted_ini_text, pkcs7_pad,
    };
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use eframe::egui;
    use std::collections::{HashMap, VecDeque};
    use std::time::{Duration, Instant};

    fn hit_with_direction(direction: &str) -> Hit {
        Hit {
            timestamp: 0.0,
            char_id: 1,
            char_name: "角色".to_owned(),
            char_known: true,
            damage: 1.0,
            byte_offset: 0,
            bit_shift: 0,
            char_source: "unknown".to_owned(),
            direction: direction.to_owned(),
            target_hp_before: 0.0,
            target_hp_after: 0.0,
            target_max_hp: 0.0,
            target_hp_percent: 0.0,
            target_id: None,
            target_name: None,
            target_context: Vec::new(),
            gameplay_effect_index: None,
            gameplay_effect_name: None,
            ability_name: None,
            damage_name: Some("招式".to_owned()),
            attack_type: None,
            damage_attribute: None,
            follow_up_damage: 0.0,
            follow_up_timestamp: None,
            follow_up_damage_name: None,
            follow_up_attack_type: None,
            follow_up_damage_attribute: None,
        }
    }

    #[test]
    fn cached_index_tracks_front_trimming_during_deferred_refresh() {
        assert_eq!(
            adjusted_cached_index(45_000, 50_000, 50_000, 20),
            Some(44_980)
        );
        assert_eq!(adjusted_cached_index(10, 50_000, 50_000, 20), None);
        assert_eq!(
            adjusted_cached_index(9_000, 10_000, 10_020, 20),
            Some(9_000)
        );
    }

    #[test]
    fn cached_hit_resolves_when_generation_changes_without_appending() {
        let hit = hit_with_direction("outgoing");
        let row = cached_hit_row(0, &hit);
        let mut hits = VecDeque::from([hit]);
        hits[0].follow_up_damage = 921.0;

        let resolved = resolve_cached_hit(&hits, &row, 1, 1)
            .expect("same-index hit should resolve after in-place follow-up update");
        assert_eq!(resolved.follow_up_damage, 921.0);
    }

    #[test]
    fn history_skill_display_prefixes_unknown_descriptions_with_character() {
        let english = CombatSessionSkillSummary {
            name: "GA_Shinku_UltraSkill".to_owned(),
            category: "技能".to_owned(),
            char_id: 1076,
            char_name: "真红".to_owned(),
            damage: 593_779.0,
            hits: 29,
            damage_share_percent: 31.2,
            is_follow_up: false,
        };
        assert_eq!(skill_display_name(&english), "真红 · 大招");

        let chinese = CombatSessionSkillSummary {
            name: "安魂曲流血伤害L".to_owned(),
            char_name: "安魂曲".to_owned(),
            ..english
        };
        assert_eq!(skill_display_name(&chinese), "安魂曲流血伤害L");
    }

    #[test]
    fn team_hit_rows_order_higher_hp_first_within_same_second() {
        let mut high_hp = hit_with_direction("outgoing");
        high_hp.timestamp = 1.9;
        high_hp.target_hp_after = 30_000.0;
        high_hp.target_max_hp = 100_000.0;
        high_hp.target_hp_percent = 30.0;
        let mut low_hp = hit_with_direction("outgoing");
        low_hp.timestamp = 1.1;
        low_hp.target_hp_after = 7_000.0;
        low_hp.target_max_hp = 100_000.0;
        low_hp.target_hp_percent = 7.0;

        let mut rows = [cached_hit_row(0, &low_hp), cached_hit_row(1, &high_hp)];
        rows.sort_by(compare_cached_team_hits);

        assert_eq!(rows[0].target_hp_after, 30_000.0);
        assert_eq!(rows[1].target_hp_after, 7_000.0);
    }

    #[test]
    fn unknown_hit_uses_candidate_output_label() {
        let outgoing_hit = hit_with_direction("outgoing");
        let incoming_hit = hit_with_direction("incoming");
        let unknown_hit = hit_with_direction("unknown");
        let outgoing = hit_type_label(&outgoing_hit);
        let incoming = hit_type_label(&incoming_hit);
        let unknown = hit_type_label(&unknown_hit);
        assert!(!outgoing.is_empty());
        assert!(!incoming.is_empty());
        assert!(!unknown.is_empty());
        assert_ne!(unknown, incoming);
    }

    #[test]
    fn reaction_text_key_only_marks_reaction_trigger_hits() {
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·覆纹"),
            Some(2)
        );
        assert_eq!(reaction_text_key_from_trigger_attack_type("覆纹"), None);
        assert_eq!(reaction_text_key_from_trigger_attack_type("创生花"), None);
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·黯星"),
            Some(3)
        );
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·浊燃"),
            Some(4)
        );
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·浸染"),
            Some(5)
        );
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·延滞"),
            Some(6)
        );
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·盈蓄"),
            Some(7)
        );
        assert_eq!(
            reaction_text_key_from_trigger_attack_type("环合·失谐"),
            Some(8)
        );

        let mut hit = hit_with_direction("outgoing");
        hit.gameplay_effect_name = Some("GE_ActorReaction_1_Damage".to_owned());
        hit.attack_type = Some("创生花".to_owned());
        assert_eq!(reaction_text_key_for_hit(&hit), None);

        hit.attack_type = Some("环合·覆纹".to_owned());
        assert_eq!(reaction_text_key_for_hit(&hit), Some(2));
    }

    #[test]
    fn qte_type_filters_include_only_present_outgoing_qte_types() {
        let mut qte_a = hit_with_direction("outgoing");
        qte_a.attack_type = Some("覆纹".to_owned());
        qte_a.damage = 10.0;
        let mut qte_b = hit_with_direction("unknown");
        qte_b.attack_type = Some("创生花".to_owned());
        qte_b.damage = 20.0;
        let mut incoming_qte = hit_with_direction("incoming");
        incoming_qte.attack_type = Some("覆纹".to_owned());
        let mut entry_qte = hit_with_direction("outgoing");
        entry_qte.attack_type = Some("环合·覆纹".to_owned());
        let mut non_qte = hit_with_direction("outgoing");
        non_qte.attack_type = Some("普攻".to_owned());

        let hits = VecDeque::from([qte_a, qte_b, incoming_qte, entry_qte, non_qte]);
        let summaries = summarize_qte_type_filters(&hits, None);

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].attack_type, "创生花");
        assert_eq!(summaries[0].hits, 1);
        assert_eq!(summaries[0].damage, 20.0);
        assert_eq!(summaries[1].attack_type, "覆纹");
        assert_eq!(summaries[1].hits, 1);
        assert_eq!(summaries[1].damage, 10.0);
        assert!(hit_detail_filter_available(
            &HitDetailFilter::QteType("覆纹".to_owned()),
            &summaries
        ));
        assert!(!hit_detail_filter_available(
            &HitDetailFilter::QteType("黯星".to_owned()),
            &summaries
        ));
    }

    #[test]
    fn qte_type_filters_include_merged_follow_up_damage() {
        let mut source = hit_with_direction("outgoing");
        source.attack_type = Some("Q技能".to_owned());
        source.follow_up_damage = 921.0;
        source.follow_up_attack_type = Some("覆纹".to_owned());
        source.follow_up_damage_attribute = Some("咒".to_owned());

        let hits = VecDeque::from([source]);
        let summaries = summarize_qte_type_filters(&hits, None);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].attack_type, "覆纹");
        assert_eq!(summaries[0].hits, 1);
        assert_eq!(summaries[0].damage, 921.0);
        assert!(HitDetailFilter::QteType("覆纹".to_owned()).matches(&hits[0]));
    }

    #[test]
    fn qte_type_filter_matches_only_that_attack_type() {
        let filter = HitDetailFilter::QteType("覆纹".to_owned());
        let mut matching = hit_with_direction("outgoing");
        matching.attack_type = Some("覆纹".to_owned());
        let mut different_qte = hit_with_direction("outgoing");
        different_qte.attack_type = Some("创生花".to_owned());
        let mut incoming = hit_with_direction("incoming");
        incoming.attack_type = Some("覆纹".to_owned());

        assert!(filter.matches(&matching));
        assert!(!filter.matches(&different_qte));
        assert!(!filter.matches(&incoming));
    }

    #[test]
    fn qte_type_filter_label_shows_damage_and_share() {
        let summary = QteTypeFilterSummary {
            attack_type: "覆纹".to_owned(),
            hits: 42,
            damage: 77_136.0,
        };

        assert_eq!(
            qte_type_filter_label(&summary, 1_928_356.0),
            "覆纹 77,136 · 4.0%"
        );
    }

    #[test]
    fn damage_digit_text_uses_plain_digits_for_image_rendering() {
        assert_eq!(damage_number_digits_text(77_136.2), "77136");
        assert_eq!(damage_number_digits_text(77_136.8), "77137");
        assert_eq!(
            damage_digit_resource_path("guang", 7),
            "res/images/font/tiaozi1/guang_7.png"
        );
    }

    #[test]
    fn damage_digit_key_prefers_special_damage_types() {
        let characters = HashMap::from([(
            1,
            CharacterInfo {
                name_zh: "角色".to_owned(),
                name_en: String::new(),
                color: None,
                avatar: None,
                attribute: Some("灵".to_owned()),
            },
        )]);
        let mut hit = hit_with_direction("outgoing");
        assert_eq!(damage_digit_key_for_hit(&hit, &characters), Some("灵"));

        hit.attack_type = Some("倾陷伤害".to_owned());
        assert_eq!(damage_digit_key_for_hit(&hit, &characters), Some("真实"));

        // 覆纹跳字固定为灵字系（lingzhou_L），即便伤害记给的是咒角色。
        hit.attack_type = Some("覆纹".to_owned());
        hit.damage_attribute = Some("咒".to_owned());
        assert_eq!(
            damage_digit_key_for_hit(&hit, &characters),
            Some("lingzhou_L")
        );

        hit.attack_type = Some("创生花".to_owned());
        hit.damage_attribute = Some("光".to_owned());
        assert_eq!(
            damage_digit_key_for_hit(&hit, &characters),
            Some("Guangling_G")
        );
        hit.attack_type = Some("载具伤害".to_owned());
        hit.damage_attribute = Some("物理".to_owned());
        assert_eq!(damage_digit_key_for_hit(&hit, &characters), Some("物理"));

        hit.direction = "incoming".to_owned();
        hit.attack_type = Some("覆纹".to_owned());
        hit.damage_attribute = Some("咒".to_owned());
        assert_eq!(damage_digit_key_for_hit(&hit, &characters), Some("物理"));

        hit.direction = "outgoing".to_owned();
        hit.follow_up_attack_type = Some("覆纹".to_owned());
        hit.follow_up_damage_attribute = Some("咒".to_owned());
        assert_eq!(follow_up_damage_digit_key_for_hit(&hit), Some("lingzhou_L"));
    }

    #[test]
    fn reaction_burst_digit_key_locks_to_trigger_side_series() {
        // 环合反应伤害本体（不带 "环合·" 前缀）的跳字固定为触发侧属性字系，
        // 无论记给环合双方哪一侧，也无论是否解析出 source_attribute。
        for attribute in [None, Some("光"), Some("灵")] {
            assert_eq!(
                mixed_damage_digit_key("创生花", attribute),
                Some("Guangling_G")
            );
        }
        for attribute in [None, Some("灵"), Some("咒")] {
            assert_eq!(
                mixed_damage_digit_key("覆纹", attribute),
                Some("lingzhou_L")
            );
        }
        for attribute in [None, Some("暗"), Some("魂")] {
            assert_eq!(mixed_damage_digit_key("黯星", attribute), Some("Anhun_A"));
        }
        for attribute in [None, Some("暗"), Some("咒")] {
            assert_eq!(mixed_damage_digit_key("浊燃", attribute), Some("Zhouan_A"));
        }
    }

    #[test]
    fn reaction_trigger_hit_digit_key_uses_caster_attribute() {
        // 触发环合的那一下伤害（"环合·xxx"）走造成伤害角色自身属性字系，
        // mixed_damage_digit_key 返回 None，由调用方回退到 source_attribute。
        for trigger in ["环合·创生", "环合·黯星", "环合·浊燃", "环合·覆纹"] {
            assert_eq!(mixed_damage_digit_key(trigger, Some("灵")), None);
        }
        // 端到端：娜娜莉（灵）触发创生的那一下，跳字应为灵字系而非创生字系。
        let characters = HashMap::from([(
            1010,
            CharacterInfo {
                name_zh: "娜娜莉".to_owned(),
                name_en: String::new(),
                color: None,
                avatar: None,
                attribute: Some("灵".to_owned()),
            },
        )]);
        let mut trigger_hit = hit_with_direction("outgoing");
        trigger_hit.char_id = 1010;
        trigger_hit.attack_type = Some("环合·创生".to_owned());
        assert_eq!(
            damage_digit_key_for_hit(&trigger_hit, &characters),
            Some("灵")
        );
    }

    #[test]
    fn party_member_rows_hide_qte_follow_up_pseudo_characters() {
        let mut real_hit = hit_with_direction("outgoing");
        real_hit.char_id = 1;
        real_hit.attack_type = Some("普攻".to_owned());
        let mut follow_up = hit_with_direction("outgoing");
        follow_up.char_id = 999_999;
        follow_up.char_known = false;
        follow_up.attack_type = Some("覆纹".to_owned());

        let hits = VecDeque::from([real_hit, follow_up]);

        assert!(is_party_member_row(
            &CharacterStats {
                char_id: 1,
                ..Default::default()
            },
            &hits
        ));
        assert!(!is_party_member_row(
            &CharacterStats {
                char_id: 999_999,
                ..Default::default()
            },
            &hits
        ));
    }

    #[test]
    fn team_dps_snapshot_uses_team_duration_and_top_party_members() {
        let stats = [
            CharacterStats {
                char_id: 1,
                name: "一".to_owned(),
                damage: 100.0,
                ..Default::default()
            },
            CharacterStats {
                char_id: 2,
                name: "二".to_owned(),
                damage: 200.0,
                ..Default::default()
            },
            CharacterStats {
                char_id: 3,
                name: "三".to_owned(),
                damage: 300.0,
                ..Default::default()
            },
            CharacterStats {
                char_id: 4,
                name: "四".to_owned(),
                damage: 400.0,
                ..Default::default()
            },
            CharacterStats {
                char_id: 5,
                name: "五".to_owned(),
                damage: 500.0,
                ..Default::default()
            },
            CharacterStats {
                char_id: 6,
                name: "零伤害".to_owned(),
                damage: 0.0,
                ..Default::default()
            },
            CharacterStats {
                char_id: 999_999,
                name: "伪角色".to_owned(),
                damage: 10_000.0,
                ..Default::default()
            },
        ];

        let team = snapshot_team_from_stats(150.0, 10.0, stats.iter()).unwrap();

        assert_eq!(team.dps, 150.0);
        assert_eq!(
            team.members
                .iter()
                .map(|member| member.id)
                .collect::<Vec<_>>(),
            vec![5, 4, 3, 2]
        );
        assert_eq!(
            team.members
                .iter()
                .map(|member| member.dps)
                .collect::<Vec<_>>(),
            vec![50.0, 40.0, 30.0, 20.0]
        );
    }

    #[test]
    fn team_dps_export_prefers_realtime_abyss_halves() {
        let mut state = CombatState::default();
        let mut upper_hit = hit_with_direction("outgoing");
        upper_hit.char_id = 10;
        upper_hit.char_name = "上行".to_owned();
        upper_hit.damage = 100.0;
        let mut lower_hit = hit_with_direction("outgoing");
        lower_hit.char_id = 20;
        lower_hit.char_name = "下行".to_owned();
        lower_hit.damage = 200.0;
        state.abyss.first_half.push_hit(upper_hit);
        state.abyss.second_half.push_hit(lower_hit);

        let overview = AbyssOverviewState {
            upper_team: Some(TeamDps {
                dps: 1.0,
                members: vec![TeamDpsMember {
                    id: 99,
                    dps: 1.0,
                    name: "旧预测".to_owned(),
                }],
            }),
            ..AbyssOverviewState::default()
        };

        let export = build_team_dps_export(&state, &overview, true).unwrap();

        assert!(export.single.is_none());
        assert_eq!(export.upper.unwrap().members[0].id, 10);
        assert_eq!(export.lower.unwrap().members[0].id, 20);
    }

    #[test]
    fn character_avatar_colors_fill_missing_table_colors() {
        let mut characters = HashMap::from([
            (
                1004,
                CharacterInfo {
                    name_zh: "安魂曲".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: Some("res/images/characters/player_004_256.png".to_owned()),
                    attribute: Some("暗".to_owned()),
                },
            ),
            (
                1020,
                CharacterInfo {
                    name_zh: "哈尼娅".to_owned(),
                    name_en: String::new(),
                    color: None,
                    avatar: Some("res/images/characters/player_020_256.png".to_owned()),
                    attribute: Some("魂".to_owned()),
                },
            ),
        ]);

        fill_missing_character_colors_from_avatars(&mut characters, std::path::Path::new("."));

        let first = character_color(1004, &characters, 0);
        let second = character_color(1020, &characters, 1);
        assert_ne!(first, second);
        assert!(characters.values().all(|character| {
            character
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .is_some()
        }));
    }

    #[test]
    fn character_table_color_wins_over_avatar_color() {
        let mut characters = HashMap::from([(
            1010,
            CharacterInfo {
                name_zh: "娜娜莉".to_owned(),
                name_en: String::new(),
                color: Some("#123456".to_owned()),
                avatar: Some("res/images/characters/player_010_256.png".to_owned()),
                attribute: Some("灵".to_owned()),
            },
        )]);

        fill_missing_character_colors_from_avatars(&mut characters, std::path::Path::new("."));

        assert_eq!(
            characters
                .get(&1010)
                .and_then(|character| character.color.as_deref()),
            Some("#123456")
        );
        assert_eq!(
            character_color(1010, &characters, 0),
            egui::Color32::from_rgb(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn encrypted_ini_round_trips_with_china_key() {
        let plaintext = "[Core.System]\nGameName=NTE\nEndpoint=https://example.invalid";
        let encrypted = encrypt_encrypted_ini_text(plaintext, EncryptedIniKey::China)
            .expect("fixture should encrypt");
        assert!(encrypted.lines().all(|line| BASE64.decode(line).is_ok()));

        let (key, decrypted, encrypted_lines) =
            decrypt_encrypted_ini_text(&encrypted).expect("encrypted INI should decrypt");
        assert_eq!(key, EncryptedIniKey::China);
        assert_eq!(decrypted, plaintext);
        assert_eq!(encrypted_lines, 3);
    }

    #[test]
    fn encrypted_ini_save_preserves_unchanged_records() {
        let first = BASE64.encode(
            encrypt_aes256_ecb(
                &pkcs7_pad(b"[Setting]|SPLIT||SPLIT|Sound_MainVolumn=70|SPLIT|"),
                EncryptedIniKey::China.key(),
            )
            .expect("fixture should encrypt"),
        );
        let second = BASE64.encode(
            encrypt_aes256_ecb(
                &pkcs7_pad(b"FightDefaultArmLengthScale=0"),
                EncryptedIniKey::China.key(),
            )
            .expect("fixture should encrypt"),
        );
        let encrypted = format!("{first}\n{second}\n");
        let (key, plaintext, records, line_ending, final_newline) =
            parse_encrypted_ini_text(&encrypted).expect("fixture should decrypt");
        assert_eq!(
            plaintext,
            "[Setting]\nSound_MainVolumn=70\nFightDefaultArmLengthScale=0"
        );

        let modified = plaintext.replace(
            "FightDefaultArmLengthScale=0",
            "FightDefaultArmLengthScale=1",
        );
        let saved = encrypt_encrypted_ini_records(
            &modified,
            key,
            key,
            &records,
            &line_ending,
            final_newline,
        )
        .expect("fixture should encrypt");
        let saved_lines = saved.lines().collect::<Vec<_>>();
        assert_eq!(saved_lines[0], first);
        assert_ne!(saved_lines[1], second);

        let restored = encrypt_encrypted_ini_records(
            &plaintext,
            key,
            key,
            &records,
            &line_ending,
            final_newline,
        )
        .expect("fixture should encrypt");
        assert_eq!(restored, encrypted);
    }

    #[test]
    fn encrypted_ini_reencrypts_same_payload_to_same_ciphertext_after_reload() {
        let original_payload = "|SPLIT|FightDefaultArmLengthScale=0|SPLIT|";
        let changed_line = "FightDefaultArmLengthScale=1";
        let original = format!(
            "{}\n",
            BASE64.encode(
                encrypt_aes256_ecb(
                    &pkcs7_pad(original_payload.as_bytes()),
                    EncryptedIniKey::China.key(),
                )
                .expect("fixture should encrypt"),
            )
        );
        let (key, _, records, line_ending, final_newline) =
            parse_encrypted_ini_text(&original).expect("fixture should decrypt");

        let changed = encrypt_encrypted_ini_records(
            changed_line,
            key,
            key,
            &records,
            &line_ending,
            final_newline,
        )
        .expect("fixture should encrypt");
        assert_ne!(changed, original);
        let (reloaded_key, _, reloaded_records, reloaded_line_ending, reloaded_final_newline) =
            parse_encrypted_ini_text(&changed).expect("changed fixture should decrypt");
        let restored = encrypt_encrypted_ini_records(
            "FightDefaultArmLengthScale=0",
            reloaded_key,
            reloaded_key,
            &reloaded_records,
            &reloaded_line_ending,
            reloaded_final_newline,
        )
        .expect("fixture should encrypt");
        assert_eq!(restored, original);
    }

    #[test]
    fn encrypted_ini_save_preserves_crlf_and_missing_final_newline() {
        let first = BASE64.encode(
            encrypt_aes256_ecb(
                &pkcs7_pad(b"[Setting]|SPLIT|Sound_MainVolumn=70"),
                EncryptedIniKey::China.key(),
            )
            .expect("fixture should encrypt"),
        );
        let second = BASE64.encode(
            encrypt_aes256_ecb(
                &pkcs7_pad(b"FightDefaultArmLengthScale=0"),
                EncryptedIniKey::China.key(),
            )
            .expect("fixture should encrypt"),
        );
        let encrypted = format!("{first}\r\n{second}");
        let (key, plaintext, records, line_ending, final_newline) =
            parse_encrypted_ini_text(&encrypted).expect("fixture should decrypt");
        assert_eq!(line_ending, "\r\n");
        assert!(!final_newline);

        let saved = encrypt_encrypted_ini_records(
            &plaintext,
            key,
            key,
            &records,
            &line_ending,
            final_newline,
        )
        .expect("fixture should encrypt");
        assert_eq!(saved, encrypted);
        assert!(!saved.ends_with('\n'));
        assert!(saved.contains("\r\n"));
    }

    #[test]
    fn encrypted_ini_search_is_case_insensitive() {
        let text = "bUseHDR=True\nHDRDisplay=1\nOther=0";
        let matches = encrypted_ini_search_matches(text, "hdr");
        assert_eq!(matches.len(), 2);
        assert_eq!(&text[matches[0]..matches[0] + 3], "HDR");
    }

    #[test]
    fn encrypted_ini_text_fingerprint_tracks_same_length_edits() {
        assert_ne!(
            encrypted_ini_text_fingerprint("Alpha=1\nBeta=2"),
            encrypted_ini_text_fingerprint("Alpha=1\nBeta=3")
        );
    }

    #[test]
    fn ui_config_save_plan_debounces_first_dirty_state() {
        let now = Instant::now();
        let saved = UiConfig {
            opacity: 0.8,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
            ..UiConfig::default()
        };
        let current = UiConfig {
            opacity: 0.9,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
            ..UiConfig::default()
        };
        match DpsApp::ui_config_save_plan(&current, &saved, None, now) {
            UiConfigSavePlan::SetPending((pending, save_at)) => {
                assert_eq!(pending, current);
                assert!(save_at > now + Duration::from_millis(300));
            }
            _ => panic!("expected set-pending schedule"),
        }
    }

    #[test]
    fn ui_config_save_plan_retries_when_unmodified_pending_expires() {
        let now = Instant::now();
        let saved = UiConfig {
            opacity: 0.8,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
            ..UiConfig::default()
        };
        let current = UiConfig {
            opacity: 0.9,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
            ..UiConfig::default()
        };
        let future = now + Duration::from_millis(500);
        let pending = Some((current.clone(), future));
        match DpsApp::ui_config_save_plan(&current, &saved, pending.as_ref(), now) {
            UiConfigSavePlan::KeepPending((_, wait_until)) => {
                assert_eq!(wait_until, future);
            }
            _ => panic!("expected keep-pending state"),
        }

        let expired = now + Duration::from_millis(1000);
        match DpsApp::ui_config_save_plan(&current, &saved, pending.as_ref(), expired) {
            UiConfigSavePlan::Save(pending) => {
                assert_eq!(pending, current);
            }
            _ => panic!("expected save attempt"),
        }
    }
}
