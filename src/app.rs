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

use crate::abyss_data::{AbyssFloor, AbyssMonsterDataset, AbyssMonsterEntry};
use crate::capture::{
    CaptureDevice, CaptureHandle, RawCaptureBuffer, import_capture_json, import_pcapng,
    list_devices, start_capture,
};
use crate::character_editor::{
    CHARACTER_ATTRIBUTES, CharacterEditForm, CharacterEditorState, json_string_field,
};
use crate::config::{self, UiConfig};
use crate::encrypted_ini::{
    EncryptedIniKey, EncryptedIniRecord, encrypt_encrypted_ini_records,
    encrypted_ini_search_matches, encrypted_ini_text_fingerprint, parse_encrypted_ini_text,
};
use crate::file_drop::NativeFileDrop;
use crate::hotkey::{HotkeyEvent, HotkeyHandle};
use crate::io_util::{atomic_write_file, atomic_write_text};
use crate::model::{
    AbyssEvent, AbyssHalf, CharacterInfo, CharacterStats, CombatState, EngineEvent,
    HitDirectionSummary, PartyCombatState, TEAM_DPS_EXPORT_VERSION, TEAM_DPS_MAX_MEMBERS, TeamDps,
    TeamDpsExport, TeamDpsMember, summarize_hit_directions,
};
use crate::network::{GameNetwork, detect_game_device};
use crate::parser::{CHARACTER_DATA_PATH, find_data_file, load_characters};
use crate::window_attributes::{
    apply_rounding_to_process_windows, apply_window_attributes, clear_process_windows_topmost,
    restore_visible_process_windows_topmost, set_window_topmost,
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
const TITLE_BAR_BUTTON_SIZE: egui::Vec2 = egui::vec2(28.0, 22.0);
const TITLE_BAR_TOGGLE_SIZE: egui::Vec2 = egui::vec2(64.0, 24.0);
/// Default (100%) inner sizes for each window. The title-bar −／＋ stepper scales
/// these proportionally instead of free drag-resize, which keeps the aspect ratio
/// and avoids the Windows resize crash (egui #4061 / #4091). `main` is also used
/// by `main.rs` for the initial root viewport size.
pub(crate) const MAIN_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(520.0, 420.0);
const ABYSS_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(1040.0, 720.0);
const HIT_DETAIL_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(1120.0, 760.0);
const TEAM_HIT_DETAIL_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(980.0, 660.0);
const DEBUG_WINDOW_BASE_SIZE: egui::Vec2 = egui::vec2(980.0, 640.0);
const WINDOW_SCALE_MIN: f32 = 0.7;
const WINDOW_SCALE_MAX: f32 = 1.5;
const WINDOW_SCALE_STEP: f32 = 0.1;
const UI_CONFIG_SAVE_DELAY: Duration = Duration::from_millis(350);
const UI_CONFIG_SAVE_RETRY_DELAY: Duration = Duration::from_secs(2);
const ABYSS_STAT_NAMES_ZH_CN_PATH: &str = "res/data/abyss/monster_stat_names_zh_cn.json";

#[derive(Clone, Copy)]
enum DebugImportKind {
    Pcapng,
    CaptureJson,
    EncryptedIni,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DebugTab {
    #[default]
    Packets,
    Characters,
    EncryptedIni,
    Environment,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum HitDetailFilter {
    #[default]
    All,
    Outgoing,
    Incoming,
    QteType(String),
}

impl HitDetailFilter {
    fn matches(&self, hit: &crate::model::Hit) -> bool {
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
struct QteTypeFilterSummary {
    attack_type: String,
    hits: usize,
    damage: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HitDetailSource {
    Global,
    AbyssFirst,
    AbyssSecond,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HitDetailCacheKey {
    source: HitDetailSource,
    char_id: Option<u32>,
    filter: HitDetailFilter,
    skill_filter: String,
    limit: usize,
}

#[derive(Clone, Copy)]
struct CachedHitRow {
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
struct HitDetailCache {
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
struct EncryptedIniLayoutCache {
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

struct EncryptedIniLayoutRequest<'a> {
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
        let encrypted = std::fs::read_to_string(&path)
            .map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
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
            message: format!("已解析 {encrypted_lines} 行密文，使用 {} key", key.label()),
            layout_cache: EncryptedIniLayoutCache::default(),
        })
    }

    fn display_path(&self) -> String {
        self.path.as_ref().map_or_else(
            || "未打开文件".to_owned(),
            |path| path.display().to_string(),
        )
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
struct AbyssOverviewState {
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
}

fn load_abyss_stat_display_names() -> HashMap<String, String> {
    let Some(path) = find_data_file(Path::new(ABYSS_STAT_NAMES_ZH_CN_PATH)) else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
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
        *self = Self::load();
        self.search = search;
        self.upper_team = upper_team;
        self.lower_team = lower_team;
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
    abyss_overview: AbyssOverviewState,
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
    detail_last_scroll_activity: Option<Instant>,
    devices: Vec<CaptureDevice>,
    selected_device: usize,
    local_ip: String,
    game_network: Option<GameNetwork>,
    filter: String,
    active_capture_filter: Option<String>,
    include_incoming: bool,
    server_damage_calibration: bool,
    capture: Option<CaptureHandle>,
    raw_capture: Option<RawCaptureBuffer>,
    replay_stop: Option<Arc<AtomicBool>>,
    replay_thread: Option<thread::JoinHandle<()>>,
    sender: Sender<EngineEvent>,
    receiver: Receiver<EngineEvent>,
    paused_events: VecDeque<EngineEvent>,
    dropped_debug_packets: u64,
    status: String,
    diagnostic: Option<String>,
    last_error: Option<String>,
    debug_open: bool,
    debug_corner_applied: bool,
    debug_tab: DebugTab,
    debug_only_hits: bool,
    debug_search: String,
    character_editor: CharacterEditorState,
    encrypted_ini_editor: EncryptedIniEditorState,
    paused: bool,
    dark_mode: bool,
    always_on_top: bool,
    mouse_passthrough: bool,
    opacity: f32,
    applied_opacity: Option<f32>,
    corner_applied_hwnd: Option<isize>,
    // Per-window proportional size factor (1.0 = the window's default size). Set
    // by the title-bar −／＋ stepper; not persisted across launches.
    main_window_scale: f32,
    abyss_window_scale: f32,
    hit_detail_window_scale: f32,
    team_hit_detail_window_scale: f32,
    debug_window_scale: f32,
    style_dark_mode_applied: Option<bool>,
    opacity_reapply_frames: u8,
    theme_transition_from: Option<Color32>,
    theme_transition_started_at: Option<f64>,
    pending_debug_import: Option<(DebugImportKind, u8)>,
    saved_ui_config: UiConfig,
    pending_ui_config: Option<(UiConfig, Instant)>,
    ui_config_path: PathBuf,
    native_file_drop: NativeFileDrop,
    last_dropped_file: Option<(PathBuf, Instant)>,
    hotkey_receiver: Receiver<HotkeyEvent>,
    _hotkey: HotkeyHandle,
}

impl DpsApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        ui_config: UiConfig,
        config_warning: Option<String>,
    ) -> Self {
        install_fonts(&cc.egui_ctx);
        configure_style(&cc.egui_ctx, ui_config.dark_mode);
        let (hotkey, hotkey_receiver) = HotkeyHandle::start(cc.egui_ctx.clone());
        let (sender, receiver) = unbounded();
        let data_root = data_root();
        let characters_path = data_root.join(CHARACTER_DATA_PATH);
        let (characters, character_load_error) = match load_characters(characters_path.as_path()) {
            Ok(characters) => (characters, None),
            Err(error) => (
                HashMap::new(),
                Some(format!(
                    "角色数据加载失败（{}）：{error}",
                    characters_path.display()
                )),
            ),
        };
        let avatar_textures = load_character_avatars(&cc.egui_ctx, &data_root, &characters);
        let attribute_textures = load_attribute_icons(&cc.egui_ctx, &data_root);
        let damage_digit_textures = load_damage_digit_textures(&cc.egui_ctx, &data_root);
        let reaction_textures = load_reaction_text_textures(&cc.egui_ctx, &data_root);
        let abyss_overview = AbyssOverviewState::load();
        let monster_textures = load_monster_textures(&cc.egui_ctx, &data_root, &abyss_overview);
        let character_editor =
            CharacterEditorState::load(&characters_path).unwrap_or_else(|error| {
                CharacterEditorState {
                    document: serde_json::json!({"version": 2, "characters": {}}),
                    selected_id: None,
                    form: CharacterEditForm::default(),
                    search: String::new(),
                    new_id: String::new(),
                    dirty: false,
                    message: error,
                    cancel_selection: None,
                }
            });
        let (devices, device_error) = match list_devices() {
            Ok(devices) => (devices, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let (selected_device, game_network, status, mut diagnostic) = match device_error {
            Some(error) => (0, None, "采集环境不可用".to_owned(), Some(error)),
            None => match detect_game_device(&devices) {
                Ok((index, network)) => (index, Some(network), "已就绪".to_owned(), None),
                Err(error) => (0, None, "未检测到游戏".to_owned(), Some(error)),
            },
        };
        if let Some(error) = &character_load_error {
            diagnostic = Some(match diagnostic {
                Some(existing) => format!("{error}\n{existing}"),
                None => error.clone(),
            });
        }
        let local_ip = game_network
            .as_ref()
            .map(|network| network.local_ip.to_string())
            .unwrap_or_default();
        let startup_error = match (config_warning, character_load_error) {
            (Some(config_error), Some(character_error)) => {
                Some(format!("{config_error}\n{character_error}"))
            }
            (Some(error), None) | (None, Some(error)) => Some(error),
            (None, None) => None,
        };
        Self {
            characters: Arc::new(characters),
            avatar_textures,
            attribute_textures,
            monster_textures,
            damage_digit_textures,
            reaction_textures,
            state: CombatState::default(),
            selected_abyss_half: AbyssHalf::First,
            abyss_compact_mode: false,
            abyss_overview,
            abyss_overview_open: false,
            abyss_overview_corner_applied: false,
            hit_detail_char_id: None,
            hit_detail_filter: HitDetailFilter::All,
            hit_detail_skill_filter: String::new(),
            hit_detail_corner_applied: false,
            team_hit_detail_open: false,
            team_hit_detail_filter: HitDetailFilter::All,
            team_hit_detail_corner_applied: false,
            character_hit_cache: HitDetailCache::default(),
            team_hit_cache: HitDetailCache::default(),
            skill_summary_cache: SkillSummaryCache::default(),
            detail_last_scroll_activity: None,
            devices,
            selected_device,
            local_ip,
            game_network,
            filter: "udp".to_owned(),
            active_capture_filter: None,
            include_incoming: true,
            server_damage_calibration: ui_config.server_damage_calibration,
            capture: None,
            raw_capture: None,
            replay_stop: None,
            replay_thread: None,
            sender,
            receiver,
            paused_events: VecDeque::new(),
            dropped_debug_packets: 0,
            status,
            diagnostic,
            last_error: startup_error,
            debug_open: false,
            debug_corner_applied: false,
            debug_tab: DebugTab::Packets,
            debug_only_hits: false,
            debug_search: String::new(),
            character_editor,
            encrypted_ini_editor: EncryptedIniEditorState::default(),
            paused: false,
            dark_mode: ui_config.dark_mode,
            always_on_top: ui_config.always_on_top,
            mouse_passthrough: false,
            opacity: ui_config.opacity,
            applied_opacity: None,
            corner_applied_hwnd: None,
            main_window_scale: 1.0,
            abyss_window_scale: 1.0,
            hit_detail_window_scale: 1.0,
            team_hit_detail_window_scale: 1.0,
            debug_window_scale: 1.0,
            // eframe may replace the context style after app construction.
            style_dark_mode_applied: None,
            opacity_reapply_frames: 4,
            theme_transition_from: None,
            theme_transition_started_at: None,
            pending_debug_import: None,
            saved_ui_config: ui_config,
            pending_ui_config: None,
            ui_config_path: config::config_path(),
            native_file_drop: NativeFileDrop::new(),
            last_dropped_file: None,
            hotkey_receiver,
            _hotkey: hotkey,
        }
    }

    fn stop_engine(&mut self) {
        if let Some(mut capture) = self.capture.take() {
            capture.stop();
        }
        if let Some(stop) = self.replay_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(thread) = self.replay_thread.take() {
            let _ = thread.join();
        }
        // All producers are joined, so every queued event belongs to the stopped task.
        // Apply them now to prevent a delayed CaptureStopped from affecting the next task.
        self.drain_pending_events();
    }

    fn reset_combat_session(&mut self) {
        self.state.clear();
        self.selected_abyss_half = AbyssHalf::First;
        self.abyss_compact_mode = false;
        self.hit_detail_char_id = None;
        self.hit_detail_filter = HitDetailFilter::All;
        self.hit_detail_skill_filter.clear();
        self.hit_detail_corner_applied = false;
        self.team_hit_detail_open = false;
        self.team_hit_detail_filter = HitDetailFilter::All;
        self.team_hit_detail_corner_applied = false;
        self.character_hit_cache = HitDetailCache::default();
        self.team_hit_cache = HitDetailCache::default();
        self.skill_summary_cache = SkillSummaryCache::default();
        self.detail_last_scroll_activity = None;
        self.paused = false;
        self.paused_events.clear();
        self.dropped_debug_packets = 0;
    }

    fn drain_hotkeys(&mut self, ctx: &egui::Context) {
        let home_pressed = ctx.input(|input| input.key_pressed(egui::Key::Home));
        let import_pressed =
            ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::O));
        #[cfg(not(feature = "no_debug"))]
        let f12_pressed = ctx.input(|input| input.key_pressed(egui::Key::F12));
        if home_pressed {
            self.toggle_mouse_passthrough(ctx);
        }
        if import_pressed {
            self.request_debug_import(ctx, DebugImportKind::Pcapng);
        }
        #[cfg(not(feature = "no_debug"))]
        if f12_pressed {
            self.debug_open = !self.debug_open;
            if self.debug_open {
                self.debug_corner_applied = false;
            }
        }
        while let Ok(event) = self.hotkey_receiver.try_recv() {
            match event {
                HotkeyEvent::TogglePassthrough => {
                    self.toggle_mouse_passthrough(ctx);
                }
                #[cfg(not(feature = "no_debug"))]
                HotkeyEvent::ToggleDebug => {
                    self.debug_open = !self.debug_open;
                    if self.debug_open {
                        self.debug_corner_applied = false;
                    }
                }
                HotkeyEvent::RegistrationFailed(shortcut) => {
                    self.diagnostic = Some(format!(
                        "无法注册全局快捷键 {shortcut}，可能已被其他程序占用"
                    ));
                }
            }
        }
    }

    fn toggle_mouse_passthrough(&mut self, ctx: &egui::Context) {
        self.mouse_passthrough = !self.mouse_passthrough;
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(
            self.mouse_passthrough,
        ));
        self.opacity_reapply_frames = 2;
        self.status = if self.mouse_passthrough {
            "鼠标穿透已开启 按下HOME键关闭".to_owned()
        } else {
            "鼠标穿透已关闭".to_owned()
        };
    }

    fn toggle_always_on_top(&mut self, ctx: &egui::Context) {
        self.always_on_top = !self.always_on_top;
        let level = if self.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        self.opacity_reapply_frames = 2;
        self.status = if self.always_on_top {
            "窗口置顶已开启".to_owned()
        } else {
            "窗口置顶已关闭".to_owned()
        };
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let title_height = 28.0;
        // The whole title bar is the drag-to-move zone: allocate it first with a
        // drag sense, then draw the label/buttons on top. Buttons (added later)
        // win the pointer where they are, so dragging works on any empty area —
        // the title text included — no matter how many controls crowd the bar.
        let (full_rect, title_drag) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), title_height),
            egui::Sense::click_and_drag(),
        );
        if title_drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(full_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );

        child.label(
            RichText::new("NTE DPS TOOL")
                .size(13.0)
                .strong()
                .color(theme_accent(self.dark_mode)),
        );
        let title_status = if self.paused {
            format!(
                "已暂停 · 待处理 {} · 已丢弃调试封包 {}",
                self.paused_events.len(),
                self.dropped_debug_packets
            )
        } else {
            self.status.clone()
        };
        child
            .label(RichText::new("●").size(9.0).color(status_color(
                &self.status,
                self.paused,
                self.dark_mode,
            )))
            .on_hover_text(title_status);

        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_sized(TITLE_BAR_BUTTON_SIZE, egui::Button::new("×").frame(false))
                .on_hover_text("关闭")
                .clicked()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if ui
                .add_sized(TITLE_BAR_BUTTON_SIZE, egui::Button::new("−").frame(false))
                .on_hover_text("最小化")
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            window_scale_stepper(ui, &mut self.main_window_scale, MAIN_WINDOW_BASE_SIZE);
            if !self.abyss_compact_mode || !self.state.abyss.is_active() {
                ui.allocate_ui_with_layout(
                    TITLE_BAR_TOGGLE_SIZE,
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        ui.menu_button("外观", |ui| {
                            ui.set_min_width(190.0);
                            ui.horizontal(|ui| {
                                ui.label("透明度");
                                ui.add(
                                    egui::Slider::new(&mut self.opacity, 0.35..=1.0)
                                        .show_value(true)
                                        .custom_formatter(|value, _| {
                                            format!("{:.0}%", value * 100.0)
                                        }),
                                );
                            });
                            if ui
                                .button(if self.dark_mode {
                                    "切换为亮色"
                                } else {
                                    "切换为深色"
                                })
                                .clicked()
                            {
                                self.theme_transition_from =
                                    Some(shadcn_background(self.dark_mode));
                                self.theme_transition_started_at =
                                    Some(ui.input(|input| input.time));
                                self.dark_mode = !self.dark_mode;
                                ui.close();
                            }
                        });
                    },
                );
                let passthrough_label = if self.mouse_passthrough {
                    "穿透中"
                } else {
                    "穿透"
                };
                if ui
                    .add_sized(
                        TITLE_BAR_TOGGLE_SIZE,
                        egui::Button::selectable(self.mouse_passthrough, passthrough_label),
                    )
                    .on_hover_text("Home 可随时切换鼠标穿透")
                    .clicked()
                {
                    self.toggle_mouse_passthrough(ui.ctx());
                }
                if ui
                    .add_sized(
                        TITLE_BAR_TOGGLE_SIZE,
                        egui::Button::selectable(self.always_on_top, "置顶"),
                    )
                    .on_hover_text("保持主窗口位于游戏上方")
                    .clicked()
                {
                    self.toggle_always_on_top(ui.ctx());
                }
            }
        });
    }

    fn start_live(&mut self) {
        self.stop_engine();
        self.active_capture_filter = None;
        if let Err(error) = self.refresh_game_network() {
            self.last_error = Some(error);
            return;
        }
        let Some(device) = self.devices.get(self.selected_device).cloned() else {
            self.last_error = Some("没有可用抓包设备，请确认已安装 Npcap".to_owned());
            return;
        };
        let local_ip = self.game_network.as_ref().map(|network| network.local_ip);
        let capture_filter = self.filter.clone();
        self.reset_combat_session();
        let capture = start_capture(
            device,
            local_ip,
            capture_filter.clone(),
            self.include_incoming,
            self.server_damage_calibration,
            self.characters.clone(),
            self.sender.clone(),
        );
        self.active_capture_filter = Some(capture_filter);
        self.raw_capture = Some(capture.raw_capture());
        self.capture = Some(capture);
        self.status = "正在启动实时抓包...".to_owned();
    }

    fn refresh_game_network(&mut self) -> Result<(), String> {
        self.devices = list_devices().inspect_err(|error| {
            self.diagnostic = Some(error.clone());
        })?;
        let (index, network) = detect_game_device(&self.devices).inspect_err(|error| {
            self.diagnostic = Some(error.clone());
        })?;
        self.selected_device = index;
        self.local_ip = network.local_ip.to_string();
        self.status = "已检测到游戏，准备就绪".to_owned();
        self.diagnostic = None;
        self.game_network = Some(network);
        Ok(())
    }

    fn start_pcapng_import(&mut self, path: PathBuf) {
        self.stop_engine();
        self.raw_capture = None;
        self.active_capture_filter = None;
        self.reset_combat_session();
        let local_ip_hint = self
            .game_network
            .as_ref()
            .map(|network| network.local_ip)
            .or_else(|| self.local_ip.parse::<Ipv4Addr>().ok());
        let stop = Arc::new(AtomicBool::new(false));
        self.replay_thread = Some(import_pcapng(
            path,
            self.characters.clone(),
            local_ip_hint,
            self.include_incoming,
            self.server_damage_calibration,
            self.sender.clone(),
            stop.clone(),
        ));
        self.replay_stop = Some(stop);
        self.status = local_ip_hint.map_or_else(
            || "正在导入并解析 pcapng（启发式判向）...".to_owned(),
            |ip| format!("正在导入并解析 pcapng（本机 IP {ip} 过滤/判向）..."),
        );
    }

    fn start_capture_json_import(&mut self, path: PathBuf) {
        self.stop_engine();
        self.raw_capture = None;
        self.active_capture_filter = None;
        self.reset_combat_session();
        let stop = Arc::new(AtomicBool::new(false));
        self.replay_thread = Some(import_capture_json(path, self.sender.clone(), stop.clone()));
        self.replay_stop = Some(stop);
        self.status = "正在导入抓包 JSON...".to_owned();
    }

    fn process_file_drops(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        self.native_file_drop.install(frame);
        let mut paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        paths.extend(self.native_file_drop.try_iter());
        for path in paths {
            self.import_dropped_file(path);
        }
    }

    fn import_dropped_file(&mut self, path: PathBuf) {
        if self
            .last_dropped_file
            .as_ref()
            .is_some_and(|(previous, at)| {
                previous == &path && at.elapsed() < Duration::from_secs(1)
            })
        {
            return;
        }
        self.last_dropped_file = Some((path.clone(), Instant::now()));
        let is_pcapng = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pcapng"));
        if !is_pcapng {
            self.last_error = Some(format!(
                "不支持拖入该文件：{}\n当前仅支持 .pcapng",
                path.display()
            ));
            return;
        }
        self.start_pcapng_import(path);
    }

    fn current_ui_config(&self) -> UiConfig {
        UiConfig {
            opacity: self.opacity,
            dark_mode: self.dark_mode,
            always_on_top: self.always_on_top,
            server_damage_calibration: self.server_damage_calibration,
        }
        .sanitized()
    }

    fn ui_config_save_plan(
        current: &UiConfig,
        saved_ui_config: &UiConfig,
        pending_ui_config: Option<&(UiConfig, Instant)>,
        now: Instant,
    ) -> UiConfigSavePlan {
        if current == saved_ui_config {
            UiConfigSavePlan::NoChange
        } else if let Some((pending, save_at)) = pending_ui_config {
            if pending == current {
                if *save_at <= now {
                    UiConfigSavePlan::Save(pending.clone())
                } else {
                    UiConfigSavePlan::KeepPending((pending.clone(), *save_at))
                }
            } else {
                UiConfigSavePlan::SetPending((current.clone(), now + UI_CONFIG_SAVE_DELAY))
            }
        } else {
            UiConfigSavePlan::SetPending((current.clone(), now + UI_CONFIG_SAVE_DELAY))
        }
    }

    fn persist_ui_config(&mut self) {
        let current = self.current_ui_config();
        let now = Instant::now();
        match Self::ui_config_save_plan(
            &current,
            &self.saved_ui_config,
            self.pending_ui_config.as_ref(),
            now,
        ) {
            UiConfigSavePlan::NoChange => {
                self.pending_ui_config = None;
            }
            UiConfigSavePlan::SetPending((pending, save_at))
            | UiConfigSavePlan::KeepPending((pending, save_at)) => {
                self.pending_ui_config = Some((pending, save_at));
            }
            UiConfigSavePlan::Save(pending) => match config::save(&self.ui_config_path, &pending) {
                Ok(()) => {
                    self.saved_ui_config = pending;
                    self.pending_ui_config = None;
                }
                Err(error) => {
                    self.last_error = Some(format!(
                        "UI configuration failed to save. Please check: {error} {}",
                        self.ui_config_path.display()
                    ));
                    self.pending_ui_config = Some((pending, now + UI_CONFIG_SAVE_RETRY_DELAY));
                }
            },
        }
    }

    fn persist_ui_config_on_shutdown(&mut self) {
        let current = self.current_ui_config();
        if let Some((pending, _)) = self.pending_ui_config.take() {
            let _ = config::save(&self.ui_config_path, &pending);
            return;
        }
        if current != self.saved_ui_config {
            let _ = config::save(&self.ui_config_path, &current);
        }
    }

    fn request_debug_import(&mut self, ctx: &egui::Context, kind: DebugImportKind) {
        clear_process_windows_topmost(false);
        ctx.request_repaint();
        self.pending_debug_import = Some((kind, 1));
    }

    fn open_native_file_dialog<T>(
        &mut self,
        ctx: &egui::Context,
        dialog: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        clear_process_windows_topmost(false);

        let result = dialog();

        self.restore_window_levels_after_file_dialog();
        ctx.request_repaint();
        result
    }

    fn restore_window_levels_after_file_dialog(&mut self) {
        restore_visible_process_windows_topmost();
        if !self.always_on_top
            && let Some(hwnd) = self.corner_applied_hwnd
        {
            set_window_topmost(hwnd, false);
        }
        self.opacity_reapply_frames = 2;
    }

    fn process_debug_import_dialog(&mut self, ctx: &egui::Context) {
        let Some((kind, delay)) = self.pending_debug_import else {
            return;
        };
        if delay > 0 {
            self.pending_debug_import = Some((kind, delay - 1));
            return;
        }
        self.pending_debug_import = None;
        let path = self.open_native_file_dialog(ctx, || match kind {
            DebugImportKind::Pcapng => rfd::FileDialog::new()
                .add_filter("Wireshark 抓包", &["pcapng"])
                .pick_file(),
            DebugImportKind::CaptureJson => rfd::FileDialog::new()
                .add_filter("NTE 导出抓包", &["json"])
                .pick_file(),
            DebugImportKind::EncryptedIni => rfd::FileDialog::new()
                .add_filter("NTE 加密 INI", &["ini"])
                .add_filter("所有文件", &["*"])
                .pick_file(),
        });
        if let Some(path) = path {
            match kind {
                DebugImportKind::Pcapng => self.start_pcapng_import(path),
                DebugImportKind::CaptureJson => self.start_capture_json_import(path),
                DebugImportKind::EncryptedIni => self.load_encrypted_ini(path),
            }
        }
    }

    fn drain_events(&mut self) {
        let started = Instant::now();
        let scrolling = self.detail_scroll_active();
        let event_limit = if scrolling {
            MAX_UI_EVENTS_WHILE_SCROLLING
        } else {
            MAX_UI_EVENTS_PER_FRAME
        };
        if self.paused {
            for _ in 0..event_limit {
                if started.elapsed() >= UI_EVENT_BUDGET {
                    break;
                }
                let Ok(event) = self.receiver.try_recv() else {
                    break;
                };
                self.buffer_paused_event(event);
            }
            // Bound the queue even if inflow outpaces the per-frame budget while paused.
            while self.receiver.len() > MAX_ENGINE_QUEUE_HARD_CAP {
                let Ok(event) = self.receiver.try_recv() else {
                    break;
                };
                self.buffer_paused_event(event);
            }
            return;
        }
        for _ in 0..event_limit {
            if started.elapsed() >= UI_EVENT_BUDGET {
                break;
            }
            let event = if let Some(event) = self.paused_events.pop_front() {
                event
            } else if let Ok(event) = self.receiver.try_recv() {
                event
            } else {
                break;
            };
            self.apply_engine_event(event);
        }
        if !scrolling && started.elapsed() < UI_EVENT_BUDGET {
            self.shed_event_backlog(started);
        }
        self.enforce_engine_queue_hard_cap();
    }

    /// Routes one event while paused: debug packets are dropped, hit-like events are buffered
    /// (oldest dropped past the cap) for replay on resume, and lifecycle events apply immediately.
    fn buffer_paused_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Packet(_) => {
                self.dropped_debug_packets = self.dropped_debug_packets.saturating_add(1);
            }
            EngineEvent::Hit(_)
            | EngineEvent::HitFollowUp(_)
            | EngineEvent::HitDamageCorrection(_)
            | EngineEvent::Abyss(_) => {
                if self.paused_events.len() == MAX_PAUSED_EVENTS {
                    self.paused_events.pop_front();
                }
                self.paused_events.push_back(event);
            }
            EngineEvent::Status(_)
            | EngineEvent::Warning(_)
            | EngineEvent::Error(_)
            | EngineEvent::CaptureStopped => self.apply_engine_event(event),
        }
    }

    /// Absolute ceiling on the engine→UI queue so it can never grow without bound — e.g. a sustained
    /// packet flood while the user keeps a detail list scrolling (which otherwise skips shedding).
    /// Dropping debug packets is O(1); the rare non-packet events are applied so stats stay correct.
    fn enforce_engine_queue_hard_cap(&mut self) {
        while self.receiver.len() > MAX_ENGINE_QUEUE_HARD_CAP {
            let Ok(event) = self.receiver.try_recv() else {
                break;
            };
            if matches!(event, EngineEvent::Packet(_)) {
                self.dropped_debug_packets = self.dropped_debug_packets.saturating_add(1);
            } else {
                self.apply_engine_event(event);
            }
        }
    }

    fn shed_event_backlog(&mut self, started: Instant) {
        while self.receiver.len() > MAX_ENGINE_QUEUE_BACKLOG && started.elapsed() < UI_EVENT_BUDGET
        {
            let Ok(event) = self.receiver.try_recv() else {
                break;
            };
            if matches!(event, EngineEvent::Packet(_)) {
                self.dropped_debug_packets = self.dropped_debug_packets.saturating_add(1);
            } else {
                self.apply_engine_event(event);
            }
        }
    }

    fn drain_pending_events(&mut self) {
        while let Some(event) = self.paused_events.pop_front() {
            self.apply_engine_event(event);
        }
        while let Ok(event) = self.receiver.try_recv() {
            self.apply_engine_event(event);
        }
    }

    fn apply_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Hit(hit) => self.state.push_hit(*hit),
            EngineEvent::HitFollowUp(follow_up) => self.state.apply_follow_up(follow_up),
            EngineEvent::HitDamageCorrection(correction) => {
                self.state.apply_damage_correction(correction)
            }
            EngineEvent::Packet(packet) => self.state.push_packet(*packet),
            EngineEvent::Abyss(event) => {
                self.character_hit_cache = HitDetailCache::default();
                self.team_hit_cache = HitDetailCache::default();
                self.skill_summary_cache = SkillSummaryCache::default();
                if let AbyssEvent::Stage { half, .. } = &event {
                    self.selected_abyss_half = *half;
                    self.abyss_compact_mode = true;
                } else if matches!(&event, AbyssEvent::Success { .. } | AbyssEvent::Exit { .. }) {
                    self.abyss_compact_mode = false;
                }
                self.state.apply_abyss_event(event);
            }
            EngineEvent::Status(status) => self.status = status,
            EngineEvent::Warning(warning) => {
                self.diagnostic = Some(format!("部分资源加载失败，功能降级：{warning}"));
            }
            EngineEvent::Error(error) => {
                self.status = "运行失败".to_owned();
                self.last_error = Some(error);
            }
            EngineEvent::CaptureStopped => {
                let import_finished = self.replay_thread.is_some();
                self.capture.take();
                self.replay_stop = None;
                if let Some(thread) = self.replay_thread.take() {
                    let _ = thread.join();
                }
                if import_finished {
                    self.selected_abyss_half = AbyssHalf::First;
                    self.abyss_compact_mode = false;
                }
                self.status = "已停止".to_owned();
            }
        }
    }

    fn export_capture_info(&mut self, ctx: &egui::Context) {
        self.drain_pending_events();
        if self.state.hits.is_empty() && self.state.packets.is_empty() {
            self.last_error = Some("当前没有可导出的抓包信息".to_owned());
            return;
        }
        if self.capture.is_some() || self.replay_thread.is_some() {
            self.last_error = Some("请先停止抓包或回放，再导出本次抓包信息".to_owned());
            return;
        }

        let Some(path) = self.open_native_file_dialog(ctx, || {
            rfd::FileDialog::new()
                .add_filter("抓包信息 JSON", &["json"])
                .set_file_name(default_export_filename())
                .save_file()
        }) else {
            return;
        };

        match atomic_write_file(&path, |writer| {
            let mut out = IoFmtWriter::new(writer);
            self.write_capture_export_json(&mut out);
            out.finish()
        }) {
            Ok(()) => {
                self.status = "已导出抓包信息".to_owned();
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("导出抓包信息失败：{error}"));
            }
        }
    }

    fn export_raw_capture(&mut self, ctx: &egui::Context) {
        if self.capture.is_some() {
            self.last_error = Some("请先停止抓包，再另存完整 PCAPNG".to_owned());
            return;
        }
        if self.raw_capture.is_none() {
            self.last_error = Some("当前没有可另存的完整 PCAPNG".to_owned());
            return;
        }
        let default_file_name = format!("nte_raw_{}.pcapng", Local::now().format("%Y%m%d_%H%M%S"));
        let Some(destination) = self.open_native_file_dialog(ctx, || {
            rfd::FileDialog::new()
                .add_filter("完整原始抓包", &["pcapng"])
                .set_file_name(default_file_name)
                .save_file()
        }) else {
            return;
        };
        let Some(raw_capture) = self.raw_capture.as_ref() else {
            self.last_error = Some("当前没有可另存的完整 PCAPNG".to_owned());
            return;
        };
        match raw_capture.save(&destination) {
            Ok((packet_count, captured_bytes)) => {
                self.status = format!(
                    "已另存完整抓包至 {}（{} 包，{} 字节）",
                    destination.display(),
                    packet_count,
                    captured_bytes
                );
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("另存完整抓包失败：{error}"));
            }
        }
    }

    fn write_capture_export_json(&self, mut out: &mut dyn std::fmt::Write) {
        let duration = self.state.duration().max(0.001);
        let packet_count = self.state.packets.len();
        let hit_count = self.state.hits.len();
        let started_at = self.state.started_at;
        let ended_at = self
            .state
            .hits
            .iter()
            .map(|hit| hit.timestamp)
            .chain(self.state.packets.iter().map(|packet| packet.timestamp))
            .max_by(|left, right| left.total_cmp(right));

        let mut rows: Vec<_> = self.state.stats.values().collect();
        rows.sort_by(|left, right| right.damage.total_cmp(&left.damage));

        writeln!(&mut out, "{{").ok();
        writeln!(
            &mut out,
            "  \"exported_at\": {},",
            json_string(&Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
        )
        .ok();
        writeln!(&mut out, "  \"filter\": {},", json_string(&self.filter)).ok();
        writeln!(
            &mut out,
            "  \"include_incoming\": {},",
            self.include_incoming
        )
        .ok();
        if let Some(network) = &self.game_network {
            writeln!(&mut out, "  \"game_network\": {{").ok();
            writeln!(&mut out, "    \"pid\": {},", network.pid).ok();
            writeln!(
                &mut out,
                "    \"local_ip\": {},",
                json_string(&network.local_ip.to_string())
            )
            .ok();
            writeln!(
                &mut out,
                "    \"remote_ip\": {},",
                json_string(&network.remote_ip.to_string())
            )
            .ok();
            writeln!(&mut out, "    \"remote_port\": {}", network.remote_port).ok();
            writeln!(&mut out, "  }},").ok();
        } else {
            writeln!(&mut out, "  \"game_network\": null,").ok();
        }
        writeln!(&mut out, "  \"summary\": {{").ok();
        writeln!(&mut out, "    \"hits\": {},", hit_count).ok();
        writeln!(&mut out, "    \"packets\": {},", packet_count).ok();
        writeln!(
            &mut out,
            "    \"total_damage\": {},",
            json_f64(self.state.total_damage)
        )
        .ok();
        writeln!(&mut out, "    \"dps\": {},", json_f64(self.state.dps())).ok();
        writeln!(
            &mut out,
            "    \"duration_seconds\": {},",
            json_f64(duration)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"started_at_unix\": {},",
            json_option_f64(started_at)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"started_at_local\": {},",
            json_option_time(started_at)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"ended_at_unix\": {},",
            json_option_f64(ended_at)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"ended_at_local\": {}",
            json_option_time(ended_at)
        )
        .ok();
        writeln!(&mut out, "  }},").ok();

        writeln!(&mut out, "  \"party\": [").ok();
        for (index, row) in rows.iter().enumerate() {
            let share = if self.state.total_damage > 0.0 {
                row.damage / self.state.total_damage * 100.0
            } else {
                0.0
            };
            writeln!(&mut out, "    {{").ok();
            writeln!(&mut out, "      \"char_id\": {},", row.char_id).ok();
            writeln!(&mut out, "      \"name\": {},", json_string(&row.name)).ok();
            writeln!(&mut out, "      \"hits\": {},", row.hits).ok();
            writeln!(&mut out, "      \"damage\": {},", json_f64(row.damage)).ok();
            writeln!(&mut out, "      \"dps\": {},", json_f64(row.dps())).ok();
            writeln!(
                &mut out,
                "      \"duration_seconds\": {},",
                json_f64(row.duration())
            )
            .ok();
            writeln!(&mut out, "      \"share_percent\": {}", json_f64(share)).ok();
            writeln!(
                &mut out,
                "    }}{}",
                if index + 1 == rows.len() { "" } else { "," }
            )
            .ok();
        }
        writeln!(&mut out, "  ],").ok();

        writeln!(&mut out, "  \"abyss\": {{").ok();
        writeln!(
            &mut out,
            "    \"detected\": {},",
            self.state.abyss.is_active()
        )
        .ok();
        writeln!(
            &mut out,
            "    \"floor\": {},",
            self.state
                .abyss
                .floor
                .map_or_else(|| "null".to_owned(), |floor| floor.to_string())
        )
        .ok();
        writeln!(
            &mut out,
            "    \"active_half\": {},",
            self.state
                .abyss
                .active_half
                .map(|half| json_string(half.label()))
                .unwrap_or_else(|| "null".to_owned())
        )
        .ok();
        writeln!(
            &mut out,
            "    \"success_at_unix\": {},",
            json_option_f64(self.state.abyss.success_at)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"first_half_at_unix\": {},",
            json_option_f64(self.state.abyss.first_half_at)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"second_half_at_unix\": {},",
            json_option_f64(self.state.abyss.second_half_at)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"exited_at_unix\": {},",
            json_option_f64(self.state.abyss.exited_at)
        )
        .ok();
        write_abyss_half_json(&mut out, "first_half", &self.state.abyss.first_half, true);
        write_abyss_half_json(
            &mut out,
            "second_half",
            &self.state.abyss.second_half,
            false,
        );
        writeln!(&mut out, "  }},").ok();

        writeln!(&mut out, "  \"hits\": [").ok();
        for (index, hit) in self.state.hits.iter().enumerate() {
            writeln!(&mut out, "    {{").ok();
            writeln!(
                &mut out,
                "      \"timestamp_unix\": {},",
                json_f64(hit.timestamp)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"time_local\": {},",
                json_string(&format_time(hit.timestamp))
            )
            .ok();
            writeln!(&mut out, "      \"char_id\": {},", hit.char_id).ok();
            writeln!(
                &mut out,
                "      \"char_name\": {},",
                json_string(&hit.char_name)
            )
            .ok();
            writeln!(&mut out, "      \"damage\": {},", json_f64(hit.damage)).ok();
            writeln!(
                &mut out,
                "      \"attack_type\": {},",
                hit.attack_type
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"gameplay_effect_index\": {},",
                hit.gameplay_effect_index
                    .map_or_else(|| "null".to_owned(), |value| value.to_string())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"gameplay_effect_name\": {},",
                hit.gameplay_effect_name
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"ability_name\": {},",
                hit.ability_name
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"damage_name\": {},",
                hit.damage_name
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"damage_attribute\": {},",
                hit.damage_attribute
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"follow_up_damage\": {},",
                json_f64(hit.follow_up_damage)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"follow_up_timestamp\": {},",
                hit.follow_up_timestamp
                    .map_or_else(|| "null".to_owned(), json_f64)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"follow_up_damage_name\": {},",
                hit.follow_up_damage_name
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"follow_up_attack_type\": {},",
                hit.follow_up_attack_type
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"follow_up_damage_attribute\": {},",
                hit.follow_up_damage_attribute
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"direction\": {},",
                json_string(&hit.direction)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"target_hp_before\": {},",
                json_f64(hit.target_hp_before)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"target_hp_after\": {},",
                json_f64(hit.target_hp_after)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"target_max_hp\": {},",
                json_f64(hit.target_max_hp)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"target_hp_percent\": {},",
                json_f64(hit.target_hp_percent)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"target_id\": {},",
                hit.target_id
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"target_name\": {},",
                hit.target_name
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned())
            )
            .ok();
            writeln!(&mut out, "      \"target_context\": [").ok();
            for (context_index, value) in hit.target_context.iter().enumerate() {
                writeln!(
                    &mut out,
                    "        {}{}",
                    json_string(value),
                    if context_index + 1 == hit.target_context.len() {
                        ""
                    } else {
                        ","
                    }
                )
                .ok();
            }
            writeln!(&mut out, "      ]").ok();
            writeln!(
                &mut out,
                "    }}{}",
                if index + 1 == hit_count { "" } else { "," }
            )
            .ok();
        }
        writeln!(&mut out, "  ],").ok();

        writeln!(&mut out, "  \"packets\": [").ok();
        for (index, packet) in self.state.packets.iter().enumerate() {
            writeln!(&mut out, "    {{").ok();
            writeln!(
                &mut out,
                "      \"timestamp_unix\": {},",
                json_f64(packet.timestamp)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"time_local\": {},",
                json_string(&format_time(packet.timestamp))
            )
            .ok();
            writeln!(
                &mut out,
                "      \"source\": {},",
                json_string(&packet.source.to_string())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"destination\": {},",
                json_string(&packet.destination.to_string())
            )
            .ok();
            writeln!(
                &mut out,
                "      \"direction\": {},",
                json_string(&packet.direction)
            )
            .ok();
            writeln!(&mut out, "      \"payload_len\": {},", packet.payload_len).ok();
            writeln!(
                &mut out,
                "      \"declared_ids\": {},",
                serde_json::to_string(&packet.declared_ids).unwrap_or_else(|_| "[]".to_owned())
            )
            .ok();
            writeln!(&mut out, "      \"parsed_hits\": {},", packet.parsed_hits).ok();
            writeln!(&mut out, "      \"note\": {},", json_string(&packet.note)).ok();
            writeln!(
                &mut out,
                "      \"payload_preview\": {},",
                json_string(&packet.payload_preview)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"payload_hex\": {},",
                json_string(&packet.payload_hex)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"decoded_text\": {}",
                json_string(&packet.decoded_text)
            )
            .ok();
            writeln!(
                &mut out,
                "    }}{}",
                if index + 1 == packet_count { "" } else { "," }
            )
            .ok();
        }
        writeln!(&mut out, "  ]").ok();
        writeln!(&mut out, "}}").ok();
    }

    fn selected_party_state(&self) -> Option<&PartyCombatState> {
        self.state
            .abyss
            .is_active()
            .then(|| self.state.abyss.half(self.selected_abyss_half))
    }

    fn detail_source(&self) -> (HitDetailSource, u64) {
        if self.state.abyss.is_active() {
            let party = self.state.abyss.half(self.selected_abyss_half);
            let source = match self.selected_abyss_half {
                AbyssHalf::First => HitDetailSource::AbyssFirst,
                AbyssHalf::Second => HitDetailSource::AbyssSecond,
            };
            (source, party.hits_generation)
        } else {
            (HitDetailSource::Global, self.state.hits_generation)
        }
    }

    fn note_detail_scroll_activity(&mut self, ctx: &egui::Context) {
        let scrolling = ctx.input(|input| {
            input.raw_scroll_delta != egui::Vec2::ZERO
                || input.smooth_scroll_delta != egui::Vec2::ZERO
                || ((self.hit_detail_char_id.is_some() || self.team_hit_detail_open)
                    && input.pointer.primary_down())
        });
        if scrolling {
            self.detail_last_scroll_activity = Some(Instant::now());
        }
    }

    fn detail_scroll_active(&self) -> bool {
        self.detail_last_scroll_activity
            .is_some_and(|last| last.elapsed() < DETAIL_CACHE_REFRESH_DELAY)
    }

    fn cached_skill_summaries(&mut self, char_id: u32) -> Vec<SkillDamageSummary> {
        let (source, generation) = self.detail_source();
        let key = SkillSummaryCacheKey {
            source,
            generation,
            char_id,
        };
        let structural_change = self
            .skill_summary_cache
            .key
            .as_ref()
            .is_none_or(|current| current.source != source || current.char_id != char_id);
        let generation_changed = self.skill_summary_cache.key.as_ref() != Some(&key);
        if generation_changed && self.skill_summary_cache.dirty_since.is_none() {
            self.skill_summary_cache.dirty_since = Some(Instant::now());
        }
        let refresh_due = structural_change
            || (generation_changed
                && !self.detail_scroll_active()
                && self
                    .skill_summary_cache
                    .dirty_since
                    .is_some_and(|dirty| dirty.elapsed() >= DETAIL_CACHE_REFRESH_DELAY));
        if refresh_due {
            let rows = aggregate_character_skill_damage(
                detail_hits_for_source(&self.state, source),
                char_id,
            );
            self.skill_summary_cache = SkillSummaryCache {
                key: Some(key),
                rows,
                dirty_since: None,
            };
        }
        self.skill_summary_cache.rows.clone()
    }

    fn abyss_selector(&mut self, ui: &mut egui::Ui) {
        if !self.state.abyss.is_active() {
            return;
        }
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 28.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().interact_size.y = 28.0;
                let floor = self
                    .state
                    .abyss
                    .floor
                    .map_or_else(|| "深渊".to_owned(), |floor| format!("深渊 {floor} 层"));
                ui.add(
                    egui::Label::new(
                        RichText::new(floor)
                            .size(13.0)
                            .strong()
                            .color(shadcn_foreground(self.dark_mode)),
                    )
                    .selectable(false),
                );
                ui.separator();
                ui.selectable_value(
                    &mut self.selected_abyss_half,
                    AbyssHalf::First,
                    RichText::new(AbyssHalf::First.label()).size(13.0),
                );
                ui.selectable_value(
                    &mut self.selected_abyss_half,
                    AbyssHalf::Second,
                    RichText::new(AbyssHalf::Second.label()).size(13.0),
                );
                if self.state.abyss.success_at.is_some() {
                    ui.separator();
                    ui.label(RichText::new("挑战成功").color(semantic_success(self.dark_mode)));
                }
                if self.abyss_compact_mode {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("展开").clicked() {
                            self.abyss_compact_mode = false;
                        }
                    });
                }
            },
        );
        ui.add_space(3.0);
    }

    fn summary_bar(&mut self, ui: &mut egui::Ui) {
        let (duration, dps, total_damage, total_damage_taken) =
            if let Some(party) = self.selected_party_state() {
                (
                    party.duration(),
                    party.dps(),
                    party.total_damage,
                    party.total_damage_taken,
                )
            } else {
                (
                    self.state.duration(),
                    self.state.dps(),
                    self.state.total_damage,
                    self.state.total_damage_taken,
                )
            };
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.columns(4, |columns| {
            compact_metric(
                &mut columns[0],
                "队伍 DPS",
                format_number(dps),
                theme_accent(self.dark_mode),
                true,
            );
            let total_color = columns[1].visuals().text_color();
            compact_metric(
                &mut columns[1],
                "总伤害",
                format_number(total_damage),
                total_color,
                true,
            );
            compact_metric(
                &mut columns[2],
                "总受击",
                format_number(total_damage_taken),
                semantic_danger(self.dark_mode),
                false,
            );
            let time_color = columns[3].visuals().text_color();
            compact_metric(
                &mut columns[3],
                "时间",
                format!("{duration:.1}s"),
                time_color,
                false,
            );
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.capture.is_none() && self.replay_thread.is_none() {
                if ui.add(primary_button("开始", self.dark_mode)).clicked() {
                    self.start_live();
                }
            } else if ui
                .add(
                    egui::Button::new(
                        RichText::new("停止")
                            .strong()
                            .color(semantic_danger(self.dark_mode)),
                    )
                    .stroke(Stroke::new(1.0, semantic_danger(self.dark_mode))),
                )
                .clicked()
            {
                self.stop_engine();
                self.drain_pending_events();
            }
            if ui.button("重置").clicked() {
                self.reset_combat_session();
            }
            if ui
                .button("导入 DPS 数据")
                .on_hover_text("导入队伍 DPS 数据（json），用于深渊通关预测")
                .clicked()
            {
                self.import_team_dps();
            }
            if ui
                .button("导出队伍数据")
                .on_hover_text("导出当前队伍与深渊上下队伍的 DPS（json，不含封包）")
                .clicked()
            {
                self.export_team_dps();
            }
            if ui
                .selectable_label(self.paused, if self.paused { "继续" } else { "暂停" })
                .clicked()
            {
                self.paused = !self.paused;
            }
            if self.state.abyss.is_active() && ui.button("折叠").clicked() {
                self.abyss_compact_mode = true;
            }
            if ui.button("深渊数值").clicked() {
                self.abyss_overview_open = true;
                self.abyss_overview.ensure_selection();
            }
            ui.separator();
            let status_color = status_color(&self.status, self.paused, self.dark_mode);
            let status = if self.paused {
                format!(
                    "已暂停 · 待处理 {} · 已丢弃调试封包 {}",
                    self.paused_events.len(),
                    self.dropped_debug_packets
                )
            } else if self.receiver.is_empty() {
                self.status.clone()
            } else {
                format!("{} · 队列 {}", self.status, self.receiver.len())
            };
            ui.add(
                egui::Label::new(
                    RichText::new(format!("● {status}"))
                        .size(10.5)
                        .color(status_color),
                )
                .truncate(),
            );
        });
    }

    fn animated_controls(&mut self, ui: &mut egui::Ui) {
        let expanded = !self.abyss_compact_mode || !self.state.abyss.is_active();
        let progress = ui.ctx().animate_bool_with_time(
            egui::Id::new("main_controls_expanded"),
            expanded,
            0.22,
        );
        if progress <= 0.001 {
            return;
        }

        let full_height = 35.0;
        let visible_height = full_height * ease_out_cubic(progress);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), visible_height),
            egui::Sense::hover(),
        );
        let full_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(2.0, 1.0),
            egui::vec2((rect.width() - 4.0).max(0.0), full_height - 2.0),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("animated_controls")
                .max_rect(full_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        child.set_clip_rect(rect);
        child.set_opacity(progress);
        self.controls(&mut child);
        child.separator();
    }

    fn animated_party_content(&mut self, ui: &mut egui::Ui) {
        let second_half = matches!(self.selected_abyss_half, AbyssHalf::Second);
        let phase = ui.ctx().animate_value_with_time(
            egui::Id::new("abyss_half_transition"),
            if second_half { 1.0 } else { 0.0 },
            0.22,
        );
        let visibility = if second_half { phase } else { 1.0 - phase };
        let direction = if second_half { 1.0 } else { -1.0 };
        let offset_x = direction * (1.0 - visibility) * 14.0;
        let available = ui.available_rect_before_wrap();
        let content_rect = available.translate(egui::vec2(offset_x, 0.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("animated_party_content")
                .max_rect(content_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        child.set_clip_rect(available);
        child.set_opacity(0.25 + visibility * 0.75);

        self.summary_bar(&mut child);
        child.add_space(2.0);
        child.horizontal(|ui| {
            ui.label(
                RichText::new("队伍")
                    .size(12.0)
                    .strong()
                    .color(shadcn_foreground(self.dark_mode)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        !self
                            .selected_party_state()
                            .map_or(self.state.hits.is_empty(), |party| party.hits.is_empty()),
                        egui::Button::new("队伍战斗明细"),
                    )
                    .clicked()
                {
                    self.team_hit_detail_open = true;
                    self.team_hit_detail_filter = HitDetailFilter::All;
                    self.team_hit_detail_corner_applied = false;
                }
            });
        });
        self.party_panel(&mut child);
        ui.allocate_rect(available, egui::Sense::hover());
    }

    fn import_loading_content(&self, ui: &mut egui::Ui) {
        let available_rect = ui.available_rect_before_wrap();
        ui.allocate_rect(available_rect, egui::Sense::hover());
        let card_size = egui::vec2(
            360.0_f32.min(available_rect.width()),
            150.0_f32.min(available_rect.height()),
        );
        let card_rect = egui::Rect::from_center_size(available_rect.center(), card_size);
        ui.painter().rect(
            card_rect,
            egui::CornerRadius::same(10),
            shadcn_card(self.dark_mode),
            Stroke::new(1.0, shadcn_border(self.dark_mode)),
            egui::StrokeKind::Inside,
        );
        let content_rect = card_rect.shrink(18.0);
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("import_loading_content")
                .max_rect(content_rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        content.add_space((content_rect.height() - 83.0).max(0.0) * 0.5);
        content.add(
            egui::Spinner::new()
                .size(28.0)
                .color(theme_accent(self.dark_mode)),
        );
        content.add_space(8.0);
        content.label(
            RichText::new("正在导入并解析抓包")
                .size(15.0)
                .strong()
                .color(shadcn_foreground(self.dark_mode)),
        );
        content.add_space(2.0);
        content.label(
            RichText::new(format!(
                "已解析 {} 条伤害记录 · {} 个封包",
                self.state.hits.len(),
                self.state.packets.len()
            ))
            .size(11.0)
            .color(content.visuals().weak_text_color()),
        );
    }

    fn paint_theme_transition(&mut self, ctx: &egui::Context) {
        let (Some(color), Some(started_at)) =
            (self.theme_transition_from, self.theme_transition_started_at)
        else {
            return;
        };
        let elapsed = (ctx.input(|input| input.time) - started_at).max(0.0) as f32;
        let progress = (elapsed / 0.24).clamp(0.0, 1.0);
        if progress >= 1.0 {
            self.theme_transition_from = None;
            self.theme_transition_started_at = None;
            return;
        }

        let alpha = ((1.0 - ease_out_cubic(progress)) * 96.0).round() as u8;
        let overlay = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("theme_transition_overlay"),
        ))
        .rect_filled(ctx.screen_rect(), 0.0, overlay);
        ctx.request_repaint();
    }

    fn party_panel(&mut self, ui: &mut egui::Ui) {
        let (mut rows, total_damage, hits): (Vec<_>, f64, &VecDeque<crate::model::Hit>) =
            if let Some(party) = self.selected_party_state() {
                (
                    party.stats.values().cloned().collect(),
                    party.total_damage,
                    &party.hits,
                )
            } else {
                (
                    self.state.stats.values().cloned().collect(),
                    self.state.total_damage,
                    &self.state.hits,
                )
            };
        rows.retain(|row| is_party_member_row(row, hits));
        rows.sort_by(|left, right| right.damage.total_cmp(&left.damage));
        let row_height = party_row_height(ui.available_height(), rows.len());
        if rows.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 40.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(RichText::new("等待伤害数据").color(ui.visuals().weak_text_color()));
                },
            );
            return;
        }

        let row_spacing = 5.0;
        let total_height =
            row_height * rows.len() as f32 + row_spacing * rows.len().saturating_sub(1) as f32;
        let (container, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), total_height),
            egui::Sense::hover(),
        );
        let stride = row_height + row_spacing;
        for (index, row) in rows.iter().enumerate().rev() {
            let target_y = index as f32 * stride;
            let animated_y = ui.ctx().animate_value_with_time(
                egui::Id::new(("party_rank_y", row.char_id)),
                target_y,
                0.24,
            );
            let row_rect = egui::Rect::from_min_size(
                egui::pos2(container.left(), container.top() + animated_y),
                egui::vec2(container.width(), row_height),
            );
            let mut row_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("party_row", row.char_id))
                    .max_rect(row_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.draw_party_row(&mut row_ui, row, index, total_damage, row_height);
        }
    }

    fn draw_party_row(
        &mut self,
        ui: &mut egui::Ui,
        row: &CharacterStats,
        index: usize,
        total_damage: f64,
        row_height: f32,
    ) {
        let color = readable_accent(
            character_color(row.char_id, &self.characters, index),
            self.dark_mode,
        );
        let avatar_texture = self
            .characters
            .get(&row.char_id)
            .and_then(|character| character.avatar.as_deref())
            .and_then(|avatar| self.avatar_textures.get(avatar));
        let attribute_texture = self
            .characters
            .get(&row.char_id)
            .and_then(|character| character.attribute.as_deref())
            .and_then(|attribute| self.attribute_textures.get(attribute));
        let share = if total_damage > 0.0 {
            row.damage / total_damage * 100.0
        } else {
            0.0
        };
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::click(),
        );
        let hover =
            ui.ctx()
                .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
        let card_fill = mix_color(
            shadcn_card(self.dark_mode),
            shadcn_card_hover(self.dark_mode),
            hover,
        );
        ui.painter().rect_filled(rect, 6.0, card_fill);
        ui.painter().rect_stroke(
            rect,
            6.0,
            Stroke::new(
                1.0,
                mix_color(
                    shadcn_border(self.dark_mode),
                    color.gamma_multiply(0.72),
                    hover,
                ),
            ),
            egui::StrokeKind::Inside,
        );
        let contribution_track = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 7.0, rect.bottom() - 4.0),
            egui::pos2(rect.right() - 7.0, rect.bottom() - 2.0),
        );
        let animated_share = ui.ctx().animate_value_with_time(
            response.id.with("share"),
            (share as f32 / 100.0).clamp(0.0, 1.0),
            0.25,
        );
        ui.painter()
            .rect_filled(contribution_track, 1.0, shadcn_muted(self.dark_mode));
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                contribution_track.min,
                egui::vec2(
                    contribution_track.width() * animated_share,
                    contribution_track.height(),
                ),
            ),
            1.0,
            color,
        );
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                rect.left_top(),
                egui::pos2(rect.left() + 3.0 + hover, rect.bottom()),
            ),
            6.0,
            color,
        );
        if let Some(texture) = attribute_texture {
            let attribute_rect = pixel_aligned_rect(
                egui::pos2(rect.left(), rect.center().y - 12.0),
                24.0,
                ui.ctx().pixels_per_point(),
            );
            ui.put(
                attribute_rect,
                egui::Image::new((texture.id(), attribute_rect.size())),
            );
        } else {
            ui.painter().text(
                egui::pos2(rect.left() + 10.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                format!("#{}", index + 1),
                egui::FontId::monospace(9.5),
                color,
            );
        }
        let avatar_size = (row_height - 8.0).clamp(32.0, 40.0);
        let avatar_rect = pixel_aligned_rect(
            egui::pos2(rect.left() + 24.0, rect.center().y - avatar_size * 0.5),
            avatar_size,
            ui.ctx().pixels_per_point(),
        );
        let avatar_border = if self.dark_mode {
            Color32::from_rgb(78, 82, 92)
        } else {
            Color32::from_rgb(210, 213, 220)
        };
        ui.painter().rect_filled(avatar_rect, 8.0, avatar_border);
        if let Some(texture) = avatar_texture {
            ui.put(
                avatar_rect,
                egui::Image::new((texture.id(), avatar_rect.size())).corner_radius(8),
            );
            ui.painter().rect_stroke(
                avatar_rect,
                8.0,
                Stroke::new(1.0, avatar_border),
                egui::StrokeKind::Inside,
            );
        } else {
            ui.painter()
                .rect_filled(avatar_rect, 8.0, color.gamma_multiply(0.82));
            ui.painter().text(
                avatar_rect.center(),
                egui::Align2::CENTER_CENTER,
                row.name.chars().next().unwrap_or('?').to_string(),
                egui::FontId::proportional(14.0),
                contrast_text(color),
            );
        }
        let text_left = avatar_rect.right() + 8.0;
        ui.painter().text(
            egui::pos2(text_left, rect.center().y - 8.0),
            egui::Align2::LEFT_CENTER,
            &row.name,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );
        ui.painter().text(
            egui::pos2(text_left, rect.center().y + 9.0),
            egui::Align2::LEFT_CENTER,
            format!("{}次 · {:.1}s", row.hits, row.duration()),
            egui::FontId::monospace(10.5),
            ui.visuals().weak_text_color(),
        );
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y - 8.0),
            egui::Align2::RIGHT_CENTER,
            format!("{} DPS", format_number(row.dps())),
            egui::FontId::monospace(12.0),
            theme_accent(self.dark_mode),
        );
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y + 9.0),
            egui::Align2::RIGHT_CENTER,
            format!(
                "伤害 {} · 占比 {share:.1}% · 受击 {}",
                format_number(row.damage),
                format_number(row.damage_taken)
            ),
            egui::FontId::monospace(10.5),
            ui.visuals().weak_text_color(),
        );
        if response.on_hover_text("在独立窗口查看战斗明细").clicked() {
            self.hit_detail_char_id = Some(row.char_id);
            self.hit_detail_filter = HitDetailFilter::All;
            self.hit_detail_skill_filter.clear();
            self.hit_detail_corner_applied = false;
        }
    }

    fn character_hits(
        &mut self,
        ui: &mut egui::Ui,
        char_id: u32,
        filter: HitDetailFilter,
        skill_filter: &str,
    ) {
        let scrollbar_width = ui.style().spacing.scroll.allocated_width().max(10.0);
        let content_width = (ui.available_width() - scrollbar_width - 4.0).max(0.0);
        let layout = CharacterHitLayout::new(content_width);
        let (source, generation) = self.detail_source();
        let key = HitDetailCacheKey {
            source,
            char_id: Some(char_id),
            filter,
            skill_filter: skill_filter.to_owned(),
            limit: MAX_DETAIL_HITS,
        };
        let structural_change = self.character_hit_cache.key.as_ref() != Some(&key);
        let generation_changed = self.character_hit_cache.generation != generation;
        if generation_changed && self.character_hit_cache.dirty_since.is_none() {
            self.character_hit_cache.dirty_since = Some(Instant::now());
        }
        let refresh_due = structural_change
            || (generation_changed
                && !self.detail_scroll_active()
                && self
                    .character_hit_cache
                    .dirty_since
                    .is_some_and(|dirty| dirty.elapsed() >= DETAIL_CACHE_REFRESH_DELAY));
        if refresh_due {
            self.character_hit_cache = build_hit_detail_cache(
                detail_hits_for_source(&self.state, source),
                generation,
                key,
            );
        }
        let hits = detail_hits_for_source(&self.state, source);
        let filtered_count = self.character_hit_cache.filtered_count;
        let max_damage = self.character_hit_cache.max_damage;
        show_detail_limit_notice(ui, filtered_count);
        draw_character_hit_header(ui, layout);
        let hit_count = self.character_hit_cache.rows.len();
        if hit_count == 0 {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 72.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new("当前筛选条件下暂无命中记录")
                            .color(ui.visuals().weak_text_color()),
                    );
                },
            );
            return;
        }
        let output = egui::ScrollArea::vertical()
            .id_salt(("character_hits", char_id))
            .max_height(ui.available_height())
            .show_rows(ui, DETAIL_HIT_ROW_HEIGHT, hit_count, |ui, visible_rows| {
                let visible_count = visible_rows.end.saturating_sub(visible_rows.start);
                for row in self.character_hit_cache.rows[visible_rows]
                    .iter()
                    .take(visible_count)
                {
                    if let Some(hit) = resolve_cached_hit(
                        hits,
                        row,
                        self.character_hit_cache.source_len,
                        generation.saturating_sub(self.character_hit_cache.generation),
                    ) {
                        let damage_digits = damage_digit_textures_for_hit(
                            hit,
                            &self.characters,
                            &self.damage_digit_textures,
                        );
                        let follow_up_digits = follow_up_damage_digit_textures_for_hit(
                            hit,
                            &self.damage_digit_textures,
                        );
                        draw_character_hit_row(
                            ui,
                            layout,
                            hit,
                            max_damage,
                            damage_digits,
                            follow_up_digits,
                            &self.reaction_textures,
                        );
                    }
                }
            });
        if self
            .character_hit_cache
            .last_scroll_offset
            .is_some_and(|previous| (previous - output.state.offset.y).abs() > 0.5)
        {
            self.detail_last_scroll_activity = Some(Instant::now());
        }
        self.character_hit_cache.last_scroll_offset = Some(output.state.offset.y);
    }

    fn team_hits(&mut self, ui: &mut egui::Ui, filter: HitDetailFilter) {
        let scrollbar_width = ui.style().spacing.scroll.allocated_width().max(10.0);
        let content_width = (ui.available_width() - scrollbar_width - 4.0).max(0.0);
        let layout = TeamHitLayout::new(content_width);
        let (source, generation) = self.detail_source();
        let key = HitDetailCacheKey {
            source,
            char_id: None,
            filter,
            skill_filter: String::new(),
            limit: MAX_DETAIL_HITS,
        };
        let structural_change = self.team_hit_cache.key.as_ref() != Some(&key);
        let generation_changed = self.team_hit_cache.generation != generation;
        if generation_changed && self.team_hit_cache.dirty_since.is_none() {
            self.team_hit_cache.dirty_since = Some(Instant::now());
        }
        let refresh_due = structural_change
            || (generation_changed
                && !self.detail_scroll_active()
                && self
                    .team_hit_cache
                    .dirty_since
                    .is_some_and(|dirty| dirty.elapsed() >= DETAIL_CACHE_REFRESH_DELAY));
        if refresh_due {
            self.team_hit_cache = build_hit_detail_cache(
                detail_hits_for_source(&self.state, source),
                generation,
                key,
            );
        }
        let hits = detail_hits_for_source(&self.state, source);
        let filtered_count = self.team_hit_cache.filtered_count;
        let max_damage = self.team_hit_cache.max_damage;
        show_detail_limit_notice(ui, filtered_count);
        draw_team_hit_header(ui, layout);
        if self.team_hit_cache.rows.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 72.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new("当前筛选条件下暂无命中记录")
                            .color(ui.visuals().weak_text_color()),
                    );
                },
            );
            return;
        }
        let hit_count = self.team_hit_cache.rows.len();
        let output = egui::ScrollArea::vertical()
            .id_salt((
                "team_hits",
                matches!(self.selected_abyss_half, AbyssHalf::Second),
            ))
            .max_height(ui.available_height())
            .show_rows(ui, DETAIL_HIT_ROW_HEIGHT, hit_count, |ui, visible_rows| {
                let visible_count = visible_rows.end.saturating_sub(visible_rows.start);
                for row in self.team_hit_cache.rows[visible_rows]
                    .iter()
                    .take(visible_count)
                {
                    let Some(hit) = resolve_cached_hit(
                        hits,
                        row,
                        self.team_hit_cache.source_len,
                        generation.saturating_sub(self.team_hit_cache.generation),
                    ) else {
                        continue;
                    };
                    let color = readable_accent(
                        character_color(hit.char_id, &self.characters, 0),
                        self.dark_mode,
                    );
                    let avatar_texture = self
                        .characters
                        .get(&hit.char_id)
                        .and_then(|character| character.avatar.as_deref())
                        .and_then(|avatar| self.avatar_textures.get(avatar));
                    let damage_digits = damage_digit_textures_for_hit(
                        hit,
                        &self.characters,
                        &self.damage_digit_textures,
                    );
                    let follow_up_digits =
                        follow_up_damage_digit_textures_for_hit(hit, &self.damage_digit_textures);
                    draw_team_hit_row(
                        ui,
                        layout,
                        hit,
                        max_damage,
                        color,
                        TeamHitRowAssets {
                            avatar_texture,
                            damage_digits,
                            follow_up_damage_digits: follow_up_digits,
                            reaction_textures: &self.reaction_textures,
                        },
                    );
                }
            });
        if self
            .team_hit_cache
            .last_scroll_offset
            .is_some_and(|previous| (previous - output.state.offset.y).abs() > 0.5)
        {
            self.detail_last_scroll_activity = Some(Instant::now());
        }
        self.team_hit_cache.last_scroll_offset = Some(output.state.offset.y);
    }

    fn team_hit_detail_panel(&mut self, ctx: &egui::Context) {
        let (detail_source, _) = self.detail_source();
        let direction_summary =
            summarize_hit_directions(detail_hits_for_source(&self.state, detail_source));
        let qte_summaries =
            summarize_qte_type_filters(detail_hits_for_source(&self.state, detail_source), None);
        if !hit_detail_filter_available(&self.team_hit_detail_filter, &qte_summaries) {
            self.team_hit_detail_filter = HitDetailFilter::All;
        }
        let (total_damage, total_damage_taken, duration, dps, outgoing_count, incoming_count) =
            if let Some(party) = self.selected_party_state() {
                (
                    party.total_damage,
                    party.total_damage_taken,
                    party.duration(),
                    party.dps(),
                    party
                        .stats
                        .values()
                        .map(|row| row.hits as usize)
                        .sum::<usize>(),
                    party
                        .stats
                        .values()
                        .map(|row| row.hits_taken as usize)
                        .sum::<usize>(),
                )
            } else {
                (
                    self.state.total_damage,
                    self.state.total_damage_taken,
                    self.state.duration(),
                    self.state.dps(),
                    self.state
                        .stats
                        .values()
                        .map(|row| row.hits as usize)
                        .sum::<usize>(),
                    self.state
                        .stats
                        .values()
                        .map(|row| row.hits_taken as usize)
                        .sum::<usize>(),
                )
            };
        let title = if self.state.abyss.is_active() {
            format!("队伍战斗明细 - {}", self.selected_abyss_half.label())
        } else {
            "队伍战斗明细".to_owned()
        };
        let close_requested = ctx.show_viewport_immediate(
            team_hit_detail_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title(&title)
                .with_inner_size(TEAM_HIT_DETAIL_WINDOW_BASE_SIZE)
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                // Not transparent and not resizable on purpose — see
                // window_scale_stepper for the Windows resize-crash rationale.
                .with_resizable(false),
            |ctx, _class| {
                if !self.team_hit_detail_corner_applied {
                    apply_rounding_to_process_windows();
                    self.team_hit_detail_corner_applied = true;
                }
                let mut close_clicked = false;
                egui::TopBottomPanel::top("team_hit_detail_title_bar")
                    .exact_height(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show(ctx, |ui| {
                        close_clicked = secondary_title_bar(
                            ui,
                            &title,
                            &mut self.team_hit_detail_window_scale,
                            TEAM_HIT_DETAIL_WINDOW_BASE_SIZE,
                        );
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(shadcn_card(self.dark_mode))
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .corner_radius(10)
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                let text_color = ui.visuals().text_color();
                                draw_hit_metric_row(
                                    ui,
                                    [
                                        (
                                            "总输出",
                                            format_number(total_damage),
                                            theme_accent(self.dark_mode),
                                        ),
                                        ("DPS", format_number(dps), theme_accent(self.dark_mode)),
                                        ("输出次数", outgoing_count.to_string(), text_color),
                                        (
                                            "总受击",
                                            format_number(total_damage_taken),
                                            semantic_danger(self.dark_mode),
                                        ),
                                        ("战斗时间", format!("{duration:.1}s"), text_color),
                                    ],
                                );
                                draw_direction_summary(ui, direction_summary);
                            });
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().interact_size.y = 28.0;
                            ui.spacing_mut().button_padding.y = 4.0;
                            ui.add_sized(
                                egui::vec2(92.0, 28.0),
                                egui::Label::new(
                                    RichText::new("命中类型")
                                        .strong()
                                        .color(ui.visuals().weak_text_color()),
                                ),
                            );
                            ui.selectable_value(
                                &mut self.team_hit_detail_filter,
                                HitDetailFilter::All,
                                format!("全部 {}", outgoing_count + incoming_count),
                            );
                            ui.selectable_value(
                                &mut self.team_hit_detail_filter,
                                HitDetailFilter::Outgoing,
                                format!("输出 {outgoing_count}"),
                            );
                            ui.selectable_value(
                                &mut self.team_hit_detail_filter,
                                HitDetailFilter::Incoming,
                                format!("受击 {incoming_count}"),
                            );
                        });
                        ui.add_space(4.0);
                        draw_qte_damage_summary(
                            ui,
                            &qte_summaries,
                            total_damage,
                            &mut self.team_hit_detail_filter,
                        );
                        ui.add_space(4.0);
                        ui.separator();
                        self.team_hits(ui, self.team_hit_detail_filter.clone());
                    });
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.team_hit_detail_open = false;
            self.team_hit_detail_corner_applied = false;
        }
    }

    fn abyss_overview_panel(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.show_viewport_immediate(
            abyss_overview_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("深渊怪物数值")
                .with_inner_size(ABYSS_WINDOW_BASE_SIZE)
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                // Not transparent and not resizable on purpose — see
                // window_scale_stepper for the Windows resize-crash rationale.
                .with_resizable(false),
            |ctx, _class| {
                if !self.abyss_overview_corner_applied {
                    apply_rounding_to_process_windows();
                    self.abyss_overview_corner_applied = true;
                }
                let mut close_clicked = false;
                egui::TopBottomPanel::top("abyss_overview_title_bar")
                    .exact_height(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show(ctx, |ui| {
                        close_clicked = secondary_title_bar(
                            ui,
                            "深渊怪物数值",
                            &mut self.abyss_window_scale,
                            ABYSS_WINDOW_BASE_SIZE,
                        );
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show(ctx, |ui| {
                        self.abyss_overview_contents(ui);
                    });
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.abyss_overview_open = false;
            self.abyss_overview_corner_applied = false;
        }
    }

    fn abyss_overview_contents(&mut self, ui: &mut egui::Ui) {
        self.abyss_overview.ensure_selection();
        let Some(dataset) = self.abyss_overview.dataset.as_ref() else {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), ui.available_height()),
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.label(
                        RichText::new("深渊怪物数值表未加载")
                            .size(18.0)
                            .strong()
                            .color(semantic_danger(self.dark_mode)),
                    );
                    if let Some(error) = &self.abyss_overview.load_error {
                        ui.add_space(6.0);
                        ui.label(RichText::new(error).color(ui.visuals().weak_text_color()));
                    }
                    ui.add_space(10.0);
                    if ui.button("重新加载").clicked() {
                        self.abyss_overview.reload();
                    }
                },
            );
            return;
        };
        let season_count = dataset.seasons.len();
        let season_nav = dataset
            .seasons
            .iter()
            .map(|season| {
                (
                    season.season,
                    season
                        .floors
                        .iter()
                        .map(|floor| (floor.floor, floor.monsters.len()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let floor_count = season_nav
            .iter()
            .map(|(_, floors)| floors.len())
            .sum::<usize>();
        let selected_floor = self
            .abyss_overview
            .selected_season
            .zip(self.abyss_overview.selected_floor)
            .and_then(|(season, floor)| dataset.floor(season, floor).cloned());
        let selected_monster = self
            .abyss_overview
            .selected_monster_pack_id
            .as_deref()
            .and_then(|pack_id| dataset.monster(pack_id))
            .cloned();
        let total_monsters = dataset
            .seasons
            .iter()
            .flat_map(|season| season.floors.iter())
            .map(|floor| floor.monsters.len())
            .sum::<usize>();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "共 {} 期 · {} 层 · {} 条深渊怪物数值",
                    season_count, floor_count, total_monsters
                ))
                .size(12.0)
                .strong()
                .color(shadcn_foreground(self.dark_mode)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("重新加载").clicked() {
                    self.abyss_overview.reload();
                }
            });
        });
        ui.add_space(8.0);

        let available = ui.available_size();
        let (main_rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
        let nav_width = 170.0_f32.min((main_rect.width() * 0.26).max(140.0));
        let gap = 12.0;
        let nav_rect =
            egui::Rect::from_min_size(main_rect.min, egui::vec2(nav_width, main_rect.height()));
        let separator_x = nav_rect.right() + gap * 0.5;
        ui.painter().vline(
            separator_x,
            main_rect.y_range(),
            Stroke::new(1.0, shadcn_border(self.dark_mode)),
        );
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(nav_rect.right() + gap, main_rect.top()),
            main_rect.right_bottom(),
        );

        let mut nav_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(nav_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        nav_ui.set_clip_rect(nav_rect);
        draw_abyss_floor_nav(
            &mut nav_ui,
            &season_nav,
            &mut self.abyss_overview.selected_season,
            &mut self.abyss_overview.selected_floor,
            &mut self.abyss_overview.selected_monster_pack_id,
            &mut self.abyss_overview.expanded_season,
        );

        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(content_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        content_ui.set_clip_rect(content_rect);
        let Some(floor) = selected_floor else {
            content_ui.label(
                RichText::new("请选择深渊层级").color(content_ui.visuals().weak_text_color()),
            );
            return;
        };
        self.abyss_floor_contents(&mut content_ui, &floor, selected_monster.as_ref());
    }

    /// Snapshot the current combat session into a prediction team. Returns `None`
    /// when there is no measured output yet (DPS is zero), so callers can keep the
    /// "import data first" prompt.
    fn snapshot_current_team(&self) -> Option<TeamDps> {
        let dps = self.state.dps();
        if dps <= 0.0 {
            return None;
        }
        // A single capture can contain both abyss teams; keep only the top
        // TEAM_DPS_MAX_MEMBERS contributors so one slot is never overfilled.
        let mut members: Vec<&CharacterStats> = self
            .state
            .stats
            .values()
            .filter(|stats| stats.char_id != 0 && stats.char_id < 900_000 && stats.damage > 0.0)
            .collect();
        members.sort_by(|left, right| right.damage.total_cmp(&left.damage));
        members.truncate(TEAM_DPS_MAX_MEMBERS);
        Some(TeamDps {
            dps,
            members: members
                .into_iter()
                .map(|stats| TeamDpsMember {
                    id: stats.char_id,
                    dps: stats.dps(),
                    name: stats.name.clone(),
                })
                .collect(),
        })
    }

    fn apply_line_prediction_action(&mut self, upper: bool, action: LinePredictionAction) {
        let team = match action {
            LinePredictionAction::None => return,
            LinePredictionAction::ImportCurrent => self.snapshot_current_team(),
            LinePredictionAction::Clear => None,
            LinePredictionAction::ImportFile => match self.pick_and_load_team_dps() {
                // For one line, prefer that line's team from the file, then the
                // matching upper/lower, then the single team.
                Some(export) => {
                    let preferred = if upper { export.upper } else { export.lower };
                    let team = preferred.or(export.single);
                    if team.is_none() {
                        self.last_error = Some("DPS 数据文件里没有可用于该行的队伍".to_owned());
                        return;
                    }
                    team
                }
                None => return,
            },
        };
        if upper {
            self.abyss_overview.upper_team = team;
        } else {
            self.abyss_overview.lower_team = team;
        }
    }

    /// Open a file dialog and parse a team DPS data file. Returns `None` when the
    /// user cancels; sets `last_error` on a read/parse failure.
    fn pick_and_load_team_dps(&mut self) -> Option<TeamDpsExport> {
        let path = rfd::FileDialog::new()
            .add_filter("NTE 队伍数据", &["json"])
            .pick_file()?;
        match std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<TeamDpsExport>(&text).map_err(|error| error.to_string())
            }) {
            Ok(export) => Some(export),
            Err(error) => {
                self.last_error = Some(format!("导入 DPS 数据失败：{error}"));
                None
            }
        }
    }

    /// Main-window "导入 DPS 数据": load a team DPS file into the abyss prediction.
    /// A dual file fills 上行线/下行线 separately; a single-team file fills both.
    fn import_team_dps(&mut self) {
        let Some(export) = self.pick_and_load_team_dps() else {
            return;
        };
        let upper = export.upper.clone().or_else(|| export.single.clone());
        let lower = export.lower.clone().or_else(|| export.single.clone());
        if upper.is_none() && lower.is_none() {
            self.last_error = Some("DPS 数据文件里没有队伍数据".to_owned());
            return;
        }
        if upper.is_some() {
            self.abyss_overview.upper_team = upper;
        }
        if lower.is_some() {
            self.abyss_overview.lower_team = lower;
        }
        self.status = "已导入 DPS 队伍数据".to_owned();
    }

    /// Main-window "导出队伍数据": write a compact JSON with whatever teams exist
    /// (current session as `single`, plus the abyss upper/lower if set). No
    /// packets or per-hit data, latest DPS only, serialized without indentation.
    fn export_team_dps(&mut self) {
        let single = self.snapshot_current_team();
        let upper = self.abyss_overview.upper_team.clone();
        let lower = self.abyss_overview.lower_team.clone();
        if single.is_none() && upper.is_none() && lower.is_none() {
            self.last_error = Some("没有可导出的队伍数据，请先抓包或设置深渊队伍".to_owned());
            return;
        }
        let export = TeamDpsExport {
            version: TEAM_DPS_EXPORT_VERSION,
            single,
            upper,
            lower,
        };
        let Ok(json) = serde_json::to_string(&export) else {
            self.last_error = Some("序列化队伍数据失败".to_owned());
            return;
        };
        let default_name = format!("nte_team_dps_{}.json", Local::now().format("%Y%m%d_%H%M%S"));
        let Some(path) = rfd::FileDialog::new()
            .add_filter("NTE 队伍数据", &["json"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return;
        };
        match atomic_write_text(&path, &json) {
            Ok(()) => self.status = "已导出队伍数据".to_owned(),
            Err(error) => self.last_error = Some(format!("导出队伍数据失败：{error}")),
        }
    }

    fn abyss_floor_contents(
        &mut self,
        ui: &mut egui::Ui,
        floor: &AbyssFloor,
        selected_monster: Option<&AbyssMonsterEntry>,
    ) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("第 {} 期 · 第 {} 层", floor.season, floor.floor))
                    .size(16.0)
                    .strong()
                    .color(shadcn_foreground(self.dark_mode)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.abyss_overview.search)
                        .hint_text("搜索怪物 / ID")
                        .desired_width(190.0),
                );
                let has_team = self.abyss_overview.upper_team.is_some()
                    || self.abyss_overview.lower_team.is_some();
                if ui
                    .add_enabled(has_team, egui::Button::new("交换上下队伍"))
                    .on_hover_text("交换上行线与下行线的预测队伍")
                    .clicked()
                {
                    self.abyss_overview.swap_teams();
                }
            });
        });
        ui.add_space(4.0);
        let query = self.abyss_overview.search.trim().to_ascii_lowercase();
        let monsters = floor
            .monsters
            .iter()
            .filter(|monster| {
                query.is_empty()
                    || monster.name.to_ascii_lowercase().contains(&query)
                    || monster.pack_id.to_ascii_lowercase().contains(&query)
                    || monster.monster_id.to_ascii_lowercase().contains(&query)
            })
            .collect::<Vec<_>>();
        if monsters.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 92.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(RichText::new("没有匹配的怪物").color(ui.visuals().weak_text_color()));
                },
            );
        } else {
            let mut upper_line = Vec::new();
            let mut lower_line = Vec::new();
            let mut unassigned = Vec::new();
            for monster in monsters {
                match monster.half {
                    Some(0) => upper_line.push(monster),
                    Some(1) => lower_line.push(monster),
                    _ => unassigned.push(monster),
                }
            }
            let selected_pack_id = self.abyss_overview.selected_monster_pack_id.clone();
            let upper_team = self.abyss_overview.upper_team.clone();
            let lower_team = self.abyss_overview.lower_team.clone();
            let can_import = self.state.dps() > 0.0;
            let mut upper_action = LinePredictionAction::None;
            let mut lower_action = LinePredictionAction::None;
            if !upper_line.is_empty() || !lower_line.is_empty() {
                upper_action = draw_abyss_line_section(
                    ui,
                    "上行线",
                    &upper_line,
                    selected_pack_id.as_deref(),
                    &mut self.abyss_overview.selected_monster_pack_id,
                    &self.monster_textures,
                    self.dark_mode,
                    Some(LinePredictionView {
                        team: upper_team.as_ref(),
                        line_hp: abyss_line_hp_total(&upper_line),
                        can_import,
                        avatar_textures: &self.avatar_textures,
                        characters: &self.characters,
                    }),
                );
                ui.add_space(6.0);
                lower_action = draw_abyss_line_section(
                    ui,
                    "下行线",
                    &lower_line,
                    selected_pack_id.as_deref(),
                    &mut self.abyss_overview.selected_monster_pack_id,
                    &self.monster_textures,
                    self.dark_mode,
                    Some(LinePredictionView {
                        team: lower_team.as_ref(),
                        line_hp: abyss_line_hp_total(&lower_line),
                        can_import,
                        avatar_textures: &self.avatar_textures,
                        characters: &self.characters,
                    }),
                );
            }
            if !unassigned.is_empty() {
                if !upper_line.is_empty() || !lower_line.is_empty() {
                    ui.add_space(6.0);
                }
                draw_abyss_line_section(
                    ui,
                    "整层配置",
                    &unassigned,
                    selected_pack_id.as_deref(),
                    &mut self.abyss_overview.selected_monster_pack_id,
                    &self.monster_textures,
                    self.dark_mode,
                    None,
                );
            }
            self.apply_line_prediction_action(true, upper_action);
            self.apply_line_prediction_action(false, lower_action);
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        if let Some(monster) = selected_monster {
            draw_abyss_monster_detail(
                ui,
                monster,
                monster_texture(&self.monster_textures, &monster.monster_id),
                self.dark_mode,
                ui.available_height(),
                &self.abyss_overview.stat_display_names,
            );
        } else {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), ui.available_height()),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new("点击怪物卡片查看全部数值字段")
                            .color(ui.visuals().weak_text_color()),
                    );
                },
            );
        }
    }

    fn hit_detail_panel(&mut self, ctx: &egui::Context, char_id: u32) {
        let stats = if let Some(party) = self.selected_party_state() {
            party.stats.get(&char_id).cloned()
        } else {
            self.state.stats.get(&char_id).cloned()
        };
        let Some(stats) = stats else {
            self.hit_detail_char_id = None;
            self.hit_detail_corner_applied = false;
            return;
        };
        let outgoing_count = stats.hits as usize;
        let incoming_count = stats.hits_taken as usize;
        let (detail_source, _) = self.detail_source();
        let direction_summary = summarize_hit_directions(
            detail_hits_for_source(&self.state, detail_source)
                .iter()
                .filter(|hit| hit.char_id == char_id),
        );
        let qte_summaries = summarize_qte_type_filters(
            detail_hits_for_source(&self.state, detail_source),
            Some(char_id),
        );
        if !hit_detail_filter_available(&self.hit_detail_filter, &qte_summaries) {
            self.hit_detail_filter = HitDetailFilter::All;
        }
        let skill_summaries = self.cached_skill_summaries(char_id);
        if !self.hit_detail_skill_filter.is_empty()
            && !skill_summaries
                .iter()
                .any(|summary| summary.name == self.hit_detail_skill_filter)
        {
            self.hit_detail_skill_filter.clear();
        }
        let avatar_texture = self
            .characters
            .get(&char_id)
            .and_then(|character| character.avatar.as_deref())
            .and_then(|avatar| self.avatar_textures.get(avatar))
            .cloned();
        let character_color = readable_accent(
            character_color(char_id, &self.characters, 0),
            self.dark_mode,
        );
        let title = format!("{} - 战斗明细", stats.name);
        let close_requested = ctx.show_viewport_immediate(
            hit_detail_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title(&title)
                .with_inner_size(HIT_DETAIL_WINDOW_BASE_SIZE)
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                // Not transparent and not resizable on purpose — see
                // window_scale_stepper for the Windows resize-crash rationale.
                .with_resizable(false),
            |ctx, _class| {
                if !self.hit_detail_corner_applied {
                    apply_rounding_to_process_windows();
                    self.hit_detail_corner_applied = true;
                }
                let mut close_clicked = false;
                egui::TopBottomPanel::top("hit_detail_title_bar")
                    .exact_height(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show(ctx, |ui| {
                        close_clicked = secondary_title_bar(
                            ui,
                            &title,
                            &mut self.hit_detail_window_scale,
                            HIT_DETAIL_WINDOW_BASE_SIZE,
                        );
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(shadcn_card(self.dark_mode))
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .corner_radius(10)
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(160.0, 62.0),
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                let avatar_rect = pixel_aligned_rect(
                                                    ui.cursor().min,
                                                    62.0,
                                                    ui.ctx().pixels_per_point(),
                                                );
                                                ui.allocate_rect(avatar_rect, egui::Sense::hover());
                                                ui.painter().rect_filled(
                                                    avatar_rect,
                                                    10.0,
                                                    character_color.gamma_multiply(0.8),
                                                );
                                                if let Some(texture) = &avatar_texture {
                                                    ui.put(
                                                        avatar_rect,
                                                        egui::Image::new((
                                                            texture.id(),
                                                            avatar_rect.size(),
                                                        ))
                                                        .corner_radius(10),
                                                    );
                                                } else {
                                                    ui.painter().text(
                                                        avatar_rect.center(),
                                                        egui::Align2::CENTER_CENTER,
                                                        stats
                                                            .name
                                                            .chars()
                                                            .next()
                                                            .unwrap_or('?')
                                                            .to_string(),
                                                        egui::FontId::proportional(25.0),
                                                        contrast_text(character_color),
                                                    );
                                                }
                                                ui.add_space(4.0);
                                                ui.vertical(|ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&stats.name)
                                                                .size(20.0)
                                                                .strong()
                                                                .color(shadcn_foreground(
                                                                    self.dark_mode,
                                                                )),
                                                        )
                                                        .truncate(),
                                                    );
                                                    ui.label(
                                                        RichText::new(format!("角色 ID {char_id}"))
                                                            .size(11.0)
                                                            .color(ui.visuals().weak_text_color()),
                                                    );
                                                });
                                            },
                                        );
                                        ui.add_space(12.0);
                                        let text_color = ui.visuals().text_color();
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(ui.available_width(), 62.0),
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                draw_hit_metric_row(
                                                    ui,
                                                    [
                                                        (
                                                            "总输出",
                                                            format_number(stats.damage),
                                                            theme_accent(self.dark_mode),
                                                        ),
                                                        (
                                                            "DPS",
                                                            format_number(stats.dps()),
                                                            theme_accent(self.dark_mode),
                                                        ),
                                                        (
                                                            "输出次数",
                                                            outgoing_count.to_string(),
                                                            text_color,
                                                        ),
                                                        (
                                                            "总受击",
                                                            format_number(stats.damage_taken),
                                                            semantic_danger(self.dark_mode),
                                                        ),
                                                        (
                                                            "战斗时间",
                                                            format!("{:.1}s", stats.duration()),
                                                            text_color,
                                                        ),
                                                    ],
                                                );
                                            },
                                        );
                                    });
                                    draw_direction_summary(ui, direction_summary);
                                });
                            });
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().interact_size.y = 28.0;
                            ui.spacing_mut().button_padding.y = 4.0;
                            ui.add_sized(
                                egui::vec2(92.0, 28.0),
                                egui::Label::new(
                                    RichText::new("伤害类型")
                                        .strong()
                                        .color(ui.visuals().weak_text_color()),
                                ),
                            );
                            ui.selectable_value(
                                &mut self.hit_detail_filter,
                                HitDetailFilter::All,
                                format!("全部 {}", outgoing_count + incoming_count),
                            );
                            ui.selectable_value(
                                &mut self.hit_detail_filter,
                                HitDetailFilter::Outgoing,
                                format!("输出 {outgoing_count}"),
                            );
                            ui.selectable_value(
                                &mut self.hit_detail_filter,
                                HitDetailFilter::Incoming,
                                format!("受击 {incoming_count}"),
                            );
                            ui.separator();
                            ui.add_sized(
                                egui::vec2(92.0, 28.0),
                                egui::Label::new(
                                    RichText::new("具体招式")
                                        .strong()
                                        .color(ui.visuals().weak_text_color()),
                                ),
                            );
                            ui.scope(|ui| {
                                ui.spacing_mut().interact_size.y = 27.0;
                                ui.spacing_mut().button_padding.y = 2.0;
                                egui::ComboBox::from_id_salt(("hit_skill_filter", char_id))
                                    .width(240.0)
                                    .selected_text(if self.hit_detail_skill_filter.is_empty() {
                                        "全部招式".to_owned()
                                    } else {
                                        self.hit_detail_skill_filter.clone()
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.hit_detail_skill_filter,
                                            String::new(),
                                            "全部招式",
                                        );
                                        for summary in &skill_summaries {
                                            ui.selectable_value(
                                                &mut self.hit_detail_skill_filter,
                                                summary.name.clone(),
                                                format!("{}  {}次", summary.name, summary.hits),
                                            );
                                        }
                                    });
                            });
                        });
                        ui.add_space(4.0);
                        draw_qte_damage_summary(
                            ui,
                            &qte_summaries,
                            stats.damage,
                            &mut self.hit_detail_filter,
                        );
                        ui.add_space(4.0);
                        draw_skill_damage_summary(
                            ui,
                            &skill_summaries,
                            stats.damage,
                            &mut self.hit_detail_skill_filter,
                            self.dark_mode,
                        );
                        ui.add_space(4.0);
                        ui.separator();
                        let skill_filter = self.hit_detail_skill_filter.clone();
                        self.character_hits(
                            ui,
                            char_id,
                            self.hit_detail_filter.clone(),
                            &skill_filter,
                        );
                    });
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.hit_detail_char_id = None;
            self.hit_detail_corner_applied = false;
        }
    }

    fn debug_panel(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.show_viewport_immediate(
            debug_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("NTE Debug")
                .with_inner_size(DEBUG_WINDOW_BASE_SIZE)
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                // Not transparent and not resizable on purpose — see
                // window_scale_stepper for the Windows resize-crash rationale.
                .with_resizable(false),
            |ctx, _class| {
                if !self.debug_corner_applied {
                    apply_rounding_to_process_windows();
                    self.debug_corner_applied = true;
                }
                let mut close_clicked = false;
                egui::TopBottomPanel::top("debug_title_bar")
                    .exact_height(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show(ctx, |ui| {
                        close_clicked = secondary_title_bar(
                            ui,
                            "NTE Debug",
                            &mut self.debug_window_scale,
                            DEBUG_WINDOW_BASE_SIZE,
                        );
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show(ctx, |ui| {
                        self.debug_contents(ui);
                    });
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.debug_open = false;
            self.debug_corner_applied = false;
        }
    }

    fn debug_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.debug_tab, DebugTab::Packets, "封包");
            ui.selectable_value(&mut self.debug_tab, DebugTab::Characters, "角色数据");
            ui.selectable_value(&mut self.debug_tab, DebugTab::EncryptedIni, "加密 INI");
            ui.selectable_value(&mut self.debug_tab, DebugTab::Environment, "环境");
        });
        ui.separator();
        match self.debug_tab {
            DebugTab::Packets => self.debug_packets_contents(ui),
            DebugTab::Characters => self.debug_characters_contents(ui),
            DebugTab::EncryptedIni => self.debug_encrypted_ini_contents(ui),
            DebugTab::Environment => self.debug_environment_contents(ui),
        }
    }

    fn debug_encrypted_ini_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("打开 INI").clicked() {
                self.request_debug_import(ui.ctx(), DebugImportKind::EncryptedIni);
            }
            let can_save = self.encrypted_ini_editor.path.is_some();
            if ui
                .add_enabled(can_save, egui::Button::new("保存为加密 INI"))
                .clicked()
            {
                self.save_encrypted_ini();
            }
            if ui
                .add_enabled(can_save, egui::Button::new("重新载入"))
                .clicked()
                && let Some(path) = self.encrypted_ini_editor.path.clone()
            {
                self.load_encrypted_ini(path);
            }
            if ui.button("清空").clicked() {
                self.encrypted_ini_editor = EncryptedIniEditorState::default();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_sized([92.0, 28.0], egui::Label::new("文件"));
            ui.monospace(self.encrypted_ini_editor.display_path());
        });
        ui.horizontal(|ui| {
            ui.add_sized([92.0, 28.0], egui::Label::new("保存 key"));
            egui::ComboBox::from_id_salt("encrypted_ini_key")
                .width(200.0)
                .selected_text(self.encrypted_ini_editor.key.label())
                .show_ui(ui, |ui| {
                    for key in EncryptedIniKey::all() {
                        ui.selectable_value(&mut self.encrypted_ini_editor.key, key, key.label());
                    }
                });
        });
        let editor_id = ui.make_persistent_id("encrypted_ini_plaintext_editor");
        let mut jump_to_match = false;
        ui.horizontal(|ui| {
            ui.add_sized([92.0, 28.0], egui::Label::new("搜索"));
            let search_changed = ui
                .add(
                    egui::TextEdit::singleline(&mut self.encrypted_ini_editor.search)
                        .desired_width(360.0)
                        .vertical_align(egui::Align::Center)
                        .hint_text("输入配置名或值"),
                )
                .changed();
            if search_changed {
                self.encrypted_ini_editor.search_match = None;
                self.encrypted_ini_editor.search_matches_dirty = true;
            }
            self.encrypted_ini_editor.refresh_search_matches();
            let matches = &self.encrypted_ini_editor.search_matches;
            let can_search = !matches.is_empty();
            if ui
                .add_enabled(can_search, egui::Button::new("上一个"))
                .clicked()
            {
                self.encrypted_ini_editor.search_match =
                    previous_search_match(self.encrypted_ini_editor.search_match, matches.len());
                jump_to_match = true;
            }
            if ui
                .add_enabled(can_search, egui::Button::new("下一个"))
                .clicked()
            {
                self.encrypted_ini_editor.search_match =
                    next_search_match(self.encrypted_ini_editor.search_match, matches.len());
                jump_to_match = true;
            }
            if self.encrypted_ini_editor.search.is_empty() {
                ui.label("未搜索");
            } else if let Some(current) = self.encrypted_ini_editor.search_match {
                if let Some(&byte_index) = matches.get(current) {
                    let (line, column) =
                        line_column_for_byte(&self.encrypted_ini_editor.plaintext, byte_index);
                    ui.monospace(format!(
                        "{}/{}  行 {} 列 {}",
                        current + 1,
                        matches.len(),
                        line,
                        column
                    ));
                }
            } else {
                ui.monospace(format!("{} 处匹配", matches.len()));
            }
        });
        if !self.encrypted_ini_editor.message.is_empty() {
            ui.label(
                RichText::new(&self.encrypted_ini_editor.message)
                    .color(semantic_warning(self.dark_mode)),
            );
        }
        ui.separator();
        let editor_height = (ui.available_height() - 28.0).max(180.0);
        let editor_width = ui.available_width();
        let editor = &mut self.encrypted_ini_editor;
        let matches = &editor.search_matches;
        let current_match_byte = editor
            .search_match
            .and_then(|index| matches.get(index).copied());
        let current_cursor_range = current_match_byte.and_then(|byte_index| {
            encrypted_ini_match_cursor_range(&editor.plaintext, &editor.search, byte_index)
        });
        let dark_mode = self.dark_mode;
        let search = &editor.search;
        let layout_cache = &mut editor.layout_cache;
        let plaintext = &mut editor.plaintext;
        let mut editor_changed = false;
        let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            encrypted_ini_layout_galley(
                ui,
                EncryptedIniLayoutRequest {
                    text: buffer.as_str(),
                    query: search,
                    matches,
                    current_match_byte,
                    wrap_width,
                    dark_mode,
                },
                layout_cache,
            )
        };
        egui::ScrollArea::both()
            .id_salt("encrypted_ini_editor_scroll")
            .auto_shrink([false, false])
            .max_height(editor_height)
            .show(ui, |ui| {
                let mut editor_output = egui::TextEdit::multiline(plaintext)
                    .id(editor_id)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(editor_width)
                    .lock_focus(true)
                    .layouter(&mut layouter)
                    .hint_text("打开加密 INI 后，这里会显示解密后的明文。")
                    .show(ui);
                if editor_output.response.changed() {
                    editor_changed = true;
                }
                if jump_to_match && let Some(cursor_range) = current_cursor_range {
                    editor_output
                        .state
                        .cursor
                        .set_char_range(Some(cursor_range));
                    editor_output
                        .state
                        .store(ui.ctx(), editor_output.response.id);
                    editor_output.response.request_focus();
                    let cursor_rect = editor_output
                        .galley
                        .pos_from_cursor(cursor_range.primary)
                        .translate(editor_output.galley_pos.to_vec2());
                    ui.scroll_to_rect(
                        cursor_rect.expand2(egui::vec2(80.0, 32.0)),
                        Some(egui::Align::Center),
                    );
                    ui.ctx().request_repaint();
                }
            });
        if editor_changed {
            editor.dirty = true;
            editor.search_matches_dirty = true;
            editor.layout_cache.clear();
        }
        ui.horizontal(|ui| {
            if self.encrypted_ini_editor.dirty {
                ui.label("有未保存修改");
            } else if self.encrypted_ini_editor.path.is_some() {
                ui.label("当前内容已保存或未修改");
            }
        });
    }

    fn load_encrypted_ini(&mut self, path: PathBuf) {
        match EncryptedIniEditorState::load(path) {
            Ok(editor) => {
                self.encrypted_ini_editor = editor;
                self.last_error = None;
            }
            Err(error) => {
                self.encrypted_ini_editor.message = error.clone();
                self.last_error = Some(error);
            }
        }
    }

    fn save_encrypted_ini(&mut self) {
        let Some(path) = self.encrypted_ini_editor.path.clone() else {
            self.encrypted_ini_editor.message = "请先打开一个 INI 文件".to_owned();
            return;
        };
        if self.encrypted_ini_editor.plaintext == self.encrypted_ini_editor.original_plaintext
            && self.encrypted_ini_editor.key == self.encrypted_ini_editor.original_key
        {
            self.encrypted_ini_editor.dirty = false;
            self.encrypted_ini_editor.message = "内容未修改，已保留原始密文文件".to_owned();
            return;
        }
        let encrypted = match encrypt_encrypted_ini_records(
            &self.encrypted_ini_editor.plaintext,
            self.encrypted_ini_editor.key,
            self.encrypted_ini_editor.original_key,
            &self.encrypted_ini_editor.records,
            &self.encrypted_ini_editor.line_ending,
            self.encrypted_ini_editor.final_newline,
        ) {
            Ok(encrypted) => encrypted,
            Err(error) => {
                self.encrypted_ini_editor.message = format!("生成密文失败: {error}");
                self.last_error = Some(self.encrypted_ini_editor.message.clone());
                return;
            }
        };
        if let Err(error) = atomic_write_text(&path, &encrypted) {
            self.encrypted_ini_editor.message = format!("保存 {} 失败: {error}", path.display());
            self.last_error = Some(self.encrypted_ini_editor.message.clone());
            return;
        }
        self.encrypted_ini_editor.original_key = self.encrypted_ini_editor.key;
        self.encrypted_ini_editor.original_plaintext = self.encrypted_ini_editor.plaintext.clone();
        self.encrypted_ini_editor.dirty = false;
        self.encrypted_ini_editor.message = format!(
            "已使用 {} key 保存到 {}",
            self.encrypted_ini_editor.key.label(),
            path.display()
        );
        self.status = "加密 INI 已保存".to_owned();
        self.last_error = None;
    }

    fn debug_environment_contents(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("采集设置与环境")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("debug_environment")
                    .num_columns(2)
                    .spacing([14.0, 5.0])
                    .show(ui, |ui| {
                        ui.label("网卡");
                        ui.monospace(
                            self.devices
                                .get(self.selected_device)
                                .map(|device| {
                                    if device.description.is_empty() {
                                        device.name.as_str()
                                    } else {
                                        device.description.as_str()
                                    }
                                })
                                .unwrap_or("未检测到"),
                        );
                        ui.end_row();
                        ui.label("本机 IP");
                        ui.monospace(if self.local_ip.is_empty() {
                            "未检测到"
                        } else {
                            &self.local_ip
                        });
                        ui.end_row();
                        ui.label("游戏连接");
                        if let Some(network) = &self.game_network {
                            ui.monospace(format!(
                                "PID {}  {} -> {}:{}",
                                network.pid,
                                network.local_ip,
                                network.remote_ip,
                                network.remote_port
                            ));
                        } else {
                            ui.monospace("未检测到");
                        }
                        ui.end_row();
                        ui.label("诊断");
                        ui.monospace(self.diagnostic.as_deref().unwrap_or("正常"));
                        ui.end_row();
                        ui.label("BPF");
                        ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(220.0));
                        ui.end_row();
                        ui.label("伤害来源");
                        let calibration_response = ui.checkbox(
                            &mut self.server_damage_calibration,
                            "使用服务端 HP 差值校准",
                        );
                        calibration_response.on_hover_text(
                            "重新抓包或重新导入后生效；只在服务端 HP 同步能与单条命中明确配对时覆盖伤害数值",
                        );
                        ui.end_row();
                        ui.label("实际 BPF");
                        ui.monospace(self.active_capture_filter.as_deref().unwrap_or_else(|| {
                            if self.capture.is_some() {
                                "正在确定"
                            } else {
                                "未启动"
                            }
                        }));
                        ui.end_row();
                        ui.label("原始抓包");
                        let raw_capture_label = self.raw_capture.as_ref().map_or_else(
                            || "无原始抓包".to_owned(),
                            |capture| {
                                let path = capture.path().map_or_else(
                                    || "写入不可用".to_owned(),
                                    |path| path.display().to_string(),
                                );
                                format!("{} 包 · {path}", capture.packet_count())
                            },
                        );
                        ui.monospace(raw_capture_label);
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("重新检测").clicked()
                        && let Err(error) = self.refresh_game_network()
                    {
                        self.last_error = Some(error);
                    }
                    ui.label("受击记录已启用");
                    let can_export_json = self.capture.is_none()
                        && self.replay_thread.is_none()
                        && (!self.state.hits.is_empty() || !self.state.packets.is_empty());
                    if ui
                        .add_enabled(can_export_json, egui::Button::new("导出解析 JSON"))
                        .clicked()
                    {
                        self.export_capture_info(ui.ctx());
                    }
                    let can_export_raw = self.capture.is_none()
                        && self
                            .raw_capture
                            .as_ref()
                            .is_some_and(|capture| capture.packet_count() > 0);
                    if ui
                        .add_enabled(can_export_raw, egui::Button::new("另存完整 PCAPNG"))
                        .clicked()
                    {
                        self.export_raw_capture(ui.ctx());
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("导入 pcapng").clicked() {
                        self.request_debug_import(ui.ctx(), DebugImportKind::Pcapng);
                    }
                    if ui.button("导入抓包 JSON").clicked() {
                        self.request_debug_import(ui.ctx(), DebugImportKind::CaptureJson);
                    }
                    ui.small("导入会清空当前统计，并使用与实时抓包相同的解析流程");
                });
            });
    }

    fn debug_packets_contents(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.debug_only_hits, "仅显示命中包");
            ui.label("搜索");
            ui.add(
                egui::TextEdit::singleline(&mut self.debug_search)
                    .desired_width(260.0)
                    .hint_text("IP / ID / 协议名称"),
            );
            ui.separator();
            ui.monospace(format!(
                "events={} packets={} queued={}",
                self.state.hits.len(),
                self.state.packets.len(),
                self.receiver.len()
            ));
        });
        ui.separator();
        let scroll_width = ui.available_width();
        let debug_query = self.debug_search.to_lowercase();
        egui::ScrollArea::vertical()
            .max_width(scroll_width)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.set_max_width(ui.available_width());
                for (packet_index, packet) in
                    self.state.packets.iter().rev().take(500).rev().enumerate()
                {
                    if self.debug_only_hits && packet.parsed_hits == 0 {
                        continue;
                    }
                    if !debug_query.is_empty() {
                        let searchable = format!(
                            "{} {} {} {:?} {}",
                            packet.source,
                            packet.destination,
                            packet.direction,
                            packet.declared_ids,
                            packet.decoded_text
                        )
                        .to_lowercase();
                        if !searchable.contains(&debug_query) {
                            continue;
                        }
                    }
                    let title = format!(
                        "{}  {}  {} -> {}  {} B  ids={:?}  hits={}",
                        format_time(packet.timestamp),
                        packet.direction,
                        packet.source,
                        packet.destination,
                        packet.payload_len,
                        packet.declared_ids,
                        packet.parsed_hits
                    );
                    let id = ui.make_persistent_id((
                        "debug_packet",
                        packet_index,
                        packet.timestamp.to_bits(),
                        &packet.source,
                        &packet.destination,
                    ));
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        id,
                        false,
                    )
                    .show_header(ui, |ui| {
                        ui.add(
                            egui::Label::new(title)
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                    })
                    .body(|ui| {
                        if !packet.note.is_empty() {
                            ui.label(
                                RichText::new(&packet.note).color(semantic_warning(self.dark_mode)),
                            );
                        }
                        ui.label(
                            RichText::new("自动解析")
                                .strong()
                                .color(theme_accent(self.dark_mode)),
                        );
                        ui.add(
                            egui::TextEdit::multiline(&mut packet.decoded_text.clone())
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(packet.decoded_text.lines().count().clamp(2, 14))
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
                }
            });
    }

    fn debug_characters_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("新增 ID");
            ui.add(
                egui::TextEdit::singleline(&mut self.character_editor.new_id)
                    .desired_width(100.0)
                    .hint_text("例如 1080"),
            );
            if ui.button("新增").clicked()
                && let Err(error) = self.character_editor.start_new()
            {
                self.character_editor.message = error;
            }
            if ui.button("重新载入").clicked() {
                let path = data_root().join(CHARACTER_DATA_PATH);
                match CharacterEditorState::load(&path) {
                    Ok(editor) => {
                        self.character_editor = editor;
                        self.status = "已重新载入 characters.json".to_owned();
                    }
                    Err(error) => self.character_editor.message = error,
                }
            }
            ui.separator();
            ui.label(format!(
                "共 {} 条",
                self.character_editor.character_ids().len()
            ));
        });
        if !self.character_editor.message.is_empty() {
            ui.label(
                RichText::new(&self.character_editor.message)
                    .color(semantic_warning(self.dark_mode)),
            );
        }
        ui.separator();

        let ids = self.character_editor.character_ids();
        let search = self.character_editor.search.to_lowercase();
        ui.columns(2, |columns| {
            columns[0].set_min_width(240.0);
            columns[0].set_max_width(320.0);
            columns[0].horizontal(|ui| {
                ui.label("搜索");
                ui.add(
                    egui::TextEdit::singleline(&mut self.character_editor.search)
                        .desired_width(180.0)
                        .hint_text("ID / 名称 / 属性"),
                );
            });
            columns[0].separator();
            egui::ScrollArea::vertical()
                .id_salt("character_editor_list")
                .auto_shrink([false, false])
                .show(&mut columns[0], |ui| {
                    for id in ids {
                        let row = self
                            .character_editor
                            .document
                            .get("characters")
                            .and_then(serde_json::Value::as_object)
                            .and_then(|characters| characters.get(&id))
                            .and_then(serde_json::Value::as_object);
                        let name_zh =
                            row.map_or_else(String::new, |row| json_string_field(row, "name_zh"));
                        let name_en =
                            row.map_or_else(String::new, |row| json_string_field(row, "name_en"));
                        let attribute =
                            row.map_or_else(String::new, |row| json_string_field(row, "attribute"));
                        let searchable =
                            format!("{id} {name_zh} {name_en} {attribute}").to_lowercase();
                        if !search.is_empty() && !searchable.contains(&search) {
                            continue;
                        }
                        let selected =
                            self.character_editor.selected_id.as_deref() == Some(id.as_str());
                        let attribute_label = if attribute.is_empty() {
                            String::new()
                        } else {
                            format!("  [{attribute}]")
                        };
                        let label = if name_zh.is_empty() {
                            format!("{id}  {name_en}{attribute_label}")
                        } else {
                            format!("{id}  {name_zh}  {name_en}{attribute_label}")
                        };
                        if ui.selectable_label(selected, label).clicked() {
                            if self.character_editor.dirty {
                                self.character_editor.message =
                                    "请先保存当前修改，再切换角色".to_owned();
                            } else {
                                self.character_editor.select(&id);
                            }
                        }
                    }
                });

            columns[1].heading(if self.character_editor.selected_id.is_some() {
                "编辑角色"
            } else if self.character_editor.form.id.is_empty() {
                "选择或新增角色"
            } else {
                "新增角色"
            });
            columns[1].separator();
            if self.character_editor.form.id.is_empty() {
                columns[1].label("从左侧选择一条记录，或输入新 ID 后点击“新增”。");
                return;
            }
            egui::Grid::new("character_editor_form")
                .num_columns(2)
                .spacing([12.0, 7.0])
                .show(&mut columns[1], |ui| {
                    ui.label("角色 ID");
                    ui.add_enabled(
                        self.character_editor.selected_id.is_none(),
                        egui::TextEdit::singleline(&mut self.character_editor.form.id),
                    );
                    ui.end_row();
                    character_text_field(
                        ui,
                        "中文名",
                        &mut self.character_editor.form.name_zh,
                        &mut self.character_editor.dirty,
                    );
                    character_text_field(
                        ui,
                        "英文名",
                        &mut self.character_editor.form.name_en,
                        &mut self.character_editor.dirty,
                    );
                    character_text_field(
                        ui,
                        "Codename",
                        &mut self.character_editor.form.codename,
                        &mut self.character_editor.dirty,
                    );
                    ui.label("属性");
                    let previous_attribute = self.character_editor.form.attribute.clone();
                    egui::ComboBox::from_id_salt("character_attribute")
                        .selected_text(if self.character_editor.form.attribute.is_empty() {
                            "未设置"
                        } else {
                            self.character_editor.form.attribute.as_str()
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.character_editor.form.attribute,
                                String::new(),
                                "未设置",
                            );
                            for attribute in CHARACTER_ATTRIBUTES {
                                ui.selectable_value(
                                    &mut self.character_editor.form.attribute,
                                    attribute.to_owned(),
                                    attribute,
                                );
                            }
                        });
                    if self.character_editor.form.attribute != previous_attribute {
                        self.character_editor.dirty = true;
                    }
                    ui.end_row();
                    ui.label("已验证");
                    if ui
                        .checkbox(&mut self.character_editor.form.verified, "")
                        .changed()
                    {
                        self.character_editor.dirty = true;
                    }
                    ui.end_row();
                    character_text_field(
                        ui,
                        "颜色",
                        &mut self.character_editor.form.color,
                        &mut self.character_editor.dirty,
                    );
                    character_text_field(
                        ui,
                        "头像路径",
                        &mut self.character_editor.form.avatar,
                        &mut self.character_editor.dirty,
                    );
                });
            columns[1].add_space(8.0);
            columns[1].horizontal(|ui| {
                if ui
                    .add_enabled(
                        self.character_editor.dirty,
                        egui::Button::new("保存到 characters.json"),
                    )
                    .clicked()
                {
                    self.save_character_editor(ui.ctx());
                }
                if ui
                    .add_enabled(self.character_editor.dirty, egui::Button::new("取消修改"))
                    .clicked()
                {
                    self.character_editor.cancel_edit();
                }
                if self.character_editor.dirty {
                    ui.label("有未保存修改");
                }
            });
        });
    }

    fn save_character_editor(&mut self, ctx: &egui::Context) {
        let id = match self.character_editor.apply_form() {
            Ok(id) => id,
            Err(error) => {
                self.character_editor.message = error;
                return;
            }
        };
        let path = data_root().join(CHARACTER_DATA_PATH);
        let text = match serde_json::to_string_pretty(&self.character_editor.document) {
            Ok(text) => format!("{text}\n"),
            Err(error) => {
                self.character_editor.message = format!("角色表序列化失败: {error}");
                self.character_editor.dirty = true;
                return;
            }
        };
        if let Err(error) = atomic_write_text(&path, &text) {
            self.character_editor.message = format!("保存 {} 失败: {error}", path.display());
            self.character_editor.dirty = true;
            return;
        }
        match load_characters(&path) {
            Ok(characters) => {
                self.avatar_textures = load_character_avatars(ctx, &data_root(), &characters);
                self.characters = Arc::new(characters);
                self.character_editor.message =
                    format!("ID {id} 已保存并重新加载；实时抓包中的映射将在下次启动时更新");
                self.status = "characters.json 已保存".to_owned();
                self.last_error = None;
            }
            Err(error) => {
                self.character_editor.message = format!("文件已写入，但重新加载失败: {error}");
                self.character_editor.dirty = true;
            }
        }
    }
}

impl eframe::App for DpsApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.style_dark_mode_applied != Some(self.dark_mode) {
            configure_style(ctx, self.dark_mode);
            self.style_dark_mode_applied = Some(self.dark_mode);
        }
        self.note_detail_scroll_activity(ctx);
        self.drain_events();
        self.drain_hotkeys(ctx);
        self.process_file_drops(ctx, frame);
        self.process_debug_import_dialog(ctx);
        let force_opacity = self.opacity_reapply_frames > 0;
        apply_window_attributes(
            frame,
            self.opacity,
            force_opacity,
            &mut self.applied_opacity,
            &mut self.corner_applied_hwnd,
        );
        self.opacity_reapply_frames = self.opacity_reapply_frames.saturating_sub(1);
        if self.capture.is_some() || self.replay_thread.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("custom_title_bar")
            .exact_height(32.0)
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                self.title_bar(ui);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(shadcn_background(self.dark_mode))
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ctx, |ui| {
                self.animated_controls(ui);
                if self.replay_thread.is_some() {
                    self.import_loading_content(ui);
                } else {
                    if self.state.abyss.is_active() {
                        self.abyss_selector(ui);
                    }
                    self.animated_party_content(ui);
                }
            });

        #[cfg(not(feature = "no_debug"))]
        if self.debug_open {
            self.debug_panel(ctx);
        }
        if let Some(char_id) = self.hit_detail_char_id {
            self.hit_detail_panel(ctx, char_id);
        }
        if self.team_hit_detail_open {
            self.team_hit_detail_panel(ctx);
        }
        if self.abyss_overview_open {
            self.abyss_overview_panel(ctx);
        }
        if ctx.input(|input| !input.raw.hovered_files.is_empty()) {
            egui::Area::new(egui::Id::new("pcapng_drop_overlay"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(shadcn_card(self.dark_mode))
                        .stroke(Stroke::new(2.0, theme_accent(self.dark_mode)))
                        .inner_margin(egui::Margin::symmetric(28, 20))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("松开以导入 PCAPNG")
                                    .size(18.0)
                                    .strong()
                                    .color(theme_accent(self.dark_mode)),
                            );
                        });
                });
        }
        self.paint_theme_transition(ctx);
        if let Some(error) = self.last_error.clone() {
            egui::Window::new("错误")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(error);
                    if ui.button("关闭").clicked() {
                        self.last_error = None;
                    }
                });
        }
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

enum UiConfigSavePlan {
    NoChange,
    SetPending((UiConfig, Instant)),
    KeepPending((UiConfig, Instant)),
    Save(UiConfig),
}

/// System CJK fonts tried in order. The whole UI is Chinese, so without a CJK face every label
/// renders as tofu. Microsoft YaHei is the preferred match; the rest are fallbacks that ship on
/// common Windows editions (including ones where YaHei was removed or replaced).
const CJK_FONT_CANDIDATES: &[&str] = &[
    "msyh.ttc",   // Microsoft YaHei (Win 8+)
    "msyh.ttf",   // Microsoft YaHei (older layout)
    "msyhl.ttc",  // Microsoft YaHei Light
    "simsun.ttc", // SimSun / NSimSun
    "simhei.ttf", // SimHei
    "Deng.ttf",   // DengXian
];

fn install_fonts(ctx: &egui::Context) {
    let windows_dir = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let fonts_dir = windows_dir.join("Fonts");
    let Some(bytes) = CJK_FONT_CANDIDATES
        .iter()
        .find_map(|name| std::fs::read(fonts_dir.join(name)).ok())
    else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "system-cjk".to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "system-cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Title-bar −／＋ stepper that resizes the current viewport proportionally.
///
/// Free drag-resize was removed: dragging a window edge on Windows can trigger a
/// native access violation in eframe (egui #4061 / #4091), and there is no OS
/// resize border anyway (`with_decorations(false)`). Instead the user steps the
/// window between [`WINDOW_SCALE_MIN`]..=[`WINDOW_SCALE_MAX`] of its default size.
///
/// `scale` is clamped to that range; on a click it sends exactly one
/// `InnerSize(base_size * scale)` — a discrete resize, never the per-pixel drag
/// or rapid `InnerSize` stream that the upstream bug chokes on. `base_size` keeps
/// the window's original aspect ratio. Draw inside a right-to-left layout.
fn window_scale_stepper(ui: &mut egui::Ui, scale: &mut f32, base_size: egui::Vec2) {
    let apply = |ui: &egui::Ui, scale: f32| {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                (base_size.x * scale).round(),
                (base_size.y * scale).round(),
            )));
    };
    // right-to-left: ＋ is added first (rightmost), then the readout, then －.
    if ui
        .add_enabled(
            *scale < WINDOW_SCALE_MAX - f32::EPSILON,
            egui::Button::new("＋")
                .frame(false)
                .min_size(TITLE_BAR_BUTTON_SIZE),
        )
        .on_hover_text("放大窗口")
        .clicked()
    {
        *scale = (*scale + WINDOW_SCALE_STEP).min(WINDOW_SCALE_MAX);
        apply(ui, *scale);
    }
    ui.add(
        egui::Label::new(
            RichText::new(format!("{:.0}%", *scale * 100.0))
                .size(11.0)
                .color(ui.visuals().weak_text_color()),
        )
        .selectable(false),
    )
    .on_hover_text("窗口缩放比例");
    if ui
        .add_enabled(
            *scale > WINDOW_SCALE_MIN + f32::EPSILON,
            egui::Button::new("－")
                .frame(false)
                .min_size(TITLE_BAR_BUTTON_SIZE),
        )
        .on_hover_text("缩小窗口")
        .clicked()
    {
        *scale = (*scale - WINDOW_SCALE_STEP).max(WINDOW_SCALE_MIN);
        apply(ui, *scale);
    }
}

fn secondary_title_bar(
    ui: &mut egui::Ui,
    title: &str,
    scale: &mut f32,
    base_size: egui::Vec2,
) -> bool {
    let title_height = 28.0;
    let mut close_clicked = false;
    // Whole bar drags the window; buttons drawn on top win their own clicks. See
    // the matching note in `title_bar` — keeps the bar draggable on any empty
    // area regardless of how the controls pack in.
    let (full_rect, title_drag) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), title_height),
        egui::Sense::click_and_drag(),
    );
    if title_drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    let mut bar = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(full_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    bar.label(
        RichText::new(title)
            .size(13.0)
            .strong()
            .color(bar.visuals().text_color()),
    );
    bar.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui
            .add_sized(TITLE_BAR_BUTTON_SIZE, egui::Button::new("×").frame(false))
            .on_hover_text("关闭")
            .clicked()
        {
            close_clicked = true;
        }
        if ui
            .add_sized(TITLE_BAR_BUTTON_SIZE, egui::Button::new("−").frame(false))
            .on_hover_text("最小化")
            .clicked()
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        window_scale_stepper(ui, scale, base_size);
    });
    close_clicked
}

fn debug_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_debug_viewport")
}

fn hit_detail_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_hit_detail_viewport")
}

fn team_hit_detail_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_team_hit_detail_viewport")
}

fn abyss_overview_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_abyss_overview_viewport")
}

fn draw_abyss_floor_nav(
    ui: &mut egui::Ui,
    season_nav: &[(u32, Vec<(u32, usize)>)],
    selected_season: &mut Option<u32>,
    selected_floor: &mut Option<u32>,
    selected_monster_pack_id: &mut Option<String>,
    expanded_season: &mut Option<u32>,
) {
    ui.label(
        RichText::new("层级")
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt("abyss_all_season_floor_nav")
        .auto_shrink([false, false])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for (season, floors) in season_nav {
                let expanded = *expanded_season == Some(*season);
                let selected_in_season = *selected_season == Some(*season);
                let season_label = format!(
                    "{} 第 {season} 期 ·  {} 层",
                    if expanded { "▼" } else { "▶" },
                    floors.len()
                );
                if ui
                    .add_sized(
                        egui::vec2(ui.available_width(), 28.0),
                        egui::Button::selectable(selected_in_season || expanded, season_label),
                    )
                    .clicked()
                {
                    *expanded_season = if expanded { None } else { Some(*season) };
                }
                ui.add_space(3.0);
                if expanded {
                    ui.indent(("abyss_season_floors", season), |ui| {
                        for (floor, monster_count) in floors {
                            let selected = *selected_season == Some(*season)
                                && *selected_floor == Some(*floor);
                            let label = format!("{floor:>2} 层  ·  {monster_count} 怪");
                            if ui
                                .add_sized(
                                    egui::vec2(ui.available_width(), 24.0),
                                    egui::Button::selectable(selected, label),
                                )
                                .clicked()
                            {
                                *selected_season = Some(*season);
                                *selected_floor = Some(*floor);
                                *selected_monster_pack_id = None;
                                *expanded_season = Some(*season);
                            }
                        }
                    });
                    ui.add_space(5.0);
                }
                ui.add_space(4.0);
            }
        });
}

/// What a line section's prediction control was clicked to do this frame.
#[derive(PartialEq, Eq)]
enum LinePredictionAction {
    None,
    ImportCurrent,
    ImportFile,
    Clear,
}

/// Inputs for the per-line clear-time prediction shown in a line section header.
struct LinePredictionView<'a> {
    team: Option<&'a TeamDps>,
    line_hp: f64,
    can_import: bool,
    avatar_textures: &'a HashMap<String, egui::TextureHandle>,
    characters: &'a HashMap<u32, CharacterInfo>,
}

fn abyss_line_hp_total(monsters: &[&AbyssMonsterEntry]) -> f64 {
    monsters
        .iter()
        .map(|monster| monster.stats.hp_max_base)
        .sum()
}

fn predicted_clear_seconds(line_hp: f64, team: &TeamDps) -> Option<f64> {
    (team.dps > 0.0 && line_hp > 0.0).then(|| line_hp / team.dps)
}

fn format_clear_seconds(seconds: f64) -> String {
    if seconds >= 60.0 {
        let minutes = (seconds / 60.0).floor();
        let rest = seconds - minutes * 60.0;
        format!("{minutes:.0}分{rest:04.1}秒")
    } else {
        format!("{seconds:.1}秒")
    }
}

fn draw_team_avatar(
    ui: &mut egui::Ui,
    char_id: u32,
    size: f32,
    avatar_textures: &HashMap<String, egui::TextureHandle>,
    characters: &HashMap<u32, CharacterInfo>,
    dark_mode: bool,
) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let radius = size * 0.3;
    let character = characters.get(&char_id);
    let display_name = character.map(|info| {
        if info.name_zh.is_empty() {
            info.name_en.clone()
        } else {
            info.name_zh.clone()
        }
    });
    let texture = character
        .and_then(|info| info.avatar.as_deref())
        .and_then(|avatar| avatar_textures.get(avatar));
    if let Some(texture) = texture {
        egui::Image::new((texture.id(), rect.size()))
            .corner_radius(radius)
            .paint_at(ui, rect);
    } else {
        let color = readable_accent(character_color(char_id, characters, 0), dark_mode);
        ui.painter()
            .rect_filled(rect, radius, color.gamma_multiply(0.85));
        if let Some(initial) = display_name.as_deref().and_then(|name| name.chars().next()) {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initial,
                egui::FontId::proportional(size * 0.5),
                contrast_text(color),
            );
        }
    }
    if let Some(name) = display_name {
        response.on_hover_text(name);
    }
}

fn draw_line_prediction_header(
    ui: &mut egui::Ui,
    view: &LinePredictionView,
    dark_mode: bool,
) -> LinePredictionAction {
    let mut action = LinePredictionAction::None;
    ui.add_space(10.0);
    if let Some(team) = view.team {
        for member in team.members.iter().take(TEAM_DPS_MAX_MEMBERS) {
            draw_team_avatar(
                ui,
                member.id,
                22.0,
                view.avatar_textures,
                view.characters,
                dark_mode,
            );
        }
        ui.add_space(4.0);
        let time_text = match predicted_clear_seconds(view.line_hp, team) {
            Some(seconds) => format!("预计 {}", format_clear_seconds(seconds)),
            None => "预计 —".to_owned(),
        };
        ui.label(
            RichText::new(time_text)
                .size(12.0)
                .strong()
                .color(theme_accent(dark_mode)),
        );
        ui.label(
            RichText::new(format!("· {} DPS", format_number(team.dps)))
                .size(10.0)
                .color(ui.visuals().weak_text_color()),
        );
        if ui
            .add(egui::Button::new("清除").small())
            .on_hover_text("清除该行预测队伍")
            .clicked()
        {
            action = LinePredictionAction::Clear;
        }
    } else {
        if view.can_import
            && ui
                .add(egui::Button::new("用当前队伍预测").small())
                .on_hover_text("把当前会话测得的队伍设为该行预测队伍")
                .clicked()
        {
            action = LinePredictionAction::ImportCurrent;
        }
        if !view.can_import {
            ui.label(
                RichText::new("导入数据预测通关时间")
                    .size(11.0)
                    .color(ui.visuals().weak_text_color()),
            );
        }
    }
    // Per-line file import is always available (the "单独导入" button): load a
    // DPS data file into just this line.
    if ui
        .add(egui::Button::new("单独导入").small())
        .on_hover_text("为该行单独导入 DPS 数据文件")
        .clicked()
    {
        action = LinePredictionAction::ImportFile;
    }
    action
}

// UI draw helper: each argument is a distinct, unrelated input (selection state,
// textures, theme, prediction), so grouping them into a struct would not aid
// readability.
#[allow(clippy::too_many_arguments)]
fn draw_abyss_line_section(
    ui: &mut egui::Ui,
    title: &str,
    monsters: &[&AbyssMonsterEntry],
    selected_pack_id: Option<&str>,
    selected_target: &mut Option<String>,
    monster_textures: &HashMap<String, egui::TextureHandle>,
    dark_mode: bool,
    prediction: Option<LinePredictionView>,
) -> LinePredictionAction {
    const SLOT_COUNT: usize = 6;
    const GAP: f32 = 6.0;
    let mut action = LinePredictionAction::None;
    egui::Frame::new()
        .fill(shadcn_card(dark_mode))
        .stroke(Stroke::new(1.0, shadcn_border(dark_mode)))
        .corner_radius(7)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(title)
                        .size(13.0)
                        .strong()
                        .color(shadcn_foreground(dark_mode)),
                );
                ui.label(
                    RichText::new(format!("{} 个敌人", monsters.len()))
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                );
                if let Some(view) = prediction.as_ref() {
                    action = draw_line_prediction_header(ui, view, dark_mode);
                }
            });
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                let slot_width = ((ui.available_width() - GAP * (SLOT_COUNT as f32 - 1.0))
                    / SLOT_COUNT as f32)
                    .max(44.0);
                for index in 0..SLOT_COUNT {
                    if index == SLOT_COUNT - 1 && monsters.len() > SLOT_COUNT {
                        draw_abyss_more_chip(ui, monsters.len() - index, slot_width, dark_mode);
                    } else if let Some(monster) = monsters.get(index) {
                        let selected = selected_pack_id == Some(monster.pack_id.as_str());
                        if draw_abyss_monster_chip(
                            ui,
                            monster,
                            selected,
                            slot_width,
                            monster_texture(monster_textures, &monster.monster_id),
                            dark_mode,
                        )
                        .clicked()
                        {
                            *selected_target = Some(monster.pack_id.clone());
                        }
                    } else {
                        draw_abyss_empty_chip(ui, slot_width, dark_mode);
                    }
                }
            });
        });
    action
}

fn draw_abyss_monster_chip(
    ui: &mut egui::Ui,
    monster: &AbyssMonsterEntry,
    selected: bool,
    width: f32,
    texture: Option<&egui::TextureHandle>,
    dark_mode: bool,
) -> egui::Response {
    let size = egui::vec2(width, 34.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    draw_abyss_chip_frame(ui, rect, selected, dark_mode);

    let painter = ui.painter();
    let portrait_rect = egui::Rect::from_center_size(
        rect.left_center() + egui::vec2(17.0, 0.0),
        egui::vec2(24.0, 24.0),
    );
    draw_monster_portrait(ui, portrait_rect, monster, texture, 6.0, 11.0, dark_mode);
    let text_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(36.0, 4.0),
        rect.right_bottom() - egui::vec2(6.0, 4.0),
    );
    painter.with_clip_rect(text_rect).text(
        text_rect.left_top(),
        egui::Align2::LEFT_TOP,
        &monster.name,
        egui::FontId::proportional(11.0),
        shadcn_foreground(dark_mode),
    );
    painter.text(
        text_rect.left_bottom(),
        egui::Align2::LEFT_BOTTOM,
        format!(
            "{}  HP {}",
            monster_wave_label(monster),
            format_stat_value(monster.stats.hp_max_base)
        ),
        egui::FontId::monospace(9.0),
        ui.visuals().weak_text_color(),
    );

    response.on_hover_text(format!(
        "{}\n{}\n{}",
        monster.name,
        monster.pack_id,
        monster_line_label(monster)
    ))
}

fn draw_abyss_empty_chip(ui: &mut egui::Ui, width: f32, dark_mode: bool) {
    let size = egui::vec2(width, 34.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    draw_abyss_chip_frame(ui, rect, false, dark_mode);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "-",
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color().gamma_multiply(0.45),
    );
}

fn draw_abyss_more_chip(ui: &mut egui::Ui, count: usize, width: f32, dark_mode: bool) {
    let size = egui::vec2(width, 34.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    draw_abyss_chip_frame(ui, rect, false, dark_mode);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("+{count}"),
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    response.on_hover_text(format!("还有 {count} 个敌人未显示"));
}

fn draw_monster_portrait(
    ui: &egui::Ui,
    rect: egui::Rect,
    monster: &AbyssMonsterEntry,
    texture: Option<&egui::TextureHandle>,
    corner_radius: f32,
    fallback_text_size: f32,
    dark_mode: bool,
) {
    let painter = ui.painter();
    if let Some(texture) = texture {
        painter.rect_filled(rect, corner_radius, shadcn_background(dark_mode));
        let image_rect = contain_rect(rect.shrink(1.0), texture.size_vec2());
        // Paint the portrait as a rounded textured rect so the (usually square)
        // source art is clipped to the corner radius instead of poking out past
        // the rounded border — `painter.image` only draws a sharp-cornered quad.
        let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
        painter.add(
            egui::epaint::RectShape::filled(image_rect, corner_radius, Color32::WHITE)
                .with_texture(texture.id(), uv),
        );
        painter.rect_stroke(
            rect,
            corner_radius,
            Stroke::new(1.0, shadcn_border(dark_mode)),
            egui::StrokeKind::Inside,
        );
        return;
    }

    let icon_color = monster_color(&monster.monster_id, dark_mode);
    painter.rect_filled(rect, corner_radius, icon_color);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        monster_icon_text(monster),
        egui::FontId::proportional(fallback_text_size),
        contrast_text(icon_color),
    );
}

fn contain_rect(bounds: egui::Rect, image_size: egui::Vec2) -> egui::Rect {
    if image_size.x <= 0.0 || image_size.y <= 0.0 {
        return bounds;
    }
    let scale = (bounds.width() / image_size.x).min(bounds.height() / image_size.y);
    egui::Rect::from_center_size(bounds.center(), image_size * scale)
}

fn draw_abyss_chip_frame(ui: &mut egui::Ui, rect: egui::Rect, selected: bool, dark_mode: bool) {
    let fill = if selected {
        shadcn_muted(dark_mode)
    } else {
        shadcn_background(dark_mode)
    };
    ui.painter().rect_filled(rect, 7.0, fill);
    ui.painter().rect_stroke(
        rect,
        7.0,
        Stroke::new(
            if selected { 1.5 } else { 1.0 },
            if selected {
                theme_accent(dark_mode)
            } else {
                shadcn_border(dark_mode)
            },
        ),
        egui::StrokeKind::Inside,
    );
}

fn draw_abyss_monster_detail(
    ui: &mut egui::Ui,
    monster: &AbyssMonsterEntry,
    texture: Option<&egui::TextureHandle>,
    dark_mode: bool,
    height: f32,
    stat_display_names: &HashMap<String, String>,
) {
    let inner_height = (height - 24.0).max(180.0);
    egui::Frame::new()
        .fill(shadcn_card(dark_mode))
        .stroke(Stroke::new(1.0, shadcn_border(dark_mode)))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height(inner_height);
            ui.horizontal(|ui| {
                let icon_size = egui::vec2(56.0, 56.0);
                let (icon_rect, _) = ui.allocate_exact_size(icon_size, egui::Sense::hover());
                draw_monster_portrait(ui, icon_rect, monster, texture, 10.0, 20.0, dark_mode);
                ui.vertical(|ui| {
                    ui.add_sized(
                        egui::vec2(ui.available_width(), 24.0),
                        egui::Label::new(
                            RichText::new(&monster.name)
                                .size(18.0)
                                .strong()
                                .color(shadcn_foreground(dark_mode)),
                        )
                        .truncate(),
                    );
                    ui.add_sized(
                        egui::vec2(ui.available_width(), 18.0),
                        egui::Label::new(
                            RichText::new(format!(
                                "{} · {}",
                                monster_line_label(monster),
                                monster.pack_id
                            ))
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                        )
                        .truncate(),
                    );
                });
            });
            ui.add_space(10.0);
            ui.label(
                RichText::new("全部数值字段")
                    .strong()
                    .color(shadcn_foreground(dark_mode)),
            );
            let grid_height = ui.available_height().max(120.0);
            egui::ScrollArea::vertical()
                .id_salt(("abyss_raw_props", monster.pack_id.as_str()))
                .auto_shrink([false, false])
                .max_height(grid_height)
                .show(ui, |ui| {
                    const SCROLLBAR_GUTTER: f32 = 24.0;
                    let mut viewport_clip = ui.clip_rect();
                    viewport_clip.max.x =
                        (viewport_clip.max.x - SCROLLBAR_GUTTER).max(viewport_clip.min.x);
                    let row_width = (ui.available_width() - SCROLLBAR_GUTTER).max(0.0);
                    let row_height = 21.0;
                    let pair_gap = 22.0;
                    // Pack more columns into wider windows so values don't drift far
                    // from their labels. ~250pt per pair reads comfortably; clamp to
                    // 2–4 so it stays sensible at any size the user drags to.
                    let columns = ((row_width / 250.0).floor() as usize).clamp(2, 4);
                    let pair_width =
                        ((row_width - pair_gap * (columns as f32 - 1.0)) / columns as f32).max(0.0);
                    let value_width = 96.0_f32.min(pair_width * 0.34).max(42.0);
                    let label_width = (pair_width - value_width - 8.0).max(40.0);
                    for (index, chunk) in monster.stats.raw_props.chunks(columns).enumerate() {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(row_width, row_height),
                            egui::Sense::hover(),
                        );
                        if index % 2 == 1 {
                            ui.painter().with_clip_rect(viewport_clip).rect_filled(
                                rect,
                                3.0,
                                shadcn_muted(dark_mode),
                            );
                        }
                        let mut row_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect.shrink2(egui::vec2(2.0, 1.0)))
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        row_ui.set_clip_rect(rect.intersect(viewport_clip));
                        for pair_index in 0..columns {
                            if pair_index > 0 {
                                row_ui.add_space(pair_gap);
                            }
                            if let Some((key, value)) = chunk.get(pair_index) {
                                abyss_stat_pair_sized(
                                    &mut row_ui,
                                    key,
                                    &format_stat_value(*value),
                                    dark_mode,
                                    label_width,
                                    value_width,
                                    stat_display_names,
                                );
                            } else {
                                row_ui.add_space(label_width + value_width);
                            }
                        }
                    }
                    ui.add_space(14.0);
                });
        });
}

fn abyss_stat_pair_sized(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    dark_mode: bool,
    label_width: f32,
    value_width: f32,
    stat_display_names: &HashMap<String, String>,
) {
    let display_label = stat_display_names
        .get(label)
        .or_else(|| stat_display_names.get(&label.to_ascii_lowercase()))
        .map(String::as_str)
        .unwrap_or(label);
    let label_response = ui.add_sized(
        egui::vec2(label_width, 18.0),
        egui::Label::new(
            RichText::new(display_label)
                .size(11.0)
                .color(ui.visuals().weak_text_color()),
        )
        .truncate(),
    );
    if display_label != label {
        label_response.on_hover_text(label);
    }
    ui.add_sized(
        egui::vec2(value_width, 18.0),
        egui::Label::new(
            RichText::new(value)
                .size(11.0)
                .color(shadcn_foreground(dark_mode)),
        )
        .truncate()
        .halign(egui::Align::RIGHT),
    );
}

fn monster_texture<'a>(
    textures: &'a HashMap<String, egui::TextureHandle>,
    monster_id: &str,
) -> Option<&'a egui::TextureHandle> {
    monster_image_keys(monster_id)
        .into_iter()
        .find_map(|key| textures.get(&key))
}

fn monster_image_keys(value: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let base = canonical_monster_image_key(value);
    push_unique_key(&mut keys, base.clone());
    push_trimmed_monster_keys(&mut keys, &base);
    keys
}

fn monster_image_resource_keys(value: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let raw = value
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(value)
        .to_ascii_lowercase();
    push_unique_key(&mut keys, raw);
    push_unique_key(&mut keys, canonical_monster_image_key(value));
    keys
}

fn monster_image_stem_candidates(monster_id: &str) -> Vec<String> {
    let mut stems = Vec::new();
    for key in raw_case_monster_image_stems(monster_id) {
        push_unique_key(&mut stems, key);
    }
    for key in raw_monster_image_keys(monster_id) {
        push_unique_key(&mut stems, key.clone());
        push_unique_key(&mut stems, titlecase_boss_key(&key));
    }
    for key in monster_image_keys(monster_id) {
        push_unique_key(&mut stems, key.clone());
        push_unique_key(&mut stems, titlecase_boss_key(&key));
    }
    stems
}

fn raw_case_monster_image_stems(value: &str) -> Vec<String> {
    let mut stems = Vec::new();
    let base = value
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(value)
        .to_owned();
    push_unique_key(&mut stems, base.clone());
    push_trimmed_monster_stems(&mut stems, &base);
    stems
}

fn raw_monster_image_keys(value: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let base = value
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(value)
        .to_ascii_lowercase();
    push_unique_key(&mut keys, base.clone());
    push_trimmed_monster_keys(&mut keys, &base);
    keys
}

fn push_trimmed_monster_stems(stems: &mut Vec<String>, stem: &str) {
    let suffixes = ["_Abyss", "_abyss", "_BP", "_bp", "_BF", "_bf", "_B", "_b"];
    let mut current = stem.to_owned();
    while let Some(next) = suffixes
        .iter()
        .find_map(|suffix| current.strip_suffix(suffix).map(str::to_owned))
    {
        push_unique_key(stems, next.clone());
        current = next;
    }

    for marker in ["_summon", "_Summon", "_double_", "_Double_"] {
        if let Some((base, _)) = current.split_once(marker) {
            push_unique_key(stems, base.to_owned());
        }
    }
}

fn titlecase_boss_key(key: &str) -> String {
    key.strip_prefix("boss_")
        .map(|suffix| format!("Boss_{suffix}"))
        .unwrap_or_else(|| key.to_owned())
}

fn push_trimmed_monster_keys(keys: &mut Vec<String>, key: &str) {
    let suffixes = ["_abyss", "_bp", "_bf", "_b"];
    let mut current = key.to_owned();
    while let Some(next) = suffixes
        .iter()
        .find_map(|suffix| current.strip_suffix(suffix).map(str::to_owned))
    {
        push_unique_key(keys, next.clone());
        current = next;
    }

    if let Some(without_blue) = current.strip_suffix("_blue") {
        push_unique_key(keys, without_blue.to_owned());
    }
    if let Some(without_red) = current.strip_suffix("_red") {
        push_unique_key(keys, without_red.to_owned());
    }
    if let Some((base, _)) = current.split_once("_summon") {
        push_unique_key(keys, base.to_owned());
    }
    if let Some((base, _)) = current.split_once("_double_") {
        push_unique_key(keys, base.to_owned());
    }
    if let Some((base, suffix)) = current.rsplit_once('_')
        && suffix.chars().all(|character| character.is_ascii_digit())
        && base.contains('_')
    {
        push_unique_key(keys, base.to_owned());
    }
}

fn push_unique_key(keys: &mut Vec<String>, key: String) {
    if !keys.iter().any(|existing| existing == &key) {
        keys.push(key);
    }
}

fn canonical_monster_image_key(value: &str) -> String {
    let without_extension = value
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(value)
        .to_ascii_lowercase();
    without_extension
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u32>()
                .map(|number| number.to_string())
                .unwrap_or_else(|_| part.to_owned())
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn monster_icon_text(monster: &AbyssMonsterEntry) -> String {
    monster
        .name
        .chars()
        .find(|character| !character.is_whitespace())
        .unwrap_or('?')
        .to_string()
}

fn monster_color(monster_id: &str, dark_mode: bool) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(66, 153, 225),
        Color32::from_rgb(236, 115, 87),
        Color32::from_rgb(89, 184, 143),
        Color32::from_rgb(183, 125, 220),
        Color32::from_rgb(222, 173, 84),
        Color32::from_rgb(75, 174, 187),
        Color32::from_rgb(214, 98, 136),
        Color32::from_rgb(125, 148, 226),
    ];
    let hash = monster_id.bytes().fold(0usize, |accumulator, byte| {
        accumulator.wrapping_mul(31).wrapping_add(byte as usize)
    });
    readable_accent(PALETTE[hash % PALETTE.len()], dark_mode)
}

fn monster_line_label(monster: &AbyssMonsterEntry) -> String {
    let half = monster.half.map(|value| match value {
        0 => "上行线".to_owned(),
        1 => "下行线".to_owned(),
        other => format!("线路 {other}"),
    });
    let wave = monster.wave.map(|value| format!("第 {value} 波"));
    match (half, wave) {
        (Some(half), Some(wave)) => format!("{half} · {wave}"),
        (Some(half), None) => half,
        (None, Some(wave)) => wave,
        (None, None) => "整层配置".to_owned(),
    }
}

fn monster_wave_label(monster: &AbyssMonsterEntry) -> String {
    monster
        .wave
        .map(|wave| format!("W{wave}"))
        .unwrap_or_else(|| "-".to_owned())
}

fn format_stat_value(value: f64) -> String {
    if value.abs() >= 1000.0 || value.fract().abs() < f64::EPSILON {
        format_number(value)
    } else {
        format!("{value:.2}")
    }
}

include!(concat!(env!("OUT_DIR"), "/embedded_resources.rs"));

fn load_attribute_icons(
    ctx: &egui::Context,
    root: &std::path::Path,
) -> HashMap<String, egui::TextureHandle> {
    ATTRIBUTE_ICON_PATHS
        .into_iter()
        .filter_map(|(attribute, path)| {
            load_image_texture(ctx, root, path, "attribute-icon")
                .map(|texture| (attribute.to_owned(), texture))
        })
        .collect()
}

fn load_monster_textures(
    ctx: &egui::Context,
    root: &std::path::Path,
    abyss_overview: &AbyssOverviewState,
) -> HashMap<String, egui::TextureHandle> {
    let mut textures = HashMap::new();
    let Some(dataset) = &abyss_overview.dataset else {
        return textures;
    };

    for monster_id in dataset
        .seasons
        .iter()
        .flat_map(|season| season.floors.iter())
        .flat_map(|floor| floor.monsters.iter())
        .map(|monster| monster.monster_id.as_str())
    {
        for stem in monster_image_stem_candidates(monster_id) {
            let resource_keys = monster_image_resource_keys(&stem);
            if resource_keys.iter().any(|key| textures.contains_key(key)) {
                break;
            }
            let resource_path = format!("{MONSTER_IMAGE_DIR}/{stem}.png");
            let Some(texture) = load_image_texture(ctx, root, &resource_path, "monster") else {
                continue;
            };
            for key in resource_keys {
                textures.entry(key).or_insert_with(|| texture.clone());
            }
            break;
        }
    }

    textures
}

fn load_damage_digit_textures(
    ctx: &egui::Context,
    root: &std::path::Path,
) -> HashMap<String, Vec<egui::TextureHandle>> {
    let mut textures = HashMap::new();
    for (key, prefix) in DAMAGE_DIGIT_TEXTURE_SETS {
        let digits = (0..=9)
            .filter_map(|digit| {
                let path = damage_digit_resource_path(prefix, digit);
                load_image_texture(ctx, root, &path, "damage-digit")
            })
            .collect::<Vec<_>>();
        if digits.len() == 10 {
            textures.insert(key.to_owned(), digits);
        }
    }
    textures
}

fn load_reaction_text_textures(
    ctx: &egui::Context,
    root: &std::path::Path,
) -> HashMap<u8, Vec<egui::TextureHandle>> {
    let mut textures = HashMap::new();
    for reaction in 1..=REACTION_TEXT_IMAGE_COUNT {
        let glyphs = (1..=2)
            .filter_map(|part| {
                let path = format!("{DAMAGE_DIGIT_IMAGE_DIR}/fanying{reaction:02}_{part:02}.png");
                load_image_texture(ctx, root, &path, "reaction-text")
            })
            .collect::<Vec<_>>();
        if glyphs.len() == 2 {
            textures.insert(reaction, glyphs);
        }
    }
    textures
}

fn damage_digit_resource_path(prefix: &str, digit: usize) -> String {
    format!("{DAMAGE_DIGIT_IMAGE_DIR}/{prefix}_{digit}.png")
}

fn load_character_avatars(
    ctx: &egui::Context,
    root: &std::path::Path,
    characters: &HashMap<u32, CharacterInfo>,
) -> HashMap<String, egui::TextureHandle> {
    let mut textures = HashMap::new();
    for avatar in characters
        .values()
        .filter_map(|character| character.avatar.as_deref())
    {
        if textures.contains_key(avatar) {
            continue;
        }
        if let Some(texture) = load_image_texture(ctx, root, avatar, "character-avatar") {
            textures.insert(avatar.to_owned(), texture);
        }
    }
    textures
}

fn load_image_texture(
    ctx: &egui::Context,
    root: &std::path::Path,
    resource_path: &str,
    texture_namespace: &str,
) -> Option<egui::TextureHandle> {
    let path = root.join(resource_path);
    let disk_bytes = std::fs::read(&path).ok();
    let bytes = disk_bytes
        .as_deref()
        .or_else(|| embedded_image_resource(resource_path))?;
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    // Source art (e.g. 256px avatars) is drawn much smaller (~32px), an 8x
    // minification. Plain bilinear sampling has no mip chain, so it only reads a
    // 2x2 texel neighborhood and aliases hard edges into the jagged look seen on
    // screen. Trilinear filtering with generated mipmaps samples a pre-averaged
    // level, keeping shrunken images crisp and smooth. The glow backend (enabled
    // here) honors `mipmap_mode`.
    let texture_options = egui::TextureOptions {
        magnification: egui::TextureFilter::Linear,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    };
    Some(ctx.load_texture(
        format!("{texture_namespace}:{resource_path}"),
        color_image,
        texture_options,
    ))
}

fn pixel_aligned_rect(origin: egui::Pos2, logical_size: f32, pixels_per_point: f32) -> egui::Rect {
    let pixels_per_point = pixels_per_point.max(1.0);
    let physical_size = (logical_size * pixels_per_point).round();
    let size = physical_size / pixels_per_point;
    let min = egui::pos2(
        (origin.x * pixels_per_point).round() / pixels_per_point,
        (origin.y * pixels_per_point).round() / pixels_per_point,
    );
    egui::Rect::from_min_size(min, egui::vec2(size, size))
}

fn configure_style(ctx: &egui::Context, dark_mode: bool) {
    let mut visuals = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    if dark_mode {
        visuals.panel_fill = Color32::from_rgb(9, 9, 11);
        visuals.window_fill = Color32::from_rgb(9, 9, 11);
        visuals.extreme_bg_color = Color32::from_rgb(9, 9, 11);
        visuals.faint_bg_color = Color32::from_rgb(24, 24, 27);
        visuals.code_bg_color = Color32::from_rgb(24, 24, 27);
    } else {
        visuals.panel_fill = Color32::from_rgb(255, 255, 255);
        visuals.window_fill = Color32::from_rgb(255, 255, 255);
        visuals.extreme_bg_color = Color32::from_rgb(250, 250, 250);
        visuals.faint_bg_color = Color32::from_rgb(244, 244, 245);
        visuals.code_bg_color = Color32::from_rgb(244, 244, 245);
    }
    let border = shadcn_border(dark_mode);
    let card = shadcn_card(dark_mode);
    let hover = shadcn_card_hover(dark_mode);
    visuals.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    visuals.widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(
        1.0,
        if dark_mode {
            Color32::from_rgb(250, 250, 250)
        } else {
            Color32::from_rgb(9, 9, 11)
        },
    );
    visuals.widgets.inactive.bg_fill = card;
    visuals.widgets.inactive.weak_bg_fill = card;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.inactive.fg_stroke = visuals.widgets.noninteractive.fg_stroke;
    visuals.widgets.hovered.bg_fill = hover;
    visuals.widgets.hovered.weak_bg_fill = hover;
    visuals.widgets.hovered.fg_stroke = visuals.widgets.noninteractive.fg_stroke;
    visuals.widgets.hovered.bg_stroke = Stroke::new(
        1.0,
        if dark_mode {
            Color32::from_rgb(63, 63, 70)
        } else {
            Color32::from_rgb(212, 212, 216)
        },
    );
    visuals.widgets.active.bg_fill = if dark_mode {
        Color32::from_rgb(82, 82, 91)
    } else {
        Color32::from_rgb(212, 212, 216)
    };
    visuals.widgets.active.weak_bg_fill = visuals.widgets.active.bg_fill;
    visuals.widgets.active.fg_stroke = Stroke::new(
        1.0,
        if dark_mode {
            Color32::from_rgb(250, 250, 250)
        } else {
            Color32::from_rgb(24, 24, 27)
        },
    );
    visuals.window_stroke = Stroke::new(1.0, border);
    let accent = theme_accent(dark_mode);
    visuals.selection.bg_fill = accent;
    visuals.selection.stroke = Stroke::new(1.0, contrast_text(accent));
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.animation_time = 0.14;
    style.interaction.selectable_labels = false;
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    style.spacing.button_padding = egui::vec2(11.0, 4.0);
    let mut scroll = egui::style::ScrollStyle::solid();
    scroll.bar_width = 8.0;
    scroll.handle_min_length = 32.0;
    scroll.bar_inner_margin = 4.0;
    scroll.bar_outer_margin = 2.0;
    scroll.foreground_color = true;
    style.spacing.scroll = scroll;
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.expansion = 0.0;
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.active.expansion = 0.0;
    style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
    ctx.set_style(style);
}

#[derive(Clone)]
struct SkillDamageSummary {
    name: String,
    category: String,
    hits: u64,
    damage: f64,
}

fn is_qte_follow_up_damage_type(attack_type: &str) -> bool {
    matches!(
        attack_type,
        "创生花" | "覆纹" | "延滞" | "黯星" | "浊燃" | "浸染" | "盈蓄" | "失谐"
    )
}

fn is_qte_follow_up_damage_hit(hit: &crate::model::Hit) -> bool {
    hit.follow_up_attack_type
        .as_deref()
        .is_some_and(is_qte_follow_up_damage_type)
        || (!hit.char_known
            && hit
                .attack_type
                .as_deref()
                .is_some_and(is_qte_follow_up_damage_type))
}

fn is_party_member_row(row: &CharacterStats, hits: &VecDeque<crate::model::Hit>) -> bool {
    hits.iter()
        .any(|hit| hit.char_id == row.char_id && !is_qte_follow_up_damage_hit(hit))
}

fn hit_specific_type(hit: &crate::model::Hit) -> &str {
    hit.damage_name
        .as_deref()
        .or(hit.attack_type.as_deref())
        .unwrap_or("未知招式")
}

fn hit_type_label(hit: &crate::model::Hit) -> &str {
    match hit.direction.as_str() {
        "incoming" => "受击",
        "unknown" => "候选输出",
        _ => hit_specific_type(hit),
    }
}

fn reaction_text_key_for_hit(hit: &crate::model::Hit) -> Option<u8> {
    hit.attack_type
        .as_deref()
        .and_then(reaction_text_key_from_trigger_attack_type)
}

fn reaction_text_key_from_trigger_attack_type(attack_type: &str) -> Option<u8> {
    let reaction = attack_type.strip_prefix("环合·")?;
    match reaction {
        "创生" | "创生花" => Some(1),
        "覆纹" => Some(2),
        "黯星" => Some(3),
        "浊燃" | "灼燃" => Some(4),
        "浸染" => Some(5),
        "延滞" => Some(6),
        "盈蓄" => Some(7),
        "失谐" => Some(8),
        _ => None,
    }
}

fn hit_detail_hover_text(hit: &crate::model::Hit, include_character: bool) -> String {
    let mut lines = Vec::new();
    if include_character {
        lines.push(format!("{} · {}", hit.char_name, hit_type_label(hit)));
    } else {
        lines.push(hit_type_label(hit).to_owned());
    }
    if hit.follow_up_damage > 0.0 {
        lines.push(format!(
            "伤害：{} + {}",
            format_number(hit.damage),
            format_number(hit.follow_up_damage)
        ));
    } else {
        lines.push(format!("伤害：{}", format_number(hit.damage)));
    }
    if hit.target_max_hp > 0.0 {
        lines.push(format!(
            "目标 HP：{} / {}  {:.1}%",
            format_number(hit.target_hp_after),
            format_number(hit.target_max_hp),
            hit.target_hp_percent
        ));
    }
    if hit.direction == "unknown" {
        lines.push("方向尚未确认".to_owned());
    } else if let Some(ability_name) = hit.ability_name.as_deref() {
        lines.push(format!("GA：{ability_name}"));
    }
    lines.join("\n")
}

fn aggregate_character_skill_damage(
    hits: &std::collections::VecDeque<crate::model::Hit>,
    char_id: u32,
) -> Vec<SkillDamageSummary> {
    let mut summaries = HashMap::<String, SkillDamageSummary>::new();
    for hit in hits
        .iter()
        .filter(|hit| hit.char_id == char_id && hit.direction != "incoming")
    {
        let name = hit_specific_type(hit).to_owned();
        let row = summaries
            .entry(name.clone())
            .or_insert_with(|| SkillDamageSummary {
                name,
                category: hit.attack_type.clone().unwrap_or_else(|| "未知".to_owned()),
                hits: 0,
                damage: 0.0,
            });
        row.hits += 1;
        row.damage += hit.total_damage();
    }
    let mut rows: Vec<_> = summaries.into_values().collect();
    rows.sort_by(|left, right| {
        right
            .damage
            .total_cmp(&left.damage)
            .then_with(|| left.name.cmp(&right.name))
    });
    rows
}

fn summarize_qte_type_filters(
    hits: &VecDeque<crate::model::Hit>,
    char_id: Option<u32>,
) -> Vec<QteTypeFilterSummary> {
    let mut summaries = HashMap::<String, QteTypeFilterSummary>::new();
    for hit in hits.iter().filter(|hit| {
        hit.direction != "incoming" && char_id.is_none_or(|char_id| hit.char_id == char_id)
    }) {
        if let Some(attack_type) = hit.attack_type.as_deref()
            && is_qte_follow_up_damage_type(attack_type)
        {
            let row =
                summaries
                    .entry(attack_type.to_owned())
                    .or_insert_with(|| QteTypeFilterSummary {
                        attack_type: attack_type.to_owned(),
                        hits: 0,
                        damage: 0.0,
                    });
            row.hits += 1;
            row.damage += hit.damage;
        }
        if hit.follow_up_damage > 0.0
            && let Some(attack_type) = hit.follow_up_attack_type.as_deref()
            && is_qte_follow_up_damage_type(attack_type)
        {
            let row =
                summaries
                    .entry(attack_type.to_owned())
                    .or_insert_with(|| QteTypeFilterSummary {
                        attack_type: attack_type.to_owned(),
                        hits: 0,
                        damage: 0.0,
                    });
            row.hits += 1;
            row.damage += hit.follow_up_damage;
        }
    }
    let mut rows = summaries.into_values().collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .damage
            .total_cmp(&left.damage)
            .then_with(|| left.attack_type.cmp(&right.attack_type))
    });
    rows
}

fn hit_detail_filter_available(
    filter: &HitDetailFilter,
    qte_summaries: &[QteTypeFilterSummary],
) -> bool {
    match filter {
        HitDetailFilter::QteType(attack_type) => qte_summaries
            .iter()
            .any(|summary| summary.attack_type == *attack_type),
        _ => true,
    }
}

#[cfg(test)]
fn qte_type_filter_label(summary: &QteTypeFilterSummary, total_damage: f64) -> String {
    let share = if total_damage > 0.0 {
        summary.damage / total_damage * 100.0
    } else {
        0.0
    };
    format!(
        "{} {} · {share:.1}%",
        summary.attack_type,
        format_number(summary.damage)
    )
}

fn draw_qte_damage_summary(
    ui: &mut egui::Ui,
    qte_summaries: &[QteTypeFilterSummary],
    total_damage: f64,
    selected: &mut HitDetailFilter,
) {
    if qte_summaries.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.add_sized(
            egui::vec2(92.0, 36.0),
            egui::Label::new(
                RichText::new("环合伤害")
                    .strong()
                    .color(ui.visuals().weak_text_color()),
            ),
        );
        for summary in qte_summaries {
            qte_damage_summary_chip(ui, summary, total_damage, selected);
        }
    });
}

fn qte_damage_summary_chip(
    ui: &mut egui::Ui,
    summary: &QteTypeFilterSummary,
    total_damage: f64,
    selected: &mut HitDetailFilter,
) {
    let target_filter = HitDetailFilter::QteType(summary.attack_type.clone());
    let is_selected = selected == &target_filter;
    let share = if total_damage > 0.0 {
        summary.damage / total_damage * 100.0
    } else {
        0.0
    };
    let width = 156.0_f32.max(96.0 + summary.attack_type.chars().count() as f32 * 12.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 42.0), egui::Sense::click());
    let dark_mode = ui.visuals().dark_mode;
    let accent = theme_accent(dark_mode);
    let bg = if is_selected {
        accent
    } else if response.hovered() {
        shadcn_card_hover(dark_mode)
    } else {
        shadcn_card(dark_mode)
    };
    let text_color = if is_selected {
        contrast_text(accent)
    } else {
        shadcn_foreground(dark_mode)
    };
    ui.painter().rect(
        rect,
        egui::CornerRadius::same(6),
        bg,
        Stroke::new(
            1.0,
            if is_selected {
                accent
            } else {
                shadcn_border(dark_mode)
            },
        ),
        egui::StrokeKind::Inside,
    );
    let progress_rect = egui::Rect::from_min_max(
        rect.left_bottom() - egui::vec2(0.0, 3.0),
        egui::pos2(
            rect.left() + rect.width() * (share as f32 / 100.0).clamp(0.0, 1.0),
            rect.bottom(),
        ),
    );
    ui.painter().rect_filled(
        progress_rect,
        1.0,
        if is_selected {
            contrast_text(accent).gamma_multiply(0.45)
        } else {
            accent.gamma_multiply(0.55)
        },
    );
    let text_rect = rect.shrink2(egui::vec2(10.0, 5.0));
    ui.painter().text(
        egui::pos2(text_rect.left(), text_rect.top() + 9.0),
        egui::Align2::LEFT_CENTER,
        &summary.attack_type,
        egui::FontId::proportional(12.5),
        text_color,
    );
    ui.painter().text(
        egui::pos2(text_rect.left(), text_rect.top() + 27.0),
        egui::Align2::LEFT_CENTER,
        format!("{} · {share:.1}%", format_number(summary.damage)),
        egui::FontId::monospace(11.0),
        if is_selected {
            contrast_text(accent).gamma_multiply(0.82)
        } else {
            ui.visuals().weak_text_color()
        },
    );
    if response
        .on_hover_text(format!(
            "{} 次 · 总伤害 {} · 占总伤害 {share:.1}%",
            summary.hits,
            format_number(summary.damage)
        ))
        .clicked()
    {
        *selected = if is_selected {
            HitDetailFilter::All
        } else {
            target_filter
        };
    }
}

fn detail_hits_for_source(
    state: &CombatState,
    source: HitDetailSource,
) -> &VecDeque<crate::model::Hit> {
    match source {
        HitDetailSource::Global => &state.hits,
        HitDetailSource::AbyssFirst => &state.abyss.first_half.hits,
        HitDetailSource::AbyssSecond => &state.abyss.second_half.hits,
    }
}

fn build_hit_detail_cache(
    hits: &VecDeque<crate::model::Hit>,
    generation: u64,
    key: HitDetailCacheKey,
) -> HitDetailCache {
    let mut filtered_count = 0;
    let mut rows = Vec::with_capacity(key.limit.min(hits.len()));
    for (index, hit) in hits.iter().enumerate().rev().filter(|(_, hit)| {
        key.char_id.is_none_or(|char_id| hit.char_id == char_id)
            && key.filter.matches(hit)
            && (key.skill_filter.is_empty() || hit_specific_type(hit) == key.skill_filter.as_str())
    }) {
        filtered_count += 1;
        if rows.len() < key.limit {
            rows.push(cached_hit_row(index, hit));
        }
    }

    if key.char_id.is_some() {
        rows.sort_by(compare_cached_character_hits);
    } else {
        rows.sort_by(compare_cached_team_hits);
    }

    let max_damage = rows.iter().map(|row| row.damage).fold(1.0_f64, f64::max);

    HitDetailCache {
        key: Some(key),
        generation,
        source_len: hits.len(),
        rows,
        filtered_count,
        max_damage,
        dirty_since: None,
        last_scroll_offset: None,
    }
}

fn cached_hit_row(index: usize, hit: &crate::model::Hit) -> CachedHitRow {
    let is_incoming = hit.direction == "incoming";
    CachedHitRow {
        index,
        is_incoming,
        damage: hit.total_damage(),
        char_id: hit.char_id,
        hp_fraction: (hit.target_hp_percent / 100.0).clamp(0.0, 1.0) as f32,
        timestamp: hit.timestamp,
        byte_offset: hit.byte_offset,
        bit_shift: hit.bit_shift,
        target_hp_after: hit.target_hp_after,
        target_max_hp: hit.target_max_hp,
    }
}

fn resolve_cached_hit<'a>(
    hits: &'a VecDeque<crate::model::Hit>,
    row: &CachedHitRow,
    source_len: usize,
    appended: u64,
) -> Option<&'a crate::model::Hit> {
    let appended = usize::try_from(appended).unwrap_or(usize::MAX);
    adjusted_cached_index(row.index, source_len, hits.len(), appended)
        .and_then(|index| hits.get(index))
        .filter(|hit| cached_hit_matches(row, hit))
        .or_else(|| {
            hits.get(row.index)
                .filter(|hit| cached_hit_matches(row, hit))
        })
}

fn cached_hit_matches(row: &CachedHitRow, hit: &crate::model::Hit) -> bool {
    row.char_id == hit.char_id
        && (row.timestamp - hit.timestamp).abs() <= 0.001
        && row.byte_offset == hit.byte_offset
        && row.bit_shift == hit.bit_shift
        && (row.target_max_hp - hit.target_max_hp).abs() <= 0.5
}

fn adjusted_cached_index(
    index: usize,
    source_len: usize,
    current_len: usize,
    appended: usize,
) -> Option<usize> {
    let popped = source_len
        .saturating_add(appended)
        .saturating_sub(current_len);
    index.checked_sub(popped)
}

fn compare_cached_character_hits(left: &CachedHitRow, right: &CachedHitRow) -> std::cmp::Ordering {
    (left.timestamp.floor() as i64)
        .cmp(&(right.timestamp.floor() as i64))
        .then_with(|| u8::from(left.is_incoming).cmp(&u8::from(right.is_incoming)))
        .then_with(|| cached_health_pool_key(left).cmp(&cached_health_pool_key(right)))
        .then_with(|| right.target_hp_after.total_cmp(&left.target_hp_after))
        .then_with(|| left.timestamp.total_cmp(&right.timestamp))
        .then_with(|| left.byte_offset.cmp(&right.byte_offset))
        .then_with(|| left.bit_shift.cmp(&right.bit_shift))
        .then_with(|| left.damage.total_cmp(&right.damage))
}

fn compare_cached_team_hits(left: &CachedHitRow, right: &CachedHitRow) -> std::cmp::Ordering {
    (left.timestamp.floor() as i64)
        .cmp(&(right.timestamp.floor() as i64))
        .then_with(|| {
            u8::from(left.target_hp_after <= 0.0 || left.hp_fraction <= 0.0).cmp(&u8::from(
                right.target_hp_after <= 0.0 || right.hp_fraction <= 0.0,
            ))
        })
        .then_with(|| cached_health_pool_key(left).cmp(&cached_health_pool_key(right)))
        .then_with(|| right.target_hp_after.total_cmp(&left.target_hp_after))
        .then_with(|| left.timestamp.total_cmp(&right.timestamp))
        .then_with(|| left.byte_offset.cmp(&right.byte_offset))
        .then_with(|| left.bit_shift.cmp(&right.bit_shift))
        .then_with(|| left.char_id.cmp(&right.char_id))
        .then_with(|| right.is_incoming.cmp(&left.is_incoming))
        .then_with(|| left.damage.total_cmp(&right.damage))
}

fn cached_health_pool_key(row: &CachedHitRow) -> i64 {
    if row.target_max_hp.is_finite() && row.target_max_hp > 0.0 {
        row.target_max_hp.round() as i64
    } else {
        i64::MIN
    }
}

fn draw_skill_damage_summary(
    ui: &mut egui::Ui,
    summaries: &[SkillDamageSummary],
    total_damage: f64,
    selected_skill: &mut String,
    dark_mode: bool,
) {
    egui::CollapsingHeader::new(
        RichText::new("招式输出构成")
            .strong()
            .color(shadcn_foreground(dark_mode)),
    )
    .default_open(true)
    .show(ui, |ui| {
        let header_width = ui.available_width() - ui.style().spacing.scroll.allocated_width();
        let (header_rect, _) =
            ui.allocate_exact_size(egui::vec2(header_width, 24.0), egui::Sense::hover());
        let header_font = egui::FontId::proportional(12.0);
        let header_color = ui.visuals().weak_text_color();
        ui.painter().text(
            header_rect.left_center() + egui::vec2(10.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "具体招式",
            header_font.clone(),
            header_color,
        );
        ui.painter().text(
            header_rect.right_center() - egui::vec2(10.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            "伤害占比 / 总伤害 / 次数",
            header_font,
            header_color,
        );
        egui::ScrollArea::vertical()
            .id_salt("skill_damage_summary")
            .max_height(190.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                for (rank, summary) in summaries.iter().enumerate() {
                    let share = if total_damage > 0.0 {
                        summary.damage / total_damage * 100.0
                    } else {
                        0.0
                    };
                    let selected = selected_skill == &summary.name;
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 34.0),
                        egui::Sense::click(),
                    );
                    let corner_radius = egui::CornerRadius::same(6);
                    let base_color = if selected {
                        shadcn_muted(dark_mode)
                    } else if response.hovered() {
                        shadcn_card_hover(dark_mode)
                    } else {
                        shadcn_card(dark_mode)
                    };
                    ui.painter().rect_filled(rect, corner_radius, base_color);
                    let progress_rect = egui::Rect::from_min_max(
                        rect.min,
                        egui::pos2(
                            rect.left() + rect.width() * (share as f32 / 100.0).clamp(0.0, 1.0),
                            rect.bottom(),
                        ),
                    );
                    ui.painter().rect_filled(
                        progress_rect,
                        corner_radius,
                        theme_accent(dark_mode).gamma_multiply(if selected { 0.28 } else { 0.16 }),
                    );
                    if selected {
                        ui.painter().rect_stroke(
                            rect,
                            corner_radius,
                            Stroke::new(1.0, theme_accent(dark_mode)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    let foreground = shadcn_foreground(dark_mode);
                    let metrics_width = 230.0_f32.min(rect.width() * 0.48);
                    let left_clip = egui::Rect::from_min_max(
                        rect.min,
                        egui::pos2(rect.right() - metrics_width - 8.0, rect.bottom()),
                    );
                    ui.painter().with_clip_rect(left_clip).text(
                        rect.left_center() + egui::vec2(10.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        format!("{}. {}  [{}]", rank + 1, summary.name, summary.category),
                        egui::FontId::proportional(12.0),
                        foreground,
                    );
                    let metrics_clip = egui::Rect::from_min_max(
                        egui::pos2(rect.right() - metrics_width, rect.top()),
                        rect.max,
                    );
                    ui.painter().with_clip_rect(metrics_clip).text(
                        rect.right_center() - egui::vec2(10.0, 0.0),
                        egui::Align2::RIGHT_CENTER,
                        format!(
                            "{share:.1}%  ·  {}  ·  {}次",
                            format_number(summary.damage),
                            summary.hits
                        ),
                        egui::FontId::monospace(11.5),
                        foreground,
                    );
                    if response.clicked() {
                        if selected {
                            selected_skill.clear();
                        } else {
                            selected_skill.clone_from(&summary.name);
                        }
                    }
                }
            });
    });
}

#[derive(Clone, Copy)]
struct CharacterHitLayout {
    row_width: f32,
    time_x: f32,
    type_x: f32,
    type_width: f32,
    damage_x: f32,
    hp_x: f32,
    separators: [f32; 3],
}

#[derive(Clone, Copy)]
struct TeamHitLayout {
    row_width: f32,
    time_x: f32,
    character_x: f32,
    type_x: f32,
    type_width: f32,
    damage_x: f32,
    hp_x: f32,
    separators: [f32; 4],
}

struct TeamHitRowAssets<'a> {
    avatar_texture: Option<&'a egui::TextureHandle>,
    damage_digits: Option<&'a [egui::TextureHandle]>,
    follow_up_damage_digits: Option<&'a [egui::TextureHandle]>,
    reaction_textures: &'a HashMap<u8, Vec<egui::TextureHandle>>,
}

impl TeamHitLayout {
    fn new(available_width: f32) -> Self {
        const LEFT_INSET: f32 = 4.0;
        const TIME_WIDTH: f32 = 92.0;
        const CHARACTER_WIDTH: f32 = 132.0;
        const TYPE_WIDTH: f32 = 210.0;
        const DAMAGE_WIDTH: f32 = 120.0;
        const CELL_PADDING: f32 = 10.0;

        let time_x = LEFT_INSET + CELL_PADDING;
        let character_separator = LEFT_INSET + TIME_WIDTH;
        let character_x = character_separator + CELL_PADDING;
        let type_separator = character_separator + CHARACTER_WIDTH;
        let type_x = type_separator + CELL_PADDING;
        let damage_separator = type_separator + TYPE_WIDTH;
        let damage_x = damage_separator + CELL_PADDING;
        let hp_separator = damage_separator + DAMAGE_WIDTH;
        let hp_x = hp_separator + CELL_PADDING;

        Self {
            row_width: available_width,
            time_x,
            character_x,
            type_x,
            type_width: TYPE_WIDTH,
            damage_x,
            hp_x,
            separators: [
                character_separator,
                type_separator,
                damage_separator,
                hp_separator,
            ],
        }
    }
}

impl CharacterHitLayout {
    fn new(available_width: f32) -> Self {
        const LEFT_INSET: f32 = 4.0;
        const TIME_WIDTH: f32 = 92.0;
        const TYPE_WIDTH: f32 = 250.0;
        const DAMAGE_WIDTH: f32 = 130.0;
        const CELL_PADDING: f32 = 10.0;

        let time_x = LEFT_INSET + CELL_PADDING;
        let type_separator = LEFT_INSET + TIME_WIDTH;
        let type_x = type_separator + CELL_PADDING;
        let damage_separator = type_separator + TYPE_WIDTH;
        let damage_x = damage_separator + CELL_PADDING;
        let hp_separator = damage_separator + DAMAGE_WIDTH;
        let hp_x = hp_separator + CELL_PADDING;

        Self {
            row_width: available_width,
            time_x,
            type_x,
            type_width: TYPE_WIDTH,
            damage_x,
            hp_x,
            separators: [type_separator, damage_separator, hp_separator],
        }
    }
}

fn draw_character_hit_header(ui: &mut egui::Ui, layout: CharacterHitLayout) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(layout.row_width, 24.0), egui::Sense::hover());
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter().clone();
    let font = egui::FontId::proportional(12.0);
    let color = ui.visuals().weak_text_color();
    draw_hit_column_separators(&painter, rect, layout);

    painter.text(
        egui::pos2(x + layout.time_x, y),
        egui::Align2::LEFT_CENTER,
        "时间",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.type_x, y),
        egui::Align2::LEFT_CENTER,
        "类型",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.damage_x, y),
        egui::Align2::LEFT_CENTER,
        "伤害",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.hp_x, y),
        egui::Align2::LEFT_CENTER,
        "目标 / HP",
        font,
        color,
    );
}

fn damage_digit_textures_for_hit<'a>(
    hit: &crate::model::Hit,
    characters: &HashMap<u32, CharacterInfo>,
    damage_digit_textures: &'a HashMap<String, Vec<egui::TextureHandle>>,
) -> Option<&'a [egui::TextureHandle]> {
    damage_digit_key_for_hit(hit, characters)
        .and_then(|key| damage_digit_textures.get(key))
        .map(Vec::as_slice)
}

fn follow_up_damage_digit_textures_for_hit<'a>(
    hit: &crate::model::Hit,
    damage_digit_textures: &'a HashMap<String, Vec<egui::TextureHandle>>,
) -> Option<&'a [egui::TextureHandle]> {
    follow_up_damage_digit_key_for_hit(hit)
        .and_then(|key| damage_digit_textures.get(key))
        .map(Vec::as_slice)
}

fn damage_digit_key_for_hit<'a>(
    hit: &'a crate::model::Hit,
    characters: &'a HashMap<u32, CharacterInfo>,
) -> Option<&'a str> {
    if hit.direction == "incoming" {
        return Some("物理");
    }
    let source_attribute = hit.damage_attribute.as_deref().or_else(|| {
        characters
            .get(&hit.char_id)
            .and_then(|character| character.attribute.as_deref())
    });
    let attack_type = hit.attack_type.as_deref();

    if attack_type.is_some_and(|attack_type| attack_type == "倾陷伤害")
        || hit
            .damage_name
            .as_deref()
            .is_some_and(|damage_name| damage_name.contains("倾陷"))
    {
        return Some("真实");
    }

    attack_type
        .and_then(|attack_type| mixed_damage_digit_key(attack_type, source_attribute))
        .or(source_attribute)
}

fn follow_up_damage_digit_key_for_hit(hit: &crate::model::Hit) -> Option<&str> {
    let source_attribute = hit.follow_up_damage_attribute.as_deref()?;
    hit.follow_up_attack_type
        .as_deref()
        .and_then(|attack_type| mixed_damage_digit_key(attack_type, Some(source_attribute)))
        .or(Some(source_attribute))
}

fn mixed_damage_digit_key(
    attack_type: &str,
    source_attribute: Option<&str>,
) -> Option<&'static str> {
    // 触发环合的那一下伤害（attack_type 形如 "环合·创生"）是造成伤害角色自己打出的
    // 攻击，跳字应使用该角色的属性字系，而不是环合反应字系。这里返回 None，让调用方
    // 回退到 source_attribute（造成伤害角色的属性）。只有环合之后产生的反应伤害本体
    // （attack_type 为不带 "环合·" 前缀的反应名）才使用下面固定的反应字系。
    if attack_type.starts_with("环合·") {
        return None;
    }
    match attack_type {
        // 环合反应伤害本体的跳字固定为触发侧属性的字系，与该伤害最终记给环合双方
        // 哪一名角色无关。每个反应都只用属性对里的一侧：
        //   创生 (光灵) -> 恒为 Guangling_G（光），不再出现 Guangling_L
        //   覆纹 (灵咒) -> 恒为 lingzhou_L（灵），不再出现 lingzhou_Z
        //   黯星 (暗魂) -> 恒为 Anhun_A（暗），不再出现 Anhun_H
        //   浊燃 (咒暗) -> 恒为 Zhouan_A（暗），不再出现 Zhouan_Z
        "创生" | "创生花" => Some("Guangling_G"),
        "覆纹" => Some("lingzhou_L"),
        "黯星" => Some("Anhun_A"),
        "浊燃" => Some("Zhouan_A"),
        "延滞" => match source_attribute? {
            "光" => Some("Guangxiang_G"),
            "相" => Some("Guangxiang_X"),
            _ => None,
        },
        "浸染" | "魂相" => match source_attribute? {
            "魂" => Some("Hunxiang_H"),
            "相" => Some("Hunxiang_X"),
            _ => None,
        },
        // 盈蓄 / 失谐 only keep the reaction series whose digit PNGs still exist
        // (the trigger-side ones). The removed `_L`/`_H`/`_Z` sides fall through
        // to `None`, so the caller uses the credited character's plain element
        // digits instead of a missing texture.
        "盈蓄" => match source_attribute? {
            "光" => Some("Guangling_G"),
            "相" => Some("Guangxiang_X"),
            _ => None,
        },
        "失谐" => match source_attribute? {
            "暗" => Some("Anhun_A"),
            _ => None,
        },
        _ => None,
    }
}

fn draw_damage_number(
    ui: &egui::Ui,
    rect: egui::Rect,
    value: f64,
    damage_digits: Option<&[egui::TextureHandle]>,
    fallback_color: Color32,
) -> egui::Rect {
    let text = damage_number_digits_text(value);
    let Some(damage_digits) = damage_digits.filter(|digits| digits.len() == 10) else {
        return draw_damage_number_fallback(ui, rect, &text, fallback_color);
    };
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return draw_damage_number_fallback(ui, rect, &text, fallback_color);
    }

    let base_height = (rect.height() - 10.0).clamp(12.0, 22.0);
    let Some(base_width) = damage_number_image_width(&text, damage_digits, base_height) else {
        return draw_damage_number_fallback(ui, rect, &text, fallback_color);
    };
    if base_width <= 0.0 {
        return draw_damage_number_fallback(ui, rect, &text, fallback_color);
    }

    let height = if base_width > rect.width() {
        (base_height * rect.width() / base_width).max(10.0)
    } else {
        base_height
    };
    let Some(total_width) = damage_number_image_width(&text, damage_digits, height) else {
        return draw_damage_number_fallback(ui, rect, &text, fallback_color);
    };

    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let mut cursor = rect.left();
    let top = rect.center().y - height * 0.5;
    let drawn_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), top),
        egui::vec2(total_width, height),
    );
    for digit in text.bytes().map(|byte| (byte - b'0') as usize) {
        let texture = &damage_digits[digit];
        let size = texture.size_vec2();
        if size.y <= 0.0 {
            return draw_damage_number_fallback(ui, rect, &text, fallback_color);
        }
        let width = size.x / size.y * height;
        let digit_rect =
            egui::Rect::from_min_size(egui::pos2(cursor, top), egui::vec2(width, height));
        painter.image(
            texture.id(),
            digit_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        cursor += width;
        if cursor - rect.left() >= total_width {
            break;
        }
    }
    drawn_rect
}

fn damage_number_image_width(
    text: &str,
    damage_digits: &[egui::TextureHandle],
    height: f32,
) -> Option<f32> {
    let mut width = 0.0;
    for digit in text.bytes().map(|byte| (byte - b'0') as usize) {
        let size = damage_digits.get(digit)?.size_vec2();
        if size.y <= 0.0 {
            return None;
        }
        width += size.x / size.y * height;
    }
    Some(width)
}

fn damage_number_digits_text(value: f64) -> String {
    let rounded = value.round() as i64;
    if rounded < 0 {
        format!("-{}", rounded.unsigned_abs())
    } else {
        rounded.to_string()
    }
}

fn draw_damage_number_fallback(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    color: Color32,
) -> egui::Rect {
    ui.painter().with_clip_rect(rect).text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::monospace(15.0),
        color,
    );
    let width = ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), egui::FontId::monospace(15.0), color)
            .size()
            .x
    });
    egui::Rect::from_center_size(
        egui::pos2(rect.left() + width * 0.5, rect.center().y),
        egui::vec2(width, 18.0),
    )
}

fn draw_follow_up_damage_badge(
    ui: &egui::Ui,
    damage_cell_rect: egui::Rect,
    base_damage_rect: egui::Rect,
    hit: &crate::model::Hit,
    damage_digits: Option<&[egui::TextureHandle]>,
    fallback_color: Color32,
) {
    if hit.follow_up_damage <= 0.0 {
        return;
    }
    let badge_height = 15.0_f32.min((damage_cell_rect.height() - 8.0).max(12.0));
    let text = damage_number_digits_text(hit.follow_up_damage);
    let width = damage_digits
        .filter(|digits| digits.len() == 10)
        .and_then(|digits| damage_number_image_width(&text, digits, badge_height))
        .unwrap_or_else(|| (text.chars().count() as f32 * 8.0).max(16.0));
    let left = (base_damage_rect.right() - width * 0.18)
        .max(damage_cell_rect.left())
        .min((damage_cell_rect.right() - width).max(damage_cell_rect.left()));
    let top = (base_damage_rect.top() - badge_height * 0.35).max(damage_cell_rect.top() + 1.0);
    let badge_rect =
        egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, badge_height));
    draw_damage_number(
        ui,
        badge_rect,
        hit.follow_up_damage,
        damage_digits,
        fallback_color,
    );
}

fn draw_character_hit_row(
    ui: &mut egui::Ui,
    layout: CharacterHitLayout,
    hit: &crate::model::Hit,
    max_damage: f64,
    damage_digits: Option<&[egui::TextureHandle]>,
    follow_up_damage_digits: Option<&[egui::TextureHandle]>,
    reaction_textures: &HashMap<u8, Vec<egui::TextureHandle>>,
) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(layout.row_width, DETAIL_HIT_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    let incoming = hit.direction == "incoming";
    let type_color = match hit.direction.as_str() {
        "incoming" => semantic_danger(ui.visuals().dark_mode),
        "unknown" => semantic_warning(ui.visuals().dark_mode),
        _ => hit_output_badge_color(ui.visuals().dark_mode),
    };
    ui.painter().rect_filled(
        rect,
        5.0,
        if ui.visuals().dark_mode {
            Color32::from_rgba_unmultiplied(255, 255, 255, 8)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 5)
        },
    );
    let damage_fraction = (hit.total_damage() / max_damage).clamp(0.0, 1.0) as f32;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * damage_fraction, rect.height()),
        ),
        5.0,
        type_color.gamma_multiply(if ui.visuals().dark_mode { 0.12 } else { 0.08 }),
    );
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter().clone();
    let text_color = ui.visuals().text_color();
    let damage_color = if incoming {
        semantic_danger(ui.visuals().dark_mode)
    } else {
        hit_output_text_color(ui.visuals().dark_mode)
    };
    let mono = egui::FontId::monospace(13.0);
    draw_hit_column_separators(&painter, rect, layout);
    painter.text(
        egui::pos2(x + layout.time_x, y),
        egui::Align2::LEFT_CENTER,
        format_short_time(hit.timestamp),
        mono.clone(),
        text_color,
    );
    painter.rect_filled(
        egui::Rect::from_center_size(
            egui::pos2(x + layout.type_x + (layout.type_width - 20.0) * 0.5, y),
            egui::vec2(layout.type_width - 20.0, 24.0),
        ),
        10.0,
        type_color,
    );
    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(x + layout.type_x + (layout.type_width - 20.0) * 0.5, y),
        egui::vec2(layout.type_width - 20.0, 24.0),
    );
    draw_hit_type_badge_content(ui, badge_rect, hit, type_color, reaction_textures);
    let damage_cell_rect = egui::Rect::from_min_max(
        egui::pos2(x + layout.damage_x, rect.top()),
        egui::pos2(x + layout.hp_x - 8.0, rect.bottom()),
    );
    let base_damage_rect = draw_damage_number(
        ui,
        damage_cell_rect,
        hit.damage,
        damage_digits,
        damage_color,
    );
    draw_follow_up_damage_badge(
        ui,
        damage_cell_rect,
        base_damage_rect,
        hit,
        follow_up_damage_digits,
        damage_color,
    );
    let hp_fraction = (hit.target_hp_percent / 100.0).clamp(0.0, 1.0) as f32;
    let hp_cell_left = x + layout.hp_x - 6.0;
    let hp_cell_right = (rect.right() - 4.0).min(ui.clip_rect().right() - 4.0);
    let hp_cell_rect = egui::Rect::from_min_max(
        egui::pos2(hp_cell_left, rect.top() + 2.0),
        egui::pos2(hp_cell_right.max(hp_cell_left), rect.bottom() - 2.0),
    );
    painter.rect_filled(hp_cell_rect, 4.0, ui.visuals().faint_bg_color);
    painter.rect_filled(
        egui::Rect::from_min_size(
            hp_cell_rect.min,
            egui::vec2(hp_cell_rect.width() * hp_fraction, hp_cell_rect.height()),
        ),
        4.0,
        if hp_fraction > 0.5 {
            semantic_success(ui.visuals().dark_mode).gamma_multiply(0.16)
        } else if hp_fraction > 0.2 {
            semantic_warning(ui.visuals().dark_mode).gamma_multiply(0.16)
        } else {
            semantic_danger(ui.visuals().dark_mode).gamma_multiply(0.16)
        },
    );
    draw_target_hp_text(ui, hp_cell_rect, hit, text_color, mono.clone());
    response.on_hover_text(hit_detail_hover_text(hit, false));
}

fn draw_team_hit_header(ui: &mut egui::Ui, layout: TeamHitLayout) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(layout.row_width, 24.0), egui::Sense::hover());
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter();
    let font = egui::FontId::proportional(12.0);
    let color = ui.visuals().weak_text_color();
    draw_team_hit_column_separators(painter, rect, layout);

    for (offset, label) in [
        (layout.time_x, "时间"),
        (layout.character_x, "角色"),
        (layout.type_x, "类型"),
        (layout.damage_x, "伤害"),
        (layout.hp_x, "目标 / HP"),
    ] {
        painter.text(
            egui::pos2(x + offset, y),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            color,
        );
    }
}

fn draw_hit_type_badge_content(
    ui: &mut egui::Ui,
    badge_rect: egui::Rect,
    hit: &crate::model::Hit,
    type_color: Color32,
    reaction_textures: &HashMap<u8, Vec<egui::TextureHandle>>,
) {
    if hit.direction == "outgoing"
        && let Some(textures) =
            reaction_text_key_for_hit(hit).and_then(|key| reaction_textures.get(&key))
        && textures.len() == 2
    {
        draw_reaction_text_images(ui, badge_rect.shrink2(egui::vec2(8.0, 3.0)), textures);
        return;
    }
    draw_clipped_label(
        ui,
        badge_rect.shrink2(egui::vec2(8.0, 0.0)),
        hit_type_label(hit),
        egui::FontId::proportional(12.0),
        contrast_text(type_color),
        egui::Align::Center,
        None,
    );
}

fn draw_reaction_text_images(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    textures: &[egui::TextureHandle],
) {
    let gap = 2.0;
    let mut height = rect.height().clamp(1.0, 19.0);
    let mut widths = textures
        .iter()
        .map(|texture| {
            let size = texture.size_vec2();
            if size.y > 0.0 {
                size.x / size.y * height
            } else {
                height
            }
        })
        .collect::<Vec<_>>();
    let total_width = widths.iter().sum::<f32>() + gap * (widths.len().saturating_sub(1) as f32);
    if total_width > rect.width() && total_width > 0.0 {
        let scale = rect.width() / total_width;
        height *= scale;
        for width in &mut widths {
            *width *= scale;
        }
    }
    let total_width = widths.iter().sum::<f32>() + gap * (widths.len().saturating_sub(1) as f32);
    let mut left = rect.center().x - total_width * 0.5;
    let top = rect.center().y - height * 0.5;
    let painter = ui.painter().with_clip_rect(rect);
    for (texture, width) in textures.iter().zip(widths) {
        let image_rect =
            egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, height));
        painter.image(
            texture.id(),
            image_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        left += width + gap;
    }
}

fn draw_team_hit_row(
    ui: &mut egui::Ui,
    layout: TeamHitLayout,
    hit: &crate::model::Hit,
    max_damage: f64,
    character_color: Color32,
    assets: TeamHitRowAssets<'_>,
) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(layout.row_width, DETAIL_HIT_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    let incoming = hit.direction == "incoming";
    let type_color = match hit.direction.as_str() {
        "incoming" => semantic_danger(ui.visuals().dark_mode),
        "unknown" => semantic_warning(ui.visuals().dark_mode),
        _ => hit_output_badge_color(ui.visuals().dark_mode),
    };
    ui.painter().rect_filled(
        rect,
        5.0,
        if ui.visuals().dark_mode {
            Color32::from_rgba_unmultiplied(255, 255, 255, 8)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 5)
        },
    );
    let damage_fraction = (hit.total_damage() / max_damage).clamp(0.0, 1.0) as f32;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * damage_fraction, rect.height()),
        ),
        5.0,
        type_color.gamma_multiply(if ui.visuals().dark_mode { 0.12 } else { 0.08 }),
    );
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter().clone();
    let text_color = ui.visuals().text_color();
    let mono = egui::FontId::monospace(13.0);
    draw_team_hit_column_separators(&painter, rect, layout);

    painter.text(
        egui::pos2(x + layout.time_x, y),
        egui::Align2::LEFT_CENTER,
        format_short_time(hit.timestamp),
        mono.clone(),
        text_color,
    );
    let avatar_rect = pixel_aligned_rect(
        egui::pos2(x + layout.character_x, y - 16.0),
        32.0,
        ui.ctx().pixels_per_point(),
    );
    painter.rect_filled(
        avatar_rect,
        7.0,
        if ui.visuals().dark_mode {
            Color32::from_rgb(55, 58, 66)
        } else {
            Color32::from_rgb(225, 227, 232)
        },
    );
    if let Some(texture) = assets.avatar_texture {
        ui.put(
            avatar_rect,
            egui::Image::new((texture.id(), avatar_rect.size())).corner_radius(7),
        );
    } else {
        painter.rect_filled(avatar_rect, 7.0, character_color.gamma_multiply(0.82));
        painter.text(
            avatar_rect.center(),
            egui::Align2::CENTER_CENTER,
            hit.char_name.chars().next().unwrap_or('?').to_string(),
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
    }
    painter.rect_stroke(
        avatar_rect,
        7.0,
        Stroke::new(1.5, character_color),
        egui::StrokeKind::Inside,
    );
    painter.text(
        egui::pos2(avatar_rect.right() + 7.0, y),
        egui::Align2::LEFT_CENTER,
        &hit.char_name,
        egui::FontId::proportional(12.0),
        text_color,
    );
    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(x + layout.type_x + (layout.type_width - 20.0) * 0.5, y),
        egui::vec2(layout.type_width - 20.0, 24.0),
    );
    painter.rect_filled(badge_rect, 10.0, type_color);
    draw_hit_type_badge_content(ui, badge_rect, hit, type_color, assets.reaction_textures);
    let follow_up_color = if incoming {
        semantic_danger(ui.visuals().dark_mode)
    } else {
        hit_output_text_color(ui.visuals().dark_mode)
    };
    let damage_cell_rect = egui::Rect::from_min_max(
        egui::pos2(x + layout.damage_x, rect.top()),
        egui::pos2(x + layout.hp_x - 8.0, rect.bottom()),
    );
    let base_damage_rect = draw_damage_number(
        ui,
        damage_cell_rect,
        hit.damage,
        assets.damage_digits,
        follow_up_color,
    );
    draw_follow_up_damage_badge(
        ui,
        damage_cell_rect,
        base_damage_rect,
        hit,
        assets.follow_up_damage_digits,
        follow_up_color,
    );

    let hp_fraction = (hit.target_hp_percent / 100.0).clamp(0.0, 1.0) as f32;
    let hp_cell_left = x + layout.hp_x - 6.0;
    let hp_cell_right = (rect.right() - 4.0).min(ui.clip_rect().right() - 4.0);
    let hp_cell_rect = egui::Rect::from_min_max(
        egui::pos2(hp_cell_left, rect.top() + 2.0),
        egui::pos2(hp_cell_right.max(hp_cell_left), rect.bottom() - 2.0),
    );
    painter.rect_filled(hp_cell_rect, 4.0, ui.visuals().faint_bg_color);
    painter.rect_filled(
        egui::Rect::from_min_size(
            hp_cell_rect.min,
            egui::vec2(hp_cell_rect.width() * hp_fraction, hp_cell_rect.height()),
        ),
        4.0,
        if hp_fraction > 0.5 {
            semantic_success(ui.visuals().dark_mode).gamma_multiply(0.16)
        } else if hp_fraction > 0.2 {
            semantic_warning(ui.visuals().dark_mode).gamma_multiply(0.16)
        } else {
            semantic_danger(ui.visuals().dark_mode).gamma_multiply(0.16)
        },
    );
    draw_target_hp_text(ui, hp_cell_rect, hit, text_color, mono);
    response.on_hover_text(hit_detail_hover_text(hit, true));
}

fn draw_hit_metric_row(ui: &mut egui::Ui, metrics: [(&str, String, Color32); 5]) {
    const CARD_HEIGHT: f32 = 56.0;

    ui.columns(5, |columns| {
        for (column, (label, value, color)) in columns.iter_mut().zip(metrics) {
            hit_metric_card_sized(
                column,
                label,
                &value,
                color,
                egui::vec2(column.available_width(), CARD_HEIGHT),
            );
        }
    });
}

fn hit_metric_card_sized(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    color: Color32,
    size: egui::Vec2,
) {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect(
        rect,
        6.0,
        shadcn_card_hover(ui.visuals().dark_mode),
        Stroke::new(1.0, shadcn_border(ui.visuals().dark_mode)),
        egui::StrokeKind::Inside,
    );
    let content_rect = rect.shrink2(egui::vec2(8.0, 5.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(egui::Layout::top_down(egui::Align::Center)),
        |ui| {
            ui.set_clip_rect(content_rect);
            ui.add_sized(
                egui::vec2(content_rect.width(), 24.0),
                egui::Label::new(
                    RichText::new(value)
                        .monospace()
                        .size(15.0)
                        .strong()
                        .color(color),
                )
                .truncate()
                .halign(egui::Align::Center),
            );
            ui.add_sized(
                egui::vec2(content_rect.width(), 16.0),
                egui::Label::new(
                    RichText::new(label)
                        .size(10.0)
                        .color(ui.visuals().weak_text_color()),
                )
                .truncate()
                .halign(egui::Align::Center),
            );
        },
    );
    response.on_hover_text(format!("{label}：{value}"));
}

fn draw_clipped_label(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    text: &str,
    font: egui::FontId,
    color: Color32,
    align: egui::Align,
    hover_text: Option<&str>,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let display_text = truncate_text_to_width(ui, text, &font, color, rect.width());
    let (position, anchor) = match align {
        egui::Align::Min => (rect.left_center(), egui::Align2::LEFT_CENTER),
        egui::Align::Center => (rect.center(), egui::Align2::CENTER_CENTER),
        egui::Align::Max => (rect.right_center(), egui::Align2::RIGHT_CENTER),
    };
    ui.painter()
        .with_clip_rect(rect)
        .text(position, anchor, display_text, font, color);
    let id = ui.next_auto_id();
    let response = ui.interact(rect, id, egui::Sense::hover());
    if let Some(hover_text) = hover_text {
        response.on_hover_text(hover_text);
    }
}

fn truncate_text_to_width(
    ui: &egui::Ui,
    text: &str,
    font: &egui::FontId,
    color: Color32,
    max_width: f32,
) -> String {
    let text_width = |value: &str| {
        ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(value.to_owned(), font.clone(), color)
                .size()
                .x
        })
    };
    if text_width(text) <= max_width {
        return text.to_owned();
    }

    let chars = text.chars().collect::<Vec<_>>();
    let ellipsis = "…";
    if text_width(ellipsis) > max_width {
        return String::new();
    }
    let mut low = 0;
    let mut high = chars.len();
    while low < high {
        let middle = (low + high).div_ceil(2);
        let mut candidate = chars[..middle].iter().collect::<String>();
        candidate.push('…');
        if text_width(&candidate) <= max_width {
            low = middle;
        } else {
            high = middle - 1;
        }
    }

    chars[..low].iter().collect::<String>() + ellipsis
}

fn draw_target_hp_text(
    ui: &mut egui::Ui,
    cell_rect: egui::Rect,
    hit: &crate::model::Hit,
    target_color: Color32,
    hp_font: egui::FontId,
) {
    let text_rect = cell_rect.shrink2(egui::vec2(8.0, 0.0));
    let target_rect = egui::Rect::from_min_max(
        text_rect.min,
        egui::pos2(text_rect.right(), text_rect.center().y),
    );
    let hp_rect = egui::Rect::from_min_max(
        egui::pos2(text_rect.left(), text_rect.center().y),
        text_rect.max,
    );
    let target = "Target HP";
    let hp = format!(
        "{} / {}  {:.1}%",
        format_number(hit.target_hp_after),
        format_number(hit.target_max_hp),
        hit.target_hp_percent
    );
    draw_clipped_label(
        ui,
        target_rect,
        target,
        egui::FontId::proportional(12.0),
        target_color,
        egui::Align::Min,
        None,
    );
    draw_clipped_label(
        ui,
        hp_rect,
        &hp,
        hp_font,
        ui.visuals().weak_text_color(),
        egui::Align::Min,
        None,
    );
}

fn draw_direction_summary(ui: &mut egui::Ui, summary: HitDirectionSummary) {
    ui.add_space(5.0);
    let text = format!(
        "已确认输出 {}（{} 次） · 候选输出 {}（{} 次，占总输出 {:.1}%）",
        format_number(summary.outgoing_damage),
        summary.outgoing_hits,
        format_number(summary.unknown_damage),
        summary.unknown_hits,
        summary.unknown_share()
    );
    ui.add(
        egui::Label::new(
            RichText::new(&text)
                .size(10.5)
                .color(ui.visuals().weak_text_color()),
        )
        .truncate(),
    )
    .on_hover_text(text);
}

fn draw_hit_column_separators(
    painter: &egui::Painter,
    rect: egui::Rect,
    layout: CharacterHitLayout,
) {
    let color = if painter.ctx().style().visuals.dark_mode {
        Color32::from_rgba_unmultiplied(255, 255, 255, 92)
    } else {
        Color32::from_rgba_unmultiplied(70, 74, 82, 88)
    };
    for separator in layout.separators {
        let x = rect.left() + separator;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(1.0, color),
        );
    }
}

fn draw_team_hit_column_separators(
    painter: &egui::Painter,
    rect: egui::Rect,
    layout: TeamHitLayout,
) {
    let color = if painter.ctx().style().visuals.dark_mode {
        Color32::from_rgba_unmultiplied(255, 255, 255, 92)
    } else {
        Color32::from_rgba_unmultiplied(70, 74, 82, 88)
    };
    for separator in layout.separators {
        let x = rect.left() + separator;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(1.0, color),
        );
    }
}

struct IoFmtWriter<'a, W: IoWrite> {
    inner: &'a mut W,
    error: Option<String>,
}

impl<'a, W: IoWrite> IoFmtWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self { inner, error: None }
    }

    fn finish(self) -> Result<(), String> {
        self.error.map_or(Ok(()), Err)
    }
}

impl<W: IoWrite> std::fmt::Write for IoFmtWriter<'_, W> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.error.is_some() {
            return Err(std::fmt::Error);
        }
        if let Err(error) = self.inner.write_all(value.as_bytes()) {
            self.error = Some(error.to_string());
            return Err(std::fmt::Error);
        }
        Ok(())
    }
}

fn default_export_filename() -> String {
    format!("nte_capture_{}.json", Local::now().format("%Y%m%d_%H%M%S"))
}

fn json_option_time(value: Option<f64>) -> String {
    value
        .map(|timestamp| json_string(&format_time(timestamp)))
        .unwrap_or_else(|| "null".to_owned())
}

fn json_option_f64(value: Option<f64>) -> String {
    value.map(json_f64).unwrap_or_else(|| "null".to_owned())
}

fn json_f64(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.3}")
    } else {
        "null".to_owned()
    }
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0C}' => escaped.push_str("\\f"),
            ch if ch.is_control() => {
                write!(&mut escaped, "\\u{:04x}", ch as u32).ok();
            }
            ch => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn character_text_field(ui: &mut egui::Ui, label: &str, value: &mut String, dirty: &mut bool) {
    ui.label(label);
    if ui
        .add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY))
        .changed()
    {
        *dirty = true;
    }
    ui.end_row();
}

fn next_search_match(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(current.map_or(0, |index| (index + 1) % len))
    }
}

fn previous_search_match(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(current.map_or(
            len - 1,
            |index| {
                if index == 0 { len - 1 } else { index - 1 }
            },
        ))
    }
}

fn encrypted_ini_match_cursor_range(
    text: &str,
    query: &str,
    match_index: usize,
) -> Option<egui::text::CCursorRange> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let start = byte_to_char_index(text, match_index);
    let end = byte_to_char_index(
        text,
        byte_index_after_chars(text, match_index, query.chars().count())
            .unwrap_or(match_index + query.len()),
    );
    Some(egui::text::CCursorRange::two(
        egui::text::CCursor::new(start),
        egui::text::CCursor::new(end),
    ))
}

fn encrypted_ini_match_byte_range(
    text: &str,
    query: &str,
    match_index: usize,
) -> Option<std::ops::Range<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let end = byte_index_after_chars(text, match_index, query.chars().count())
        .unwrap_or(match_index + query.len())
        .min(text.len());
    (match_index < end && text.is_char_boundary(match_index) && text.is_char_boundary(end))
        .then_some(match_index..end)
}

fn encrypted_ini_layout_galley(
    ui: &egui::Ui,
    request: EncryptedIniLayoutRequest<'_>,
    cache: &mut EncryptedIniLayoutCache,
) -> Arc<egui::Galley> {
    let text_color = ui.visuals().widgets.inactive.text_color();
    let query = request.query.trim();
    let highlight_query = if query.is_empty() || request.matches.is_empty() {
        ""
    } else {
        query
    };
    let current_match_byte = request
        .current_match_byte
        .filter(|_| !highlight_query.is_empty());
    let key = EncryptedIniLayoutCacheKey {
        text_len: request.text.len(),
        text_hash: encrypted_ini_text_fingerprint(request.text),
        query: highlight_query.to_owned(),
        current_match_byte,
        dark_mode: request.dark_mode,
        text_color,
    };
    if cache.key.as_ref() == Some(&key)
        && let Some(galley) = &cache.galley
    {
        return Arc::clone(galley);
    }

    let layout_job = encrypted_ini_layout_job(
        ui,
        request.text,
        highlight_query,
        request.matches,
        current_match_byte,
        request.wrap_width,
        request.dark_mode,
    );
    let galley = ui.fonts(|fonts| fonts.layout_job(layout_job));
    cache.key = Some(key);
    cache.galley = Some(Arc::clone(&galley));
    galley
}

fn encrypted_ini_layout_job(
    ui: &egui::Ui,
    text: &str,
    query: &str,
    matches: &[usize],
    current_match_byte: Option<usize>,
    _wrap_width: f32,
    dark_mode: bool,
) -> egui::text::LayoutJob {
    let text_color = ui.visuals().widgets.inactive.text_color();
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let base_format = egui::text::TextFormat {
        font_id,
        color: text_color,
        ..Default::default()
    };
    let mut match_format = base_format.clone();
    match_format.background = if dark_mode {
        Color32::from_rgb(82, 62, 12)
    } else {
        Color32::from_rgb(254, 240, 138)
    };
    match_format.color = if dark_mode {
        Color32::from_rgb(254, 249, 195)
    } else {
        Color32::from_rgb(63, 63, 70)
    };
    let mut current_format = match_format.clone();
    current_format.background = Color32::from_rgb(37, 99, 235);
    current_format.color = Color32::WHITE;

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut cursor = 0;
    for &start in matches {
        let Some(range) = encrypted_ini_match_byte_range(text, query, start) else {
            continue;
        };
        if range.start < cursor {
            continue;
        }
        if cursor < range.start {
            job.append(&text[cursor..range.start], 0.0, base_format.clone());
        }
        let format = if Some(range.start) == current_match_byte {
            current_format.clone()
        } else {
            match_format.clone()
        };
        job.append(&text[range.clone()], 0.0, format);
        cursor = range.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base_format);
    } else if text.is_empty() {
        job.append("", 0.0, base_format);
    }
    job
}

fn byte_to_char_index(text: &str, byte_index: usize) -> usize {
    text[..byte_index.min(text.len())].chars().count()
}

fn byte_index_after_chars(text: &str, byte_index: usize, char_count: usize) -> Option<usize> {
    let mut remaining = char_count;
    for (offset, ch) in text.get(byte_index..)?.char_indices() {
        if remaining == 0 {
            return Some(byte_index + offset);
        }
        remaining -= 1;
        if remaining == 0 {
            return Some(byte_index + offset + ch.len_utf8());
        }
    }
    (remaining == 0).then_some(text.len())
}

fn line_column_for_byte(text: &str, byte_index: usize) -> (usize, usize) {
    let prefix = &text[..byte_index.min(text.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, tail)| tail)
        .chars()
        .count()
        + 1;
    (line, column)
}

fn write_abyss_half_json<W: std::fmt::Write + ?Sized>(
    out: &mut W,
    key: &str,
    party: &PartyCombatState,
    trailing_comma: bool,
) {
    let mut rows: Vec<_> = party.stats.values().collect();
    rows.sort_by(|left, right| right.damage.total_cmp(&left.damage));
    writeln!(out, "    \"{key}\": {{").ok();
    writeln!(out, "      \"hits\": {},", party.hits.len()).ok();
    writeln!(
        out,
        "      \"total_damage\": {},",
        json_f64(party.total_damage)
    )
    .ok();
    writeln!(
        out,
        "      \"total_damage_taken\": {},",
        json_f64(party.total_damage_taken)
    )
    .ok();
    writeln!(out, "      \"dps\": {},", json_f64(party.dps())).ok();
    writeln!(
        out,
        "      \"duration_seconds\": {},",
        json_f64(party.duration())
    )
    .ok();
    writeln!(
        out,
        "      \"started_at_unix\": {},",
        json_option_f64(party.started_at)
    )
    .ok();
    writeln!(
        out,
        "      \"ended_at_unix\": {},",
        json_option_f64(party.ended_at)
    )
    .ok();
    writeln!(out, "      \"party\": [").ok();
    for (index, row) in rows.iter().enumerate() {
        let share = if party.total_damage > 0.0 {
            row.damage / party.total_damage * 100.0
        } else {
            0.0
        };
        writeln!(out, "        {{").ok();
        writeln!(out, "          \"char_id\": {},", row.char_id).ok();
        writeln!(out, "          \"name\": {},", json_string(&row.name)).ok();
        writeln!(out, "          \"hits\": {},", row.hits).ok();
        writeln!(out, "          \"damage\": {},", json_f64(row.damage)).ok();
        writeln!(out, "          \"hits_taken\": {},", row.hits_taken).ok();
        writeln!(
            out,
            "          \"damage_taken\": {},",
            json_f64(row.damage_taken)
        )
        .ok();
        writeln!(out, "          \"dps\": {},", json_f64(row.dps())).ok();
        writeln!(out, "          \"share_percent\": {}", json_f64(share)).ok();
        writeln!(
            out,
            "        }}{}",
            if index + 1 == rows.len() { "" } else { "," }
        )
        .ok();
    }
    writeln!(out, "      ]").ok();
    writeln!(out, "    }}{}", if trailing_comma { "," } else { "" }).ok();
}

fn compact_metric(ui: &mut egui::Ui, label: &str, value: String, color: Color32, prominent: bool) {
    let id = ui.make_persistent_id(("compact_metric", label));
    let hovered = ui
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|pointer| ui.max_rect().contains(pointer));
    let hover = ui.ctx().animate_bool_with_time(id, hovered, 0.14);
    let fill = mix_color(
        shadcn_card(ui.visuals().dark_mode),
        shadcn_card_hover(ui.visuals().dark_mode),
        hover,
    );
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .stroke(Stroke::new(
            1.0,
            mix_color(
                shadcn_border(ui.visuals().dark_mode),
                theme_accent(ui.visuals().dark_mode).gamma_multiply(0.55),
                hover,
            ),
        ))
        .inner_margin(egui::Margin::symmetric(4, 4))
        .show(ui, |ui| {
            ui.set_min_height(38.0);
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(
                    RichText::new(value)
                        .size(if prominent { 17.0 } else { 15.0 })
                        .strong()
                        .color(color),
                );
                ui.label(
                    RichText::new(label)
                        .size(9.5)
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });
}

fn party_row_height(available_height: f32, row_count: usize) -> f32 {
    if row_count == 0 {
        return 52.0;
    }

    let spacing = 5.0 * row_count.saturating_sub(1) as f32;
    ((available_height - spacing - 2.0) / row_count as f32).clamp(38.0, 52.0)
}

fn primary_button(label: &'static str, dark_mode: bool) -> egui::Button<'static> {
    let fill = theme_accent(dark_mode);
    egui::Button::new(RichText::new(label).strong().color(contrast_text(fill)))
        .fill(fill)
        .stroke(Stroke::new(1.0, fill))
}

fn status_color(status: &str, paused: bool, dark_mode: bool) -> Color32 {
    if paused {
        semantic_warning(dark_mode)
    } else if status.contains("失败") || status.contains("不可用") || status.contains("未检测到")
    {
        semantic_danger(dark_mode)
    } else if status.contains("正在")
        || status.contains("启动")
        || status.contains("导入")
        || status.contains("处理")
    {
        semantic_warning(dark_mode)
    } else {
        semantic_success(dark_mode)
    }
}

fn semantic_success(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(74, 222, 128)
    } else {
        Color32::from_rgb(22, 128, 76)
    }
}

fn semantic_warning(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(250, 204, 21)
    } else {
        Color32::from_rgb(161, 98, 7)
    }
}

fn semantic_danger(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(248, 113, 113)
    } else {
        Color32::from_rgb(190, 55, 65)
    }
}

fn theme_accent(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(250, 250, 250)
    } else {
        Color32::from_rgb(24, 24, 27)
    }
}

fn hit_output_badge_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(63, 63, 70)
    } else {
        Color32::from_rgb(24, 24, 27)
    }
}

fn hit_output_text_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(244, 244, 245)
    } else {
        Color32::from_rgb(24, 24, 27)
    }
}

fn readable_accent(color: Color32, dark_mode: bool) -> Color32 {
    let luminance = 0.2126 * f32::from(color.r())
        + 0.7152 * f32::from(color.g())
        + 0.0722 * f32::from(color.b());
    if !dark_mode && luminance > 210.0 {
        Color32::from_rgb(82, 82, 91)
    } else if dark_mode && luminance < 52.0 {
        Color32::from_rgb(161, 161, 170)
    } else {
        color
    }
}

fn contrast_text(background: Color32) -> Color32 {
    let luminance = 0.2126 * f32::from(background.r())
        + 0.7152 * f32::from(background.g())
        + 0.0722 * f32::from(background.b());
    if luminance > 150.0 {
        Color32::from_rgb(9, 9, 11)
    } else {
        Color32::from_rgb(250, 250, 250)
    }
}

fn shadcn_background(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(9, 9, 11)
    } else {
        Color32::from_rgb(250, 250, 250)
    }
}

fn shadcn_foreground(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(250, 250, 250)
    } else {
        Color32::from_rgb(9, 9, 11)
    }
}

fn shadcn_card(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(24, 24, 27)
    } else {
        Color32::from_rgb(255, 255, 255)
    }
}

fn shadcn_card_hover(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(31, 31, 35)
    } else {
        Color32::from_rgb(248, 248, 249)
    }
}

fn shadcn_muted(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(39, 39, 42)
    } else {
        Color32::from_rgb(228, 228, 231)
    }
}

fn shadcn_border(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(39, 39, 42)
    } else {
        Color32::from_rgb(228, 228, 231)
    }
}

fn mix_color(from: Color32, to: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |from: u8, to: u8| {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * amount).round() as u8
    };
    Color32::from_rgba_unmultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

fn ease_out_cubic(value: f32) -> f32 {
    1.0 - (1.0 - value.clamp(0.0, 1.0)).powi(3)
}

fn format_number(value: f64) -> String {
    let rounded = value.round() as i64;
    let source = rounded.abs().to_string();
    let grouped = source
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(",");
    if rounded < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn format_time(timestamp: f64) -> String {
    DateTime::<Local>::from(std::time::UNIX_EPOCH + Duration::from_secs_f64(timestamp.max(0.0)))
        .format("%H:%M:%S%.3f")
        .to_string()
}

fn format_short_time(timestamp: f64) -> String {
    DateTime::<Local>::from(std::time::UNIX_EPOCH + Duration::from_secs_f64(timestamp.max(0.0)))
        .format("%H:%M:%S")
        .to_string()
}

fn show_detail_limit_notice(ui: &mut egui::Ui, filtered_count: usize) {
    if filtered_count > MAX_DETAIL_HITS {
        ui.label(
            RichText::new(format!(
                "仅显示最近 {} 条，当前筛选共 {} 条；完整保留范围内统计已计入上方汇总。",
                format_number(MAX_DETAIL_HITS as f64),
                format_number(filtered_count as f64)
            ))
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(4.0);
    }
}

fn character_color(
    char_id: u32,
    characters: &HashMap<u32, CharacterInfo>,
    fallback_index: usize,
) -> Color32 {
    if let Some(value) = characters
        .get(&char_id)
        .and_then(|row| row.color.as_deref())
        && let Some(color) = parse_hex_color(value)
    {
        return color;
    }
    const PALETTE: [Color32; 6] = [
        Color32::from_rgb(193, 74, 105),
        Color32::from_rgb(112, 91, 179),
        Color32::from_rgb(70, 164, 126),
        Color32::from_rgb(210, 145, 62),
        Color32::from_rgb(72, 137, 195),
        Color32::from_rgb(171, 89, 178),
    ];
    PALETTE[(char_id as usize + fallback_index) % PALETTE.len()]
}

fn parse_hex_color(value: &str) -> Option<Color32> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return None;
    }
    Some(Color32::from_rgb(
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

fn data_root() -> PathBuf {
    if PathBuf::from(CHARACTER_DATA_PATH).is_file() {
        return PathBuf::from(".");
    }
    std::env::current_exe()
        .ok()
        .into_iter()
        .flat_map(|path| path.ancestors().map(PathBuf::from).collect::<Vec<_>>())
        .find(|path| path.join(CHARACTER_DATA_PATH).is_file())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::{
        DpsApp, HitDetailFilter, QteTypeFilterSummary, UiConfigSavePlan, adjusted_cached_index,
        cached_hit_row, compare_cached_team_hits, damage_digit_key_for_hit,
        damage_digit_resource_path, damage_number_digits_text, follow_up_damage_digit_key_for_hit,
        hit_detail_filter_available, hit_type_label, is_party_member_row, mixed_damage_digit_key,
        qte_type_filter_label, reaction_text_key_for_hit,
        reaction_text_key_from_trigger_attack_type, resolve_cached_hit, summarize_qte_type_filters,
    };
    use crate::config::UiConfig;
    use crate::encrypted_ini::{
        EncryptedIniKey, decrypt_encrypted_ini_text, encrypt_aes256_ecb,
        encrypt_encrypted_ini_records, encrypt_encrypted_ini_text, encrypted_ini_search_matches,
        encrypted_ini_text_fingerprint, parse_encrypted_ini_text, pkcs7_pad,
    };
    use crate::model::{CharacterInfo, CharacterStats, Hit};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
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
        };
        let current = UiConfig {
            opacity: 0.9,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
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
        };
        let current = UiConfig {
            opacity: 0.9,
            dark_mode: false,
            always_on_top: true,
            server_damage_calibration: false,
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
