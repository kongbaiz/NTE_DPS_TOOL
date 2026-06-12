use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Color32, RichText, Stroke};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, LWA_ALPHA,
    SetLayeredWindowAttributes, SetWindowLongPtrW, WS_EX_LAYERED,
};

use crate::capture::{
    CaptureDevice, CaptureHandle, import_capture_json, import_pcapng, list_devices, start_capture,
};
use crate::hotkey::{HotkeyEvent, HotkeyHandle};
use crate::model::{
    AbyssEvent, AbyssHalf, CharacterInfo, CombatState, EngineEvent, PartyCombatState,
};
use crate::network::{GameNetwork, detect_game_device};
use crate::parser::load_characters;

#[derive(Clone, Copy)]
enum DebugImportKind {
    Pcapng,
    CaptureJson,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DebugTab {
    #[default]
    Packets,
    Characters,
    Environment,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum HitDetailFilter {
    #[default]
    All,
    Outgoing,
    Incoming,
}

impl HitDetailFilter {
    fn matches(self, hit: &crate::model::Hit) -> bool {
        match self {
            Self::All => true,
            Self::Outgoing => hit.direction != "incoming",
            Self::Incoming => hit.direction == "incoming",
        }
    }
}

#[derive(Clone, Default)]
struct CharacterEditForm {
    id: String,
    name_zh: String,
    name_en: String,
    codename: String,
    verified: bool,
    color: String,
    avatar: String,
}

struct CharacterEditorState {
    document: serde_json::Value,
    selected_id: Option<String>,
    form: CharacterEditForm,
    search: String,
    new_id: String,
    dirty: bool,
    message: String,
    cancel_selection: Option<String>,
}

impl CharacterEditorState {
    fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
        let document: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("角色表 JSON 无效: {error}"))?;
        if !document
            .get("characters")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err("characters.json 缺少 characters 对象".to_owned());
        }
        Ok(Self {
            document,
            selected_id: None,
            form: CharacterEditForm::default(),
            search: String::new(),
            new_id: String::new(),
            dirty: false,
            message: String::new(),
            cancel_selection: None,
        })
    }

    fn character_ids(&self) -> Vec<String> {
        let mut ids = self
            .document
            .get("characters")
            .and_then(serde_json::Value::as_object)
            .map(|characters| characters.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        ids.sort_by_key(|id| id.parse::<u32>().unwrap_or(u32::MAX));
        ids
    }

    fn select(&mut self, id: &str) {
        let Some(row) = self
            .document
            .get("characters")
            .and_then(serde_json::Value::as_object)
            .and_then(|characters| characters.get(id))
            .and_then(serde_json::Value::as_object)
        else {
            return;
        };
        self.selected_id = Some(id.to_owned());
        self.form = CharacterEditForm {
            id: id.to_owned(),
            name_zh: json_string_field(row, "name_zh"),
            name_en: json_string_field(row, "name_en"),
            codename: json_string_field(row, "codename"),
            verified: row
                .get("verified")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            color: json_string_field(row, "color"),
            avatar: json_string_field(row, "avatar"),
        };
        self.dirty = false;
        self.message.clear();
        self.cancel_selection = None;
    }

    fn start_new(&mut self) -> Result<(), String> {
        let id = self.new_id.trim();
        let parsed = id
            .parse::<u32>()
            .map_err(|_| "角色 ID 必须是正整数".to_owned())?;
        if parsed == 0 {
            return Err("角色 ID 必须大于 0".to_owned());
        }
        let id = parsed.to_string();
        if self
            .document
            .get("characters")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|characters| characters.contains_key(&id))
        {
            self.select(&id);
            return Err(format!("ID {id} 已存在，已切换到现有记录"));
        }
        self.cancel_selection = self.selected_id.clone();
        self.selected_id = None;
        self.form = CharacterEditForm {
            id,
            ..Default::default()
        };
        self.new_id.clear();
        self.dirty = true;
        self.message = "正在新增角色，填写后保存".to_owned();
        Ok(())
    }

    fn apply_form(&mut self) -> Result<String, String> {
        let id = self
            .form
            .id
            .trim()
            .parse::<u32>()
            .map_err(|_| "角色 ID 必须是正整数".to_owned())?
            .to_string();
        if self.form.name_zh.trim().is_empty() && self.form.name_en.trim().is_empty() {
            return Err("中文名和英文名至少填写一项".to_owned());
        }
        let color = self.form.color.trim();
        if !color.is_empty() && parse_hex_color(color).is_none() {
            return Err("颜色必须是 #RRGGBB 格式".to_owned());
        }
        if let Some(selected_id) = &self.selected_id
            && selected_id != &id
        {
            return Err("现有角色 ID 不允许直接修改，请新增记录".to_owned());
        }
        let characters = self
            .document
            .get_mut("characters")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| "characters.json 缺少 characters 对象".to_owned())?;
        let row = characters
            .entry(id.clone())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let row = row
            .as_object_mut()
            .ok_or_else(|| format!("ID {id} 的数据不是 JSON 对象"))?;
        set_json_string(row, "name_zh", self.form.name_zh.trim());
        set_json_string(row, "name_en", self.form.name_en.trim());
        set_json_string(row, "codename", self.form.codename.trim());
        row.insert(
            "verified".to_owned(),
            serde_json::Value::Bool(self.form.verified),
        );
        set_optional_json_string(row, "color", color);
        set_optional_json_string(row, "avatar", self.form.avatar.trim());
        self.selected_id = Some(id.clone());
        self.form.id = id.clone();
        self.dirty = false;
        self.cancel_selection = None;
        Ok(id)
    }

    fn cancel_edit(&mut self) {
        if let Some(id) = self
            .cancel_selection
            .take()
            .or_else(|| self.selected_id.clone())
        {
            self.select(&id);
        } else {
            self.form = CharacterEditForm::default();
            self.dirty = false;
            self.message.clear();
        }
    }
}

pub struct DpsApp {
    characters: Arc<HashMap<u32, CharacterInfo>>,
    avatar_textures: HashMap<String, egui::TextureHandle>,
    state: CombatState,
    selected_abyss_half: AbyssHalf,
    abyss_compact_mode: bool,
    hit_detail_char_id: Option<u32>,
    hit_detail_filter: HitDetailFilter,
    devices: Vec<CaptureDevice>,
    selected_device: usize,
    local_ip: String,
    game_network: Option<GameNetwork>,
    filter: String,
    include_incoming: bool,
    capture: Option<CaptureHandle>,
    raw_capture_path: Option<PathBuf>,
    replay_stop: Option<Arc<AtomicBool>>,
    replay_thread: Option<thread::JoinHandle<()>>,
    sender: Sender<EngineEvent>,
    receiver: Receiver<EngineEvent>,
    status: String,
    diagnostic: Option<String>,
    last_error: Option<String>,
    debug_open: bool,
    debug_tab: DebugTab,
    debug_only_hits: bool,
    debug_search: String,
    character_editor: CharacterEditorState,
    paused: bool,
    dark_mode: bool,
    always_on_top: bool,
    mouse_passthrough: bool,
    opacity: f32,
    applied_opacity: Option<f32>,
    opacity_reapply_frames: u8,
    pending_debug_import: Option<(DebugImportKind, u8)>,
    hotkey_receiver: Receiver<HotkeyEvent>,
    _hotkey: HotkeyHandle,
}

impl DpsApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        configure_style(&cc.egui_ctx, false);
        let (hotkey, hotkey_receiver) = HotkeyHandle::start(cc.egui_ctx.clone());
        let (sender, receiver) = unbounded();
        let data_root = data_root();
        let characters_path = data_root.join("characters.json");
        let characters = load_characters(characters_path.as_path()).unwrap_or_default();
        let avatar_textures = load_character_avatars(&cc.egui_ctx, &data_root, &characters);
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
        let (selected_device, game_network, status, diagnostic) = match device_error {
            Some(error) => (0, None, "采集环境不可用".to_owned(), Some(error)),
            None => match detect_game_device(&devices) {
                Ok((index, network)) => (index, Some(network), "已就绪".to_owned(), None),
                Err(error) => (0, None, "未检测到游戏".to_owned(), Some(error)),
            },
        };
        let local_ip = game_network
            .as_ref()
            .map(|network| network.local_ip.to_string())
            .unwrap_or_default();
        Self {
            characters: Arc::new(characters),
            avatar_textures,
            state: CombatState::default(),
            selected_abyss_half: AbyssHalf::First,
            abyss_compact_mode: false,
            hit_detail_char_id: None,
            hit_detail_filter: HitDetailFilter::All,
            devices,
            selected_device,
            local_ip,
            game_network,
            filter: "udp".to_owned(),
            include_incoming: true,
            capture: None,
            raw_capture_path: None,
            replay_stop: None,
            replay_thread: None,
            sender,
            receiver,
            status,
            diagnostic,
            last_error: None,
            debug_open: false,
            debug_tab: DebugTab::Packets,
            debug_only_hits: false,
            debug_search: String::new(),
            character_editor,
            paused: false,
            dark_mode: false,
            always_on_top: true,
            mouse_passthrough: false,
            opacity: 0.92,
            applied_opacity: None,
            opacity_reapply_frames: 0,
            pending_debug_import: None,
            hotkey_receiver,
            _hotkey: hotkey,
        }
    }

    fn stop_engine(&mut self) {
        if let Some(mut capture) = self.capture.take() {
            self.raw_capture_path = Some(capture.raw_capture_path().to_path_buf());
            capture.stop();
        }
        if let Some(stop) = self.replay_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(thread) = self.replay_thread.take() {
            let _ = thread.join();
        }
    }

    fn drain_hotkeys(&mut self, ctx: &egui::Context) {
        let home_pressed = ctx.input(|input| input.key_pressed(egui::Key::Home));
        #[cfg(not(feature = "no_debug"))]
        let f12_pressed = ctx.input(|input| input.key_pressed(egui::Key::F12));
        if home_pressed {
            self.toggle_mouse_passthrough(ctx);
        }
        #[cfg(not(feature = "no_debug"))]
        if f12_pressed {
            self.debug_open = !self.debug_open;
        }
        while let Ok(event) = self.hotkey_receiver.try_recv() {
            match event {
                HotkeyEvent::TogglePassthrough => {
                    self.toggle_mouse_passthrough(ctx);
                }
                #[cfg(not(feature = "no_debug"))]
                HotkeyEvent::ToggleDebug => {
                    self.debug_open = !self.debug_open;
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
            "鼠标穿透已开启".to_owned()
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
        let full_rect = ui
            .allocate_exact_size(
                egui::vec2(ui.available_width(), title_height),
                egui::Sense::hover(),
            )
            .0;
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(full_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );

        child.label(
            RichText::new("NTE DPS")
                .size(13.0)
                .strong()
                .color(theme_accent(self.dark_mode)),
        );

        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new("×").frame(false))
                .on_hover_text("关闭")
                .clicked()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if ui
                .add(egui::Button::new("−").frame(false))
                .on_hover_text("最小化")
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            if !self.abyss_compact_mode || !self.state.abyss.is_active() {
                ui.menu_button("外观", |ui| {
                    ui.set_min_width(190.0);
                    ui.horizontal(|ui| {
                        ui.label("透明度");
                        ui.add(
                            egui::Slider::new(&mut self.opacity, 0.35..=1.0)
                                .show_value(true)
                                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
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
                        self.dark_mode = !self.dark_mode;
                        ui.close();
                    }
                });
                let passthrough_label = if self.mouse_passthrough {
                    "穿透中"
                } else {
                    "穿透"
                };
                if ui
                    .selectable_label(self.mouse_passthrough, passthrough_label)
                    .on_hover_text("Home 可随时切换鼠标穿透")
                    .clicked()
                {
                    self.toggle_mouse_passthrough(ui.ctx());
                }
                if ui
                    .selectable_label(self.always_on_top, "置顶")
                    .on_hover_text("保持主窗口位于游戏上方")
                    .clicked()
                {
                    self.toggle_always_on_top(ui.ctx());
                }
            }

            let drag_width = ui.available_width();
            let drag_response = ui.allocate_response(
                egui::vec2(drag_width, title_height),
                egui::Sense::click_and_drag(),
            );
            if drag_response.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        });
    }

    fn start_live(&mut self) {
        self.stop_engine();
        if let Err(error) = self.refresh_game_network() {
            self.last_error = Some(error);
            return;
        }
        let Some(device) = self.devices.get(self.selected_device).cloned() else {
            self.last_error = Some("没有可用抓包设备，请确认已安装 Npcap".to_owned());
            return;
        };
        let local_ip = self.game_network.as_ref().map(|network| network.local_ip);
        let raw_capture_path = default_raw_capture_path();
        self.raw_capture_path = Some(raw_capture_path.clone());
        self.capture = Some(start_capture(
            device,
            local_ip,
            self.filter.clone(),
            self.include_incoming,
            self.characters.clone(),
            self.sender.clone(),
            raw_capture_path,
        ));
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
        self.state.clear();
        self.selected_abyss_half = AbyssHalf::First;
        self.abyss_compact_mode = false;
        self.hit_detail_char_id = None;
        let stop = Arc::new(AtomicBool::new(false));
        self.replay_thread = Some(import_pcapng(
            path,
            self.characters.clone(),
            self.include_incoming,
            self.sender.clone(),
            stop.clone(),
        ));
        self.replay_stop = Some(stop);
        self.status = "正在导入并解析 pcapng...".to_owned();
    }

    fn start_capture_json_import(&mut self, path: PathBuf) {
        self.stop_engine();
        self.state.clear();
        self.selected_abyss_half = AbyssHalf::First;
        self.abyss_compact_mode = false;
        self.hit_detail_char_id = None;
        let stop = Arc::new(AtomicBool::new(false));
        self.replay_thread = Some(import_capture_json(path, self.sender.clone(), stop.clone()));
        self.replay_stop = Some(stop);
        self.status = "正在导入抓包 JSON...".to_owned();
    }

    fn request_debug_import(&mut self, ctx: &egui::Context, kind: DebugImportKind) {
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal),
        );
        self.pending_debug_import = Some((kind, 1));
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
        let path = match kind {
            DebugImportKind::Pcapng => rfd::FileDialog::new()
                .add_filter("Wireshark 抓包", &["pcapng"])
                .pick_file(),
            DebugImportKind::CaptureJson => rfd::FileDialog::new()
                .add_filter("NTE 导出抓包", &["json"])
                .pick_file(),
        };
        ctx.send_viewport_cmd_to(
            debug_viewport_id(),
            egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop),
        );
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::WindowLevel(if self.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }),
        );
        self.opacity_reapply_frames = 2;
        if let Some(path) = path {
            match kind {
                DebugImportKind::Pcapng => self.start_pcapng_import(path),
                DebugImportKind::CaptureJson => self.start_capture_json_import(path),
            }
        }
    }

    fn drain_events(&mut self) {
        if self.paused {
            return;
        }
        self.drain_pending_events();
    }

    fn drain_pending_events(&mut self) {
        for event in self.receiver.try_iter() {
            match event {
                EngineEvent::Hit(hit) => self.state.push_hit(hit),
                EngineEvent::Packet(packet) => self.state.push_packet(packet),
                EngineEvent::Abyss(event) => {
                    if let AbyssEvent::Stage { half, .. } = &event {
                        self.selected_abyss_half = *half;
                        self.abyss_compact_mode = true;
                    } else if matches!(&event, AbyssEvent::Success { .. } | AbyssEvent::Exit { .. })
                    {
                        self.abyss_compact_mode = false;
                    }
                    self.state.apply_abyss_event(event);
                }
                EngineEvent::Status(status) => self.status = status,
                EngineEvent::Error(error) => {
                    self.status = "运行失败".to_owned();
                    self.last_error = Some(error);
                }
                EngineEvent::CaptureStopped => {
                    let stopped_live_capture = if let Some(capture) = self.capture.take() {
                        self.raw_capture_path = Some(capture.raw_capture_path().to_path_buf());
                        true
                    } else {
                        false
                    };
                    self.replay_stop = None;
                    if let Some(thread) = self.replay_thread.take() {
                        let _ = thread.join();
                    }
                    self.status = if stopped_live_capture {
                        self.raw_capture_path
                            .as_ref()
                            .filter(|path| path.is_file())
                            .map(|path| format!("已停止，完整抓包已保存至 {}", path.display()))
                            .unwrap_or_else(|| "已停止".to_owned())
                    } else {
                        "已停止".to_owned()
                    };
                }
            }
        }
    }

    fn export_capture_info(&mut self) {
        self.drain_pending_events();
        if self.state.hits.is_empty() && self.state.packets.is_empty() {
            self.last_error = Some("当前没有可导出的抓包信息".to_owned());
            return;
        }
        if self.capture.is_some() || self.replay_thread.is_some() {
            self.last_error = Some("请先停止抓包或回放，再导出本次抓包信息".to_owned());
            return;
        }

        let Some(path) = rfd::FileDialog::new()
            .add_filter("抓包信息 JSON", &["json"])
            .set_file_name(default_export_filename())
            .save_file()
        else {
            return;
        };

        match std::fs::write(&path, self.capture_export_json()) {
            Ok(()) => {
                self.status = "已导出抓包信息".to_owned();
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("导出抓包信息失败：{error}"));
            }
        }
    }

    fn export_raw_capture(&mut self) {
        if self.capture.is_some() {
            self.last_error = Some("请先停止抓包，再另存完整 PCAPNG".to_owned());
            return;
        }
        let Some(source) = self
            .raw_capture_path
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
        else {
            self.last_error = Some("当前没有可另存的完整 PCAPNG".to_owned());
            return;
        };
        let file_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("nte_raw_capture.pcapng");
        let Some(destination) = rfd::FileDialog::new()
            .add_filter("完整原始抓包", &["pcapng"])
            .set_file_name(file_name)
            .save_file()
        else {
            return;
        };
        if destination == source {
            self.status = format!("完整抓包位于 {}", source.display());
            self.last_error = None;
            return;
        }
        match std::fs::copy(&source, &destination) {
            Ok(_) => {
                self.status = format!("已另存完整抓包至 {}", destination.display());
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("另存完整抓包失败：{error}"));
            }
        }
    }

    fn capture_export_json(&self) -> String {
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

        let mut out = String::new();
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
                json_string(&format!("{:?}", packet.declared_ids))
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
        out
    }

    fn selected_party_state(&self) -> Option<&PartyCombatState> {
        self.state
            .abyss
            .is_active()
            .then(|| self.state.abyss.half(self.selected_abyss_half))
    }

    fn abyss_selector(&mut self, ui: &mut egui::Ui) {
        if !self.state.abyss.is_active() {
            return;
        }
        ui.horizontal(|ui| {
            let floor = self
                .state
                .abyss
                .floor
                .map_or_else(|| "深渊".to_owned(), |floor| format!("深渊 {floor} 层"));
            ui.label(RichText::new(floor).strong());
            ui.separator();
            ui.selectable_value(
                &mut self.selected_abyss_half,
                AbyssHalf::First,
                AbyssHalf::First.label(),
            );
            ui.selectable_value(
                &mut self.selected_abyss_half,
                AbyssHalf::Second,
                AbyssHalf::Second.label(),
            );
            if self.state.abyss.success_at.is_some() {
                ui.separator();
                ui.label(RichText::new("挑战成功").color(Color32::from_rgb(76, 185, 128)));
            }
            if self.abyss_compact_mode {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("展开").clicked() {
                        self.abyss_compact_mode = false;
                    }
                });
            }
        });
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
        ui.columns(4, |columns| {
            compact_metric(
                &mut columns[0],
                "DPS",
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
                Color32::from_rgb(194, 74, 92),
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
                if ui
                    .button(
                        RichText::new("开始")
                            .strong()
                            .color(theme_accent(self.dark_mode)),
                    )
                    .clicked()
                {
                    self.start_live();
                }
            } else if ui.button("停止").clicked() {
                let was_live_capture = self.capture.is_some();
                self.stop_engine();
                self.drain_pending_events();
                self.status = if was_live_capture {
                    self.raw_capture_path
                        .as_ref()
                        .filter(|path| path.is_file())
                        .map(|path| format!("已停止，完整抓包已保存至 {}", path.display()))
                        .unwrap_or_else(|| "已停止".to_owned())
                } else {
                    "已停止".to_owned()
                };
            }
            if ui.button("重置").clicked() {
                self.state.clear();
                self.selected_abyss_half = AbyssHalf::First;
                self.abyss_compact_mode = false;
                self.hit_detail_char_id = None;
            }
            if ui
                .button(if self.paused { "继续" } else { "暂停" })
                .clicked()
            {
                self.paused = !self.paused;
            }
            if self.state.abyss.is_active() && ui.button("折叠").clicked() {
                self.abyss_compact_mode = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let status_color = if self.capture.is_some() {
                    Color32::from_rgb(76, 185, 128)
                } else {
                    ui.visuals().weak_text_color()
                };
                let short_status = if self.capture.is_some() {
                    "记录中"
                } else if self.status.contains("Debug") {
                    "未就绪"
                } else {
                    self.status.as_str()
                };
                ui.small(RichText::new(short_status).color(status_color))
                    .on_hover_text(&self.status);
            });
        });
    }

    fn party_panel(&mut self, ui: &mut egui::Ui) {
        let compact = self.abyss_compact_mode && self.state.abyss.is_active();
        let (mut rows, total_damage): (Vec<_>, f64) =
            if let Some(party) = self.selected_party_state() {
                (party.stats.values().cloned().collect(), party.total_damage)
            } else {
                (
                    self.state.stats.values().cloned().collect(),
                    self.state.total_damage,
                )
            };
        rows.sort_by(|left, right| right.damage.total_cmp(&left.damage));
        let max_damage = rows.first().map_or(1.0, |row| row.damage.max(1.0));
        egui::ScrollArea::vertical()
            .id_salt("party_scroll")
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for (index, row) in rows.iter().enumerate() {
                    let color = character_color(row.char_id, &self.characters, index);
                    let avatar_texture = self
                        .characters
                        .get(&row.char_id)
                        .and_then(|character| character.avatar.as_deref())
                        .and_then(|avatar| self.avatar_textures.get(avatar));
                    let fraction = (row.damage / max_damage) as f32;
                    let share = if total_damage > 0.0 {
                        row.damage / total_damage * 100.0
                    } else {
                        0.0
                    };
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), if compact { 40.0 } else { 44.0 }),
                        egui::Sense::click(),
                    );
                    ui.painter().rect_filled(
                        rect,
                        6.0,
                        if self.dark_mode {
                            Color32::from_rgb(35, 38, 44)
                        } else {
                            Color32::from_rgb(244, 245, 247)
                        },
                    );
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(
                            rect.min,
                            egui::vec2(rect.width() * fraction, rect.height()),
                        ),
                        6.0,
                        color.gamma_multiply(if self.dark_mode { 0.38 } else { 0.18 }),
                    );
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            rect.left_top(),
                            egui::pos2(rect.left() + 4.0, rect.bottom()),
                        ),
                        6.0,
                        color,
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        egui::Align2::CENTER_CENTER,
                        "↗",
                        egui::FontId::proportional(13.0),
                        ui.visuals().weak_text_color(),
                    );
                    let avatar_size = if compact { 32.0 } else { 36.0 };
                    let avatar_rect = pixel_aligned_rect(
                        egui::pos2(rect.left() + 20.0, rect.center().y - avatar_size * 0.5),
                        avatar_size,
                        ui.ctx().pixels_per_point(),
                    );
                    let avatar_border = if self.dark_mode {
                        Color32::from_rgb(78, 82, 92)
                    } else {
                        Color32::from_rgb(210, 213, 220)
                    };
                    ui.painter().rect_filled(avatar_rect, 7.0, avatar_border);
                    if let Some(texture) = avatar_texture {
                        ui.put(
                            avatar_rect,
                            egui::Image::new((texture.id(), avatar_rect.size())).corner_radius(7),
                        );
                        ui.painter().rect_stroke(
                            avatar_rect,
                            7.0,
                            Stroke::new(1.0, avatar_border),
                            egui::StrokeKind::Inside,
                        );
                    } else {
                        ui.painter()
                            .rect_filled(avatar_rect, 7.0, color.gamma_multiply(0.82));
                        ui.painter().text(
                            avatar_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            row.name.chars().next().unwrap_or('?').to_string(),
                            egui::FontId::proportional(14.0),
                            Color32::WHITE,
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
                            "{} · {share:.1}% · 受击 {}",
                            format_number(row.damage),
                            format_number(row.damage_taken)
                        ),
                        egui::FontId::monospace(10.5),
                        ui.visuals().weak_text_color(),
                    );
                    if response.on_hover_text("在独立窗口查看完整命中").clicked() {
                        self.hit_detail_char_id = Some(row.char_id);
                        self.hit_detail_filter = HitDetailFilter::All;
                    }
                    ui.add_space(3.0);
                }
                if rows.is_empty() {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 40.0),
                        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        |ui| {
                            ui.label(
                                RichText::new("等待伤害数据").color(ui.visuals().weak_text_color()),
                            );
                        },
                    );
                }
            });
    }

    fn character_hits(&self, ui: &mut egui::Ui, char_id: u32, filter: HitDetailFilter) {
        let scrollbar_width = ui.style().spacing.scroll.allocated_width().max(10.0);
        let content_width = (ui.available_width() - scrollbar_width - 4.0).max(0.0);
        let layout = CharacterHitLayout::new(content_width);
        draw_character_hit_header(ui, layout);
        let hits = if let Some(party) = self.selected_party_state() {
            &party.hits
        } else {
            &self.state.hits
        };
        let mut character_hits: Vec<_> = hits
            .iter()
            .filter(|hit| hit.char_id == char_id && filter.matches(hit))
            .collect();
        character_hits.sort_by(|left, right| compare_hit_display_order(left, right));
        let hit_count = character_hits.len();
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
        let max_damage = character_hits
            .iter()
            .map(|hit| hit.damage)
            .fold(1.0_f64, f64::max);
        egui::ScrollArea::vertical()
            .id_salt(("character_hits", char_id))
            .max_height(ui.available_height())
            .stick_to_bottom(true)
            .show_rows(ui, 30.0, hit_count, |ui, visible_rows| {
                let visible_count = visible_rows.end.saturating_sub(visible_rows.start);
                for hit in character_hits[visible_rows].iter().take(visible_count) {
                    draw_character_hit_row(ui, layout, hit, max_damage);
                }
            });
    }

    fn hit_detail_panel(&mut self, ctx: &egui::Context, char_id: u32) {
        let stats = if let Some(party) = self.selected_party_state() {
            party.stats.get(&char_id).cloned()
        } else {
            self.state.stats.get(&char_id).cloned()
        };
        let Some(stats) = stats else {
            self.hit_detail_char_id = None;
            return;
        };
        let hits = if let Some(party) = self.selected_party_state() {
            &party.hits
        } else {
            &self.state.hits
        };
        let outgoing_count = hits
            .iter()
            .filter(|hit| hit.char_id == char_id && hit.direction != "incoming")
            .count();
        let incoming_count = hits
            .iter()
            .filter(|hit| hit.char_id == char_id && hit.direction == "incoming")
            .count();
        let avatar_texture = self
            .characters
            .get(&char_id)
            .and_then(|character| character.avatar.as_deref())
            .and_then(|avatar| self.avatar_textures.get(avatar))
            .cloned();
        let character_color = character_color(char_id, &self.characters, 0);
        let title = format!("{} - 命中详情", stats.name);
        let close_requested = ctx.show_viewport_immediate(
            hit_detail_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title(&title)
                .with_inner_size([900.0, 620.0])
                .with_min_inner_size([680.0, 420.0])
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                .with_transparent(true)
                .with_resizable(true),
            |ctx, _class| {
                configure_style(ctx, self.dark_mode);
                let mut close_clicked = false;
                egui::TopBottomPanel::top("hit_detail_title_bar")
                    .exact_height(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show(ctx, |ui| {
                        close_clicked = secondary_title_bar(ui, &title);
                    });
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(if self.dark_mode {
                            Color32::from_rgb(35, 38, 44)
                        } else {
                            Color32::from_rgb(246, 247, 249)
                        })
                        .corner_radius(10)
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
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
                                        egui::Image::new((texture.id(), avatar_rect.size()))
                                            .corner_radius(10),
                                    );
                                } else {
                                    ui.painter().text(
                                        avatar_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        stats.name.chars().next().unwrap_or('?').to_string(),
                                        egui::FontId::proportional(25.0),
                                        Color32::WHITE,
                                    );
                                }
                                ui.add_space(4.0);
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&stats.name).size(20.0).strong());
                                    ui.label(
                                        RichText::new(format!("角色 ID {char_id}"))
                                            .size(11.0)
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                });
                                ui.add_space(12.0);
                                hit_metric_card(
                                    ui,
                                    "总输出",
                                    format_number(stats.damage),
                                    theme_accent(self.dark_mode),
                                );
                                hit_metric_card(
                                    ui,
                                    "DPS",
                                    format_number(stats.dps()),
                                    theme_accent(self.dark_mode),
                                );
                                hit_metric_card(
                                    ui,
                                    "输出次数",
                                    outgoing_count.to_string(),
                                    ui.visuals().text_color(),
                                );
                                hit_metric_card(
                                    ui,
                                    "总受击",
                                    format_number(stats.damage_taken),
                                    Color32::from_rgb(211, 79, 79),
                                );
                                hit_metric_card(
                                    ui,
                                    "战斗时间",
                                    format!("{:.1}s", stats.duration()),
                                    ui.visuals().text_color(),
                                );
                            });
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("伤害类型")
                                .strong()
                                .color(ui.visuals().weak_text_color()),
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
                    });
                    ui.add_space(4.0);
                    ui.separator();
                    self.character_hits(ui, char_id, self.hit_detail_filter);
                });
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.hit_detail_char_id = None;
        }
    }

    fn debug_panel(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.show_viewport_immediate(
            debug_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("NTE Debug")
                .with_inner_size([980.0, 640.0])
                .with_min_inner_size([720.0, 480.0])
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                .with_transparent(true)
                .with_resizable(true),
            |ctx, _class| {
                configure_style(ctx, self.dark_mode);
                let mut close_clicked = false;
                egui::TopBottomPanel::top("debug_title_bar")
                    .exact_height(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show(ctx, |ui| {
                        close_clicked = secondary_title_bar(ui, "NTE Debug");
                    });
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.debug_contents(ui);
                });
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.debug_open = false;
        }
    }

    fn debug_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.debug_tab, DebugTab::Packets, "封包");
            ui.selectable_value(&mut self.debug_tab, DebugTab::Characters, "角色数据");
            ui.selectable_value(&mut self.debug_tab, DebugTab::Environment, "环境");
        });
        ui.separator();
        match self.debug_tab {
            DebugTab::Packets => self.debug_packets_contents(ui),
            DebugTab::Characters => self.debug_characters_contents(ui),
            DebugTab::Environment => self.debug_environment_contents(ui),
        }
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
                        ui.label("原始抓包");
                        let raw_capture_label = self
                            .raw_capture_path
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "开始实时抓包后自动生成".to_owned());
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
                        self.export_capture_info();
                    }
                    let can_export_raw = self.capture.is_none()
                        && self
                            .raw_capture_path
                            .as_ref()
                            .is_some_and(|path| path.is_file());
                    if ui
                        .add_enabled(can_export_raw, egui::Button::new("另存完整 PCAPNG"))
                        .clicked()
                    {
                        self.export_raw_capture();
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
                    .hint_text("IP / ID / 协议名称 / 场景"),
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
                    let searchable = format!(
                        "{} {} {} {:?} {}",
                        packet.source,
                        packet.destination,
                        packet.direction,
                        packet.declared_ids,
                        packet.decoded_text
                    );
                    if !self.debug_search.is_empty()
                        && !searchable
                            .to_lowercase()
                            .contains(&self.debug_search.to_lowercase())
                    {
                        continue;
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
                                RichText::new(&packet.note).color(Color32::from_rgb(235, 188, 95)),
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
                let path = data_root().join("characters.json");
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
                    .color(Color32::from_rgb(235, 188, 95)),
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
                        .hint_text("ID / 中文名 / 英文名"),
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
                        let searchable = format!("{id} {name_zh} {name_en}").to_lowercase();
                        if !search.is_empty() && !searchable.contains(&search) {
                            continue;
                        }
                        let selected =
                            self.character_editor.selected_id.as_deref() == Some(id.as_str());
                        let label = if name_zh.is_empty() {
                            format!("{id}  {name_en}")
                        } else {
                            format!("{id}  {name_zh}  {name_en}")
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
        let path = data_root().join("characters.json");
        let text = match serde_json::to_string_pretty(&self.character_editor.document) {
            Ok(text) => format!("{text}\n"),
            Err(error) => {
                self.character_editor.message = format!("角色表序列化失败: {error}");
                self.character_editor.dirty = true;
                return;
            }
        };
        if let Err(error) = std::fs::write(&path, text) {
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
        configure_style(ctx, self.dark_mode);
        self.drain_events();
        self.drain_hotkeys(ctx);
        self.process_debug_import_dialog(ctx);
        let force_opacity = self.opacity_reapply_frames > 0;
        apply_window_attributes(
            frame,
            self.opacity,
            force_opacity,
            &mut self.applied_opacity,
        );
        self.opacity_reapply_frames = self.opacity_reapply_frames.saturating_sub(1);
        let repaint_interval = if self.capture.is_some() || self.replay_thread.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(500)
        };
        ctx.request_repaint_after(repaint_interval);

        egui::TopBottomPanel::top("custom_title_bar")
            .exact_height(32.0)
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                self.title_bar(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.abyss_compact_mode || !self.state.abyss.is_active() {
                self.controls(ui);
                ui.separator();
            }
            if self.state.abyss.is_active() {
                self.abyss_selector(ui);
            }
            self.summary_bar(ui);
            ui.add_space(2.0);
            ui.label(RichText::new("队伍").size(12.0).strong());
            self.party_panel(ui);
        });

        #[cfg(not(feature = "no_debug"))]
        if self.debug_open {
            self.debug_panel(ctx);
        }
        if let Some(char_id) = self.hit_detail_char_id {
            self.hit_detail_panel(ctx, char_id);
        }
        apply_rounding_to_process_windows();
        if let Some(error) = self.last_error.clone() {
            egui::Window::new("错误")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(error);
                    if ui.button("关闭").clicked() {
                        self.last_error = None;
                    }
                });
        }
    }
}

impl Drop for DpsApp {
    fn drop(&mut self) {
        self.stop_engine();
    }
}

fn install_fonts(ctx: &egui::Context) {
    let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\msyh.ttc") else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "microsoft-yahei".to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "microsoft-yahei".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn secondary_title_bar(ui: &mut egui::Ui, title: &str) -> bool {
    let title_height = 28.0;
    let mut close_clicked = false;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .size(13.0)
                .strong()
                .color(ui.visuals().text_color()),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new("×").frame(false))
                .on_hover_text("关闭")
                .clicked()
            {
                close_clicked = true;
            }
            if ui
                .add(egui::Button::new("−").frame(false))
                .on_hover_text("最小化")
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            let drag_response = ui.allocate_response(
                egui::vec2(ui.available_width(), title_height),
                egui::Sense::click_and_drag(),
            );
            if drag_response.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        });
    });
    close_clicked
}

fn debug_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_debug_viewport")
}

fn hit_detail_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_hit_detail_viewport")
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
        let path = root.join(avatar);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(image) = image::load_from_memory(&bytes) else {
            continue;
        };
        // Keep the source resolution so the texture remains sharp if the window
        // moves to a monitor with a different DPI after startup.
        let image = image.to_rgba8();
        let size = [image.width() as usize, image.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
        let texture = ctx.load_texture(
            format!("character-avatar:{avatar}"),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        textures.insert(avatar.to_owned(), texture);
    }
    textures
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
        visuals.panel_fill = Color32::from_rgb(23, 25, 30);
        visuals.window_fill = Color32::from_rgb(28, 30, 36);
        visuals.extreme_bg_color = Color32::from_rgb(18, 20, 24);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(48));
    } else {
        visuals.panel_fill = Color32::from_rgb(250, 250, 248);
        visuals.window_fill = Color32::WHITE;
        visuals.extreme_bg_color = Color32::from_rgb(238, 239, 241);
        visuals.widgets.noninteractive.bg_stroke =
            Stroke::new(1.0, Color32::from_rgb(210, 212, 216));
    }
    visuals.selection.bg_fill = theme_accent(dark_mode).gamma_multiply(0.45);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
    ctx.set_style(style);
}

fn apply_window_attributes(
    frame: &eframe::Frame,
    opacity: f32,
    force_opacity: bool,
    applied_opacity: &mut Option<f32>,
) {
    let opacity = opacity.clamp(0.35, 1.0);
    let Ok(window_handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(window_handle) = window_handle.as_raw() else {
        return;
    };
    let hwnd = window_handle.hwnd.get() as _;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            std::ptr::from_ref(&DWMWCP_ROUND).cast(),
            std::mem::size_of_val(&DWMWCP_ROUND) as u32,
        );
        if force_opacity
            || !applied_opacity.is_some_and(|current| (current - opacity).abs() < f32::EPSILON)
        {
            let extended_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, extended_style | WS_EX_LAYERED as isize);
            SetLayeredWindowAttributes(hwnd, 0, (opacity * 255.0).round() as u8, LWA_ALPHA);
            *applied_opacity = Some(opacity);
        }
    }
}

fn apply_rounding_to_process_windows() {
    unsafe extern "system" fn apply_rounding(hwnd: HWND, process_id: LPARAM) -> i32 {
        let mut window_process_id = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut window_process_id);
        }
        if window_process_id != process_id as u32 {
            return 1;
        }
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                std::ptr::from_ref(&DWMWCP_ROUND).cast(),
                std::mem::size_of_val(&DWMWCP_ROUND) as u32,
            );
        }
        1
    }

    unsafe {
        EnumWindows(Some(apply_rounding), std::process::id() as LPARAM);
    }
}

#[derive(Clone, Copy)]
struct CharacterHitLayout {
    row_width: f32,
    time_x: f32,
    type_x: f32,
    damage_x: f32,
    hp_x: f32,
    separators: [f32; 3],
}

impl CharacterHitLayout {
    fn new(available_width: f32) -> Self {
        const LEFT_INSET: f32 = 4.0;
        const TIME_WIDTH: f32 = 92.0;
        const TYPE_WIDTH: f32 = 84.0;
        const DAMAGE_WIDTH: f32 = 150.0;
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
            damage_x,
            hp_x,
            separators: [type_separator, damage_separator, hp_separator],
        }
    }
}

fn draw_character_hit_header(ui: &mut egui::Ui, layout: CharacterHitLayout) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(layout.row_width, 20.0), egui::Sense::hover());
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter();
    let font = egui::FontId::proportional(11.0);
    let color = ui.visuals().weak_text_color();
    draw_hit_column_separators(painter, rect, layout);

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
        "角色/目标 HP",
        font,
        color,
    );
}

fn draw_character_hit_row(
    ui: &mut egui::Ui,
    layout: CharacterHitLayout,
    hit: &crate::model::Hit,
    max_damage: f64,
) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(layout.row_width, 30.0), egui::Sense::hover());
    let incoming = hit.direction == "incoming";
    let type_color = if incoming {
        Color32::from_rgb(211, 79, 79)
    } else {
        theme_accent(ui.visuals().dark_mode)
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
    let damage_fraction = (hit.damage / max_damage).clamp(0.0, 1.0) as f32;
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
    let painter = ui.painter();
    let text_color = ui.visuals().text_color();
    let damage_color = type_color;
    let mono = egui::FontId::monospace(12.0);
    draw_hit_column_separators(painter, rect, layout);
    let hit_type = if incoming { "受击" } else { "输出" };
    let time = format_short_time(hit.timestamp);
    let damage = format_number(hit.damage);
    let target_hp = format!(
        "{} / {}  {:.1}%",
        format_number(hit.target_hp_after),
        format_number(hit.target_max_hp),
        hit.target_hp_percent
    );

    painter.text(
        egui::pos2(x + layout.time_x, y),
        egui::Align2::LEFT_CENTER,
        &time,
        mono.clone(),
        text_color,
    );
    painter.rect_filled(
        egui::Rect::from_center_size(
            egui::pos2(x + layout.type_x + 26.0, y),
            egui::vec2(48.0, 20.0),
        ),
        10.0,
        type_color,
    );
    painter.text(
        egui::pos2(x + layout.type_x + 26.0, y),
        egui::Align2::CENTER_CENTER,
        hit_type,
        egui::FontId::proportional(11.0),
        Color32::WHITE,
    );
    painter.text(
        egui::pos2(x + layout.damage_x, y),
        egui::Align2::LEFT_CENTER,
        &damage,
        egui::FontId::monospace(14.0),
        damage_color,
    );
    let hp_fraction = (hit.target_hp_percent / 100.0).clamp(0.0, 1.0) as f32;
    let hp_bar_left = x + layout.hp_x;
    let hp_bar_right = (rect.right() - 10.0).min(ui.clip_rect().right() - 10.0);
    let hp_bar_rect = egui::Rect::from_min_max(
        egui::pos2(hp_bar_left, rect.bottom() - 7.0),
        egui::pos2(hp_bar_right.max(hp_bar_left), rect.bottom() - 4.0),
    );
    painter.rect_filled(hp_bar_rect, 1.5, ui.visuals().faint_bg_color);
    painter.rect_filled(
        egui::Rect::from_min_size(
            hp_bar_rect.min,
            egui::vec2(hp_bar_rect.width() * hp_fraction, hp_bar_rect.height()),
        ),
        1.5,
        if hp_fraction > 0.5 {
            Color32::from_rgb(75, 168, 105)
        } else if hp_fraction > 0.2 {
            Color32::from_rgb(218, 154, 55)
        } else {
            Color32::from_rgb(211, 79, 79)
        },
    );
    painter.text(
        egui::pos2(x + layout.hp_x, y - 3.0),
        egui::Align2::LEFT_CENTER,
        &target_hp,
        mono.clone(),
        text_color,
    );
    if let Some(target_name) = hit.target_name.as_deref() {
        painter.text(
            egui::pos2(rect.right() - 10.0, y - 3.0),
            egui::Align2::RIGHT_CENTER,
            target_name,
            egui::FontId::proportional(10.0),
            ui.visuals().weak_text_color(),
        );
    }
    if response.hovered() {
        response.on_hover_text(if incoming {
            "角色受到的伤害"
        } else {
            "角色造成的伤害"
        });
    }
}

fn hit_metric_card(ui: &mut egui::Ui, label: &str, value: String, color: Color32) {
    egui::Frame::new()
        .fill(if ui.visuals().dark_mode {
            Color32::from_rgba_unmultiplied(255, 255, 255, 8)
        } else {
            Color32::WHITE
        })
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_min_width(92.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(value)
                        .monospace()
                        .size(15.0)
                        .strong()
                        .color(color),
                );
                ui.label(
                    RichText::new(label)
                        .size(10.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });
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

fn default_export_filename() -> String {
    format!("nte_capture_{}.json", Local::now().format("%Y%m%d_%H%M%S"))
}

fn default_raw_capture_path() -> PathBuf {
    data_root().join("logs").join(format!(
        "nte_raw_{}.pcapng",
        Local::now().format("%Y%m%d_%H%M%S_%3f")
    ))
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

fn json_string_field(row: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn set_json_string(row: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: &str) {
    row.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
}

fn set_optional_json_string(
    row: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &str,
) {
    if value.is_empty() {
        row.remove(key);
    } else {
        set_json_string(row, key, value);
    }
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

fn write_abyss_half_json(
    out: &mut String,
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
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(value)
                .size(if prominent { 18.0 } else { 15.0 })
                .strong()
                .color(color),
        );
        ui.label(
            RichText::new(label)
                .size(10.0)
                .color(ui.visuals().weak_text_color()),
        );
    });
}

fn theme_accent(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(235, 188, 95)
    } else {
        Color32::from_rgb(166, 105, 25)
    }
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

fn compare_hit_display_order(
    left: &crate::model::Hit,
    right: &crate::model::Hit,
) -> std::cmp::Ordering {
    let second_order = (left.timestamp.floor() as i64).cmp(&(right.timestamp.floor() as i64));
    let same_health_pool = left.direction == "outgoing"
        && right.direction == "outgoing"
        && left.target_max_hp > 0.0
        && right.target_max_hp > 0.0
        && (left.target_max_hp - right.target_max_hp).abs()
            <= left.target_max_hp.max(right.target_max_hp) * 0.05;
    second_order
        .then_with(|| {
            if same_health_pool {
                right.target_hp_after.total_cmp(&left.target_hp_after)
            } else {
                left.timestamp.total_cmp(&right.timestamp)
            }
        })
        .then_with(|| left.byte_offset.cmp(&right.byte_offset))
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
    if PathBuf::from("characters.json").is_file() {
        return PathBuf::from(".");
    }
    std::env::current_exe()
        .ok()
        .into_iter()
        .flat_map(|path| path.ancestors().map(PathBuf::from).collect::<Vec<_>>())
        .find(|path| path.join("characters.json").is_file())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod avatar_tests {
    use super::*;

    #[test]
    fn character_editor_preserves_unknown_fields_and_adds_rows() {
        let mut editor = CharacterEditorState {
            document: serde_json::json!({
                "version": 2,
                "description": "keep me",
                "characters": {
                    "1010": {
                        "name_zh": "旧名称",
                        "name_en": "Nanally",
                        "codename": "Nanally",
                        "verified": true,
                        "custom_field": 42
                    }
                }
            }),
            selected_id: None,
            form: CharacterEditForm::default(),
            search: String::new(),
            new_id: "1080".to_owned(),
            dirty: false,
            message: String::new(),
            cancel_selection: None,
        };

        editor.start_new().unwrap();
        editor.form.name_zh = "测试角色".to_owned();
        editor.form.name_en = "Test Character".to_owned();
        editor.form.codename = "Test".to_owned();
        editor.form.color = "#123456".to_owned();
        assert_eq!(editor.apply_form().unwrap(), "1080");

        assert_eq!(editor.document["description"], "keep me");
        assert_eq!(editor.document["characters"]["1010"]["custom_field"], 42);
        assert_eq!(editor.document["characters"]["1080"]["name_zh"], "测试角色");
        assert_eq!(editor.document["characters"]["1080"]["color"], "#123456");
    }

    #[test]
    fn character_editor_rejects_invalid_color() {
        let mut editor = CharacterEditorState {
            document: serde_json::json!({"characters": {}}),
            selected_id: None,
            form: CharacterEditForm {
                id: "1080".to_owned(),
                name_zh: "测试".to_owned(),
                color: "red".to_owned(),
                ..Default::default()
            },
            search: String::new(),
            new_id: String::new(),
            dirty: true,
            message: String::new(),
            cancel_selection: None,
        };

        assert_eq!(editor.apply_form().unwrap_err(), "颜色必须是 #RRGGBB 格式");
    }

    #[test]
    fn character_editor_cancels_new_row_and_returns_to_previous_selection() {
        let mut editor = CharacterEditorState {
            document: serde_json::json!({
                "characters": {
                    "1010": {
                        "name_zh": "娜娜莉",
                        "name_en": "Nanally"
                    }
                }
            }),
            selected_id: None,
            form: CharacterEditForm::default(),
            search: String::new(),
            new_id: String::new(),
            dirty: false,
            message: String::new(),
            cancel_selection: None,
        };

        editor.select("1010");
        editor.new_id = "1080".to_owned();
        editor.start_new().unwrap();
        editor.form.name_zh = "未保存角色".to_owned();
        editor.cancel_edit();

        assert_eq!(editor.selected_id.as_deref(), Some("1010"));
        assert_eq!(editor.form.name_zh, "娜娜莉");
        assert!(!editor.dirty);
        assert!(editor.document["characters"].get("1080").is_none());
    }

    #[test]
    fn character_editor_cancels_changes_to_existing_row() {
        let mut editor = CharacterEditorState {
            document: serde_json::json!({
                "characters": {
                    "1010": {
                        "name_zh": "娜娜莉",
                        "name_en": "Nanally"
                    }
                }
            }),
            selected_id: None,
            form: CharacterEditForm::default(),
            search: String::new(),
            new_id: String::new(),
            dirty: false,
            message: String::new(),
            cancel_selection: None,
        };

        editor.select("1010");
        editor.form.name_zh = "临时修改".to_owned();
        editor.dirty = true;
        editor.cancel_edit();

        assert_eq!(editor.form.name_zh, "娜娜莉");
        assert!(!editor.dirty);
    }

    #[test]
    fn loads_avatar_from_character_json_relative_path() {
        let root = std::env::temp_dir().join(format!("nte-avatar-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let avatar_name = "Character_Test.png";
        let avatar_path = root.join(avatar_name);
        let pixels = vec![255_u8; 200 * 200 * 4];
        image::save_buffer(&avatar_path, &pixels, 200, 200, image::ColorType::Rgba8).unwrap();

        let characters = HashMap::from([(
            1,
            CharacterInfo {
                name_zh: "测试".to_owned(),
                name_en: "Test".to_owned(),
                color: None,
                avatar: Some(avatar_name.to_owned()),
            },
        )]);
        let context = egui::Context::default();
        let textures = load_character_avatars(&context, &root, &characters);
        assert_eq!(textures.get(avatar_name).unwrap().size(), [200, 200]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn aligns_avatar_rect_to_physical_pixels() {
        let rect = pixel_aligned_rect(egui::pos2(10.3, 20.4), 40.0, 1.5);

        assert_eq!(rect.width() * 1.5, 60.0);
        assert_eq!((rect.left() * 1.5).fract(), 0.0);
        assert_eq!((rect.top() * 1.5).fract(), 0.0);
    }

    #[test]
    fn sorts_zero_health_after_positive_health_in_the_same_second() {
        let hit = |timestamp: f64, hp_after: f64, offset: usize| crate::model::Hit {
            timestamp,
            char_id: 1,
            char_name: "Test".to_owned(),
            char_known: true,
            damage: 1.0,
            byte_offset: offset,
            bit_shift: 0,
            char_source: "test".to_owned(),
            direction: "outgoing".to_owned(),
            target_hp_before: hp_after + 1.0,
            target_hp_after: hp_after,
            target_max_hp: 1_356_337.0,
            target_hp_percent: hp_after / 1_356_337.0 * 100.0,
            target_id: None,
            target_name: None,
            target_context: Vec::new(),
        };
        let mut hits = [hit(10.1, 0.0, 1), hit(10.8, 1_524.0, 2)];

        hits.sort_by(compare_hit_display_order);

        assert_eq!(hits[0].target_hp_after, 1_524.0);
        assert_eq!(hits[1].target_hp_after, 0.0);
    }
}
