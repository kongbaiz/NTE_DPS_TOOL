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
    CaptureDevice, CaptureHandle, import_capture_json, import_pcapng, list_devices, replay_hits,
    start_capture,
};
use crate::hotkey::{HotkeyEvent, HotkeyHandle};
use crate::model::{CharacterInfo, CombatState, EngineEvent};
use crate::network::{GameNetwork, detect_game_device};
use crate::parser::load_characters;

#[derive(Clone, Copy)]
enum DebugImportKind {
    Pcapng,
    CaptureJson,
}

const AVATAR_DISPLAY_SIZE: f32 = 40.0;

pub struct DpsApp {
    characters: Arc<HashMap<u32, CharacterInfo>>,
    avatar_textures: HashMap<String, egui::TextureHandle>,
    state: CombatState,
    hit_detail_char_id: Option<u32>,
    devices: Vec<CaptureDevice>,
    selected_device: usize,
    local_ip: String,
    game_network: Option<GameNetwork>,
    filter: String,
    include_incoming: bool,
    capture: Option<CaptureHandle>,
    replay_stop: Option<Arc<AtomicBool>>,
    replay_thread: Option<thread::JoinHandle<()>>,
    sender: Sender<EngineEvent>,
    receiver: Receiver<EngineEvent>,
    status: String,
    diagnostic: Option<String>,
    last_error: Option<String>,
    debug_open: bool,
    debug_only_hits: bool,
    debug_search: String,
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
        let (hotkey, hotkey_receiver) = HotkeyHandle::start();
        let (sender, receiver) = unbounded();
        let data_root = data_root();
        let characters =
            load_characters(data_root.join("characters.json").as_path()).unwrap_or_default();
        let avatar_textures = load_character_avatars(&cc.egui_ctx, &data_root, &characters);
        let (devices, device_error) = match list_devices() {
            Ok(devices) => (devices, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let (selected_device, game_network, status, diagnostic) = match device_error {
            Some(error) => (
                0,
                None,
                "采集环境不可用，请查看 Debug".to_owned(),
                Some(error),
            ),
            None => match detect_game_device(&devices) {
                Ok((index, network)) => (index, Some(network), "已就绪".to_owned(), None),
                Err(error) => (
                    0,
                    None,
                    "未检测到游戏，请查看 Debug".to_owned(),
                    Some(error),
                ),
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
            hit_detail_char_id: None,
            devices,
            selected_device,
            local_ip,
            game_network,
            filter: "udp".to_owned(),
            include_incoming: false,
            capture: None,
            replay_stop: None,
            replay_thread: None,
            sender,
            receiver,
            status,
            diagnostic,
            last_error: None,
            debug_open: false,
            debug_only_hits: false,
            debug_search: String::new(),
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
        while let Ok(event) = self.hotkey_receiver.try_recv() {
            match event {
                HotkeyEvent::TogglePassthrough => {
                    self.toggle_mouse_passthrough(ctx);
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
            if ui.small_button("×").on_hover_text("关闭").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if ui.small_button("−").on_hover_text("最小化").clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
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
        self.capture = Some(start_capture(
            device,
            local_ip,
            self.filter.clone(),
            self.include_incoming,
            self.characters.clone(),
            self.sender.clone(),
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

    fn start_replay(&mut self, path: PathBuf) {
        self.stop_engine();
        self.state.clear();
        self.hit_detail_char_id = None;
        let stop = Arc::new(AtomicBool::new(false));
        self.replay_thread = Some(replay_hits(path, self.sender.clone(), stop.clone()));
        self.replay_stop = Some(stop);
        self.status = "正在回放命中日志...".to_owned();
    }

    fn start_pcapng_import(&mut self, path: PathBuf) {
        self.stop_engine();
        self.state.clear();
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
                EngineEvent::Status(status) => self.status = status,
                EngineEvent::Error(error) => {
                    self.status = "运行失败".to_owned();
                    self.last_error = Some(error);
                }
                EngineEvent::CaptureStopped => {
                    self.capture = None;
                    self.replay_stop = None;
                    if let Some(thread) = self.replay_thread.take() {
                        let _ = thread.join();
                    }
                    self.status = "已停止".to_owned();
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
                self.status = format!("已导出抓包信息：{}", path.display());
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("导出抓包信息失败：{error}"));
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

    fn summary_bar(&mut self, ui: &mut egui::Ui) {
        let hits = self.state.hits.len();
        let duration = self.state.duration();
        ui.columns(4, |columns| {
            compact_metric(
                &mut columns[0],
                "DPS",
                format_number(self.state.dps()),
                theme_accent(self.dark_mode),
                true,
            );
            let total_color = columns[1].visuals().text_color();
            compact_metric(
                &mut columns[1],
                "总伤害",
                format_number(self.state.total_damage),
                total_color,
                true,
            );
            let hit_color = columns[2].visuals().text_color();
            compact_metric(
                &mut columns[2],
                "命中",
                format_number(hits as f64),
                hit_color,
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
                self.stop_engine();
                self.drain_pending_events();
                self.status = "已停止".to_owned();
            }
            if ui.button("重置").clicked() {
                self.state.clear();
                self.hit_detail_char_id = None;
            }
            if ui
                .button(if self.paused { "继续" } else { "暂停" })
                .clicked()
            {
                self.paused = !self.paused;
            }
            ui.toggle_value(&mut self.debug_open, "Debug");
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
        let mut rows: Vec<_> = self.state.stats.values().cloned().collect();
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
                    let share = if self.state.total_damage > 0.0 {
                        row.damage / self.state.total_damage * 100.0
                    } else {
                        0.0
                    };
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 52.0),
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
                    let avatar_rect = pixel_aligned_rect(
                        egui::pos2(
                            rect.left() + 20.0,
                            rect.center().y - AVATAR_DISPLAY_SIZE * 0.5,
                        ),
                        AVATAR_DISPLAY_SIZE,
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
                        format!("{} · {share:.1}%", format_number(row.damage)),
                        egui::FontId::monospace(10.5),
                        ui.visuals().weak_text_color(),
                    );
                    if response.on_hover_text("在独立窗口查看完整命中").clicked() {
                        self.hit_detail_char_id = Some(row.char_id);
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

    fn character_hits(&self, ui: &mut egui::Ui, char_id: u32) {
        let layout = CharacterHitLayout::new(ui.available_width());
        draw_character_hit_header(ui, layout);
        let hit_count = self
            .state
            .stats
            .get(&char_id)
            .map_or(0, |stats| stats.hits as usize);
        egui::ScrollArea::vertical()
            .id_salt(("character_hits", char_id))
            .max_height(ui.available_height())
            .stick_to_bottom(true)
            .show_rows(ui, 22.0, hit_count, |ui, visible_rows| {
                let visible_count = visible_rows.end.saturating_sub(visible_rows.start);
                for hit in self
                    .state
                    .hits
                    .iter()
                    .filter(|hit| hit.char_id == char_id)
                    .skip(visible_rows.start)
                    .take(visible_count)
                {
                    draw_character_hit_row(ui, layout, hit);
                }
            });
    }

    fn hit_detail_panel(&mut self, ctx: &egui::Context, char_id: u32) {
        let Some(stats) = self.state.stats.get(&char_id).cloned() else {
            self.hit_detail_char_id = None;
            return;
        };
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
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&stats.name).size(18.0).strong());
                        ui.separator();
                        ui.monospace(format!("{} 次命中", stats.hits));
                        ui.separator();
                        ui.monospace(format!("{:.1}s", stats.duration()));
                        ui.separator();
                        ui.label(
                            RichText::new(format!("{} DPS", format_number(stats.dps())))
                                .color(theme_accent(self.dark_mode))
                                .strong(),
                        );
                        ui.separator();
                        ui.monospace(format!("总伤害 {}", format_number(stats.damage)));
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    self.character_hits(ui, char_id);
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
                    });
                ui.horizontal(|ui| {
                    if ui.button("重新检测").clicked()
                        && let Err(error) = self.refresh_game_network()
                    {
                        self.last_error = Some(error);
                    }
                    ui.checkbox(&mut self.include_incoming, "伤害统计包含受击");
                    let can_export = self.capture.is_none()
                        && self.replay_thread.is_none()
                        && (!self.state.hits.is_empty() || !self.state.packets.is_empty());
                    if ui
                        .add_enabled(can_export, egui::Button::new("导出完整抓包 JSON"))
                        .clicked()
                    {
                        self.export_capture_info();
                    }
                    if ui.button("回放 JSONL").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("NTE 命中日志", &["jsonl"])
                            .pick_file()
                    {
                        self.start_replay(path);
                    }
                    if ui.button("回放最新日志").clicked() {
                        match latest_hit_log() {
                            Some(path) => self.start_replay(path),
                            None => {
                                self.last_error = Some("logs 目录中没有命中 JSONL 日志".to_owned());
                            }
                        }
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
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for packet in self.state.packets.iter().rev().take(500).rev() {
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
                    egui::CollapsingHeader::new(format!(
                        "{}  {}  {} -> {}  {} B  ids={:?}  hits={}",
                        format_time(packet.timestamp),
                        packet.direction,
                        packet.source,
                        packet.destination,
                        packet.payload_len,
                        packet.declared_ids,
                        packet.parsed_hits
                    ))
                    .show(ui, |ui| {
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
            self.controls(ui);
            ui.separator();
            self.summary_bar(ui);
            ui.add_space(2.0);
            ui.label(RichText::new("队伍").size(12.0).strong());
            self.party_panel(ui);
        });

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
            if ui.small_button("×").on_hover_text("关闭").clicked() {
                close_clicked = true;
            }
            if ui.small_button("−").on_hover_text("最小化").clicked() {
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
        let target_pixels = (AVATAR_DISPLAY_SIZE * ctx.pixels_per_point())
            .round()
            .max(32.0) as u32;
        let image = image
            .resize_exact(
                target_pixels,
                target_pixels,
                image::imageops::FilterType::Lanczos3,
            )
            .unsharpen(0.6, 1)
            .to_rgba8();
        let size = [image.width() as usize, image.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
        let texture = ctx.load_texture(
            format!("character-avatar:{avatar}"),
            color_image,
            egui::TextureOptions::NEAREST,
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
    target_x: f32,
    damage_x: f32,
    hp_x: f32,
    separators: [f32; 3],
}

impl CharacterHitLayout {
    fn new(available_width: f32) -> Self {
        const LEFT_INSET: f32 = 4.0;
        const TIME_WIDTH: f32 = 104.0;
        const TARGET_WIDTH: f32 = 190.0;
        const DAMAGE_WIDTH: f32 = 120.0;
        const CELL_PADDING: f32 = 10.0;

        let time_x = LEFT_INSET + CELL_PADDING;
        let target_separator = LEFT_INSET + TIME_WIDTH;
        let target_x = target_separator + CELL_PADDING;
        let damage_separator = target_separator + TARGET_WIDTH;
        let damage_x = damage_separator + CELL_PADDING;
        let hp_separator = damage_separator + DAMAGE_WIDTH;
        let hp_x = hp_separator + CELL_PADDING;

        Self {
            row_width: available_width,
            time_x,
            target_x,
            damage_x,
            hp_x,
            separators: [target_separator, damage_separator, hp_separator],
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
        egui::pos2(x + layout.target_x, y),
        egui::Align2::LEFT_CENTER,
        "目标",
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
        "当前HP / 最大HP",
        font,
        color,
    );
}

fn draw_character_hit_row(ui: &mut egui::Ui, layout: CharacterHitLayout, hit: &crate::model::Hit) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(layout.row_width, 22.0), egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        3.0,
        if ui.visuals().dark_mode {
            Color32::from_rgba_unmultiplied(255, 255, 255, 8)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 5)
        },
    );
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter();
    let text_color = ui.visuals().text_color();
    let damage_color = theme_accent(ui.visuals().dark_mode);
    let mono = egui::FontId::monospace(12.0);
    draw_hit_column_separators(painter, rect, layout);
    let target = hit
        .target_name
        .as_deref()
        .or(hit.target_id.as_deref())
        .map(|value| compact_label(value, 18))
        .unwrap_or_default();
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
    painter.text(
        egui::pos2(x + layout.target_x, y),
        egui::Align2::LEFT_CENTER,
        &target,
        mono.clone(),
        text_color,
    );
    painter.text(
        egui::pos2(x + layout.damage_x, y),
        egui::Align2::LEFT_CENTER,
        &damage,
        mono.clone(),
        damage_color,
    );
    painter.text(
        egui::pos2(x + layout.hp_x, y),
        egui::Align2::LEFT_CENTER,
        &target_hp,
        mono,
        text_color,
    );
    if response.hovered()
        && (hit.target_id.is_some() || hit.target_name.is_some() || !hit.target_context.is_empty())
    {
        let target_details = [
            hit.target_id
                .as_ref()
                .map(|value| format!("目标 ID: {value}")),
            hit.target_name
                .as_ref()
                .map(|value| format!("目标名称: {value}")),
            (!hit.target_context.is_empty())
                .then(|| format!("封包上下文: {}", hit.target_context.join(" | "))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
        response.on_hover_text(target_details);
    }
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

fn compact_label(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut result: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    result.push('…');
    result
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

fn latest_hit_log() -> Option<PathBuf> {
    std::fs::read_dir(data_root().join("logs"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("nte_hits_") && name.ends_with(".jsonl"))
        })
        .max()
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
        let expected_size = (AVATAR_DISPLAY_SIZE * context.pixels_per_point())
            .round()
            .max(32.0) as usize;

        assert_eq!(
            textures.get(avatar_name).unwrap().size(),
            [expected_size, expected_size]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn aligns_avatar_rect_to_physical_pixels() {
        let rect = pixel_aligned_rect(egui::pos2(10.3, 20.4), 40.0, 1.5);

        assert_eq!(rect.width() * 1.5, 60.0);
        assert_eq!((rect.left() * 1.5).fract(), 0.0);
        assert_eq!((rect.top() * 1.5).fract(), 0.0);
    }
}
