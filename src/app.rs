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

use crate::capture::{CaptureDevice, CaptureHandle, list_devices, replay_hits, start_capture};
use crate::model::{CharacterInfo, CombatState, EngineEvent};
use crate::network::{GameNetwork, detect_game_device};
use crate::parser::load_characters;

pub struct DpsApp {
    characters: Arc<HashMap<u32, CharacterInfo>>,
    state: CombatState,
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
    last_error: Option<String>,
    debug_open: bool,
    debug_only_hits: bool,
    debug_search: String,
    paused: bool,
}

impl DpsApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);
        let (sender, receiver) = unbounded();
        let characters =
            load_characters(data_root().join("characters.json").as_path()).unwrap_or_default();
        let (devices, device_error) = match list_devices() {
            Ok(devices) => (devices, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let (selected_device, game_network, status) = match device_error {
            Some(error) => (0, None, error),
            None => match detect_game_device(&devices) {
                Ok((index, network)) => {
                    let status = format!(
                        "已根据 HTGame.exe (PID {}) 自动选择网卡，游戏连接 {} -> {}:{}",
                        network.pid, network.local_ip, network.remote_ip, network.remote_port
                    );
                    (index, Some(network), status)
                }
                Err(error) => (0, None, error),
            },
        };
        let local_ip = game_network
            .as_ref()
            .map(|network| network.local_ip.to_string())
            .unwrap_or_default();
        Self {
            characters: Arc::new(characters),
            state: CombatState::default(),
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
            last_error: None,
            debug_open: true,
            debug_only_hits: false,
            debug_search: String::new(),
            paused: false,
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
        self.devices = list_devices()?;
        let (index, network) = detect_game_device(&self.devices)?;
        self.selected_device = index;
        self.local_ip = network.local_ip.to_string();
        self.status = format!(
            "HTGame.exe (PID {})：{} -> {}:{}",
            network.pid, network.local_ip, network.remote_ip, network.remote_port
        );
        self.game_network = Some(network);
        Ok(())
    }

    fn start_replay(&mut self, path: PathBuf) {
        self.stop_engine();
        self.state.clear();
        let stop = Arc::new(AtomicBool::new(false));
        self.replay_thread = Some(replay_hits(path, self.sender.clone(), stop.clone()));
        self.replay_stop = Some(stop);
        self.status = "正在回放命中日志...".to_owned();
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
            writeln!(
                &mut out,
                "      \"dps\": {},",
                json_f64(row.damage / duration)
            )
            .ok();
            writeln!(
                &mut out,
                "      \"share_percent\": {}",
                json_f64(share)
            )
            .ok();
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
                "      \"target_hp_percent\": {}",
                json_f64(hit.target_hp_percent)
            )
            .ok();
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
                "      \"payload_len\": {},",
                packet.payload_len
            )
            .ok();
            writeln!(
                &mut out,
                "      \"declared_ids\": {},",
                json_string(&format!("{:?}", packet.declared_ids))
            )
            .ok();
            writeln!(
                &mut out,
                "      \"parsed_hits\": {},",
                packet.parsed_hits
            )
            .ok();
            writeln!(&mut out, "      \"note\": {},", json_string(&packet.note)).ok();
            writeln!(
                &mut out,
                "      \"payload_preview\": {}",
                json_string(&packet.payload_preview)
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
        ui.horizontal(|ui| {
            metric(
                ui,
                "命中",
                format_number(hits as f64),
                Color32::from_rgb(215, 187, 111),
            );
            metric(
                ui,
                "总伤害",
                format_number(self.state.total_damage),
                Color32::from_rgb(205, 210, 220),
            );
            metric(
                ui,
                "DPS",
                format_number(self.state.dps()),
                Color32::from_rgb(235, 188, 95),
            );
            metric(
                ui,
                "战斗时间",
                format!("{duration:.1}s"),
                Color32::from_gray(180),
            );
            ui.separator();
            if ui.button("重置").clicked() {
                self.state.clear();
            }
            if ui
                .button(if self.paused {
                    "继续刷新"
                } else {
                    "暂停刷新"
                })
                .clicked()
            {
                self.paused = !self.paused;
            }
            ui.toggle_value(&mut self.debug_open, "Debug");
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("游戏网卡");
            let device_text = self
                .devices
                .get(self.selected_device)
                .map(|device| {
                    if device.description.is_empty() {
                        device.name.clone()
                    } else {
                        device.description.clone()
                    }
                })
                .unwrap_or_else(|| "未检测到".to_owned());
            ui.label(RichText::new(device_text).strong());
            ui.separator();
            ui.label("游戏 IP");
            ui.monospace(if self.local_ip.is_empty() {
                "未检测到"
            } else {
                &self.local_ip
            });
            if let Some(network) = &self.game_network {
                ui.label(format!("PID {}", network.pid));
            }
            if ui.button("重新检测").clicked()
                && let Err(error) = self.refresh_game_network()
            {
                self.last_error = Some(error);
            }
            ui.label("BPF");
            ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(100.0));
            ui.checkbox(&mut self.include_incoming, "包含受击");
            if self.capture.is_none() && self.replay_thread.is_none() {
                if ui.button(RichText::new("开始抓包").strong()).clicked() {
                    self.start_live();
                }
            } else if ui.button("停止").clicked() {
                self.stop_engine();
                self.drain_pending_events();
                self.status = "已停止".to_owned();
            }
            let can_export = self.capture.is_none()
                && self.replay_thread.is_none()
                && (!self.state.hits.is_empty() || !self.state.packets.is_empty());
            if ui
                .add_enabled(can_export, egui::Button::new("导出本次抓包"))
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
                    None => self.last_error = Some("logs 目录中没有命中 JSONL 日志".to_owned()),
                }
            }
        });
        ui.small(RichText::new(&self.status).color(Color32::from_gray(155)));
    }

    fn party_panel(&self, ui: &mut egui::Ui) {
        let duration = self.state.duration().max(0.001);
        let mut rows: Vec<_> = self.state.stats.values().collect();
        rows.sort_by(|left, right| right.damage.total_cmp(&left.damage));
        let max_damage = rows.first().map_or(1.0, |row| row.damage.max(1.0));
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, row) in rows.iter().enumerate() {
                let color = character_color(row.char_id, &self.characters, index);
                let fraction = (row.damage / max_damage) as f32;
                let share = if self.state.total_damage > 0.0 {
                    row.damage / self.state.total_damage * 100.0
                } else {
                    0.0
                };
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 58.0),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .rect_filled(rect, 10.0, Color32::from_rgb(37, 39, 47));
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(rect.width() * fraction, rect.height()),
                    ),
                    10.0,
                    color.gamma_multiply(0.45),
                );
                let avatar = egui::Rect::from_center_size(
                    egui::pos2(rect.left() + 27.0, rect.center().y),
                    egui::vec2(38.0, 38.0),
                );
                ui.painter().circle_filled(avatar.center(), 19.0, color);
                let initial = row.name.chars().next().unwrap_or('?').to_string();
                ui.painter().text(
                    avatar.center(),
                    egui::Align2::CENTER_CENTER,
                    initial,
                    egui::FontId::proportional(18.0),
                    Color32::WHITE,
                );
                ui.painter().text(
                    egui::pos2(rect.left() + 55.0, rect.top() + 12.0),
                    egui::Align2::LEFT_TOP,
                    &row.name,
                    egui::FontId::proportional(16.0),
                    Color32::WHITE,
                );
                ui.painter().text(
                    egui::pos2(rect.left() + 55.0, rect.bottom() - 11.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!(
                        "{} hits  ·  {} DPS",
                        row.hits,
                        format_number(row.damage / duration)
                    ),
                    egui::FontId::proportional(12.0),
                    Color32::from_gray(175),
                );
                ui.painter().text(
                    egui::pos2(rect.right() - 12.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{}  {share:.1}%", format_number(row.damage)),
                    egui::FontId::proportional(15.0),
                    Color32::from_rgb(238, 205, 120),
                );
                ui.add_space(7.0);
            }
            if rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("等待伤害数据").color(Color32::from_gray(130)));
                });
            }
        });
    }

    fn hit_table(&self, ui: &mut egui::Ui) {
        let layout = HitTableLayout::new(ui.available_width());
        draw_hit_table_header(ui, layout);
        ui.separator();

        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let layout = HitTableLayout::new(ui.available_width());
                for hit in self.state.hits.iter().rev().take(300).rev() {
                    draw_hit_table_row(
                        ui,
                        layout,
                        &format_time(hit.timestamp),
                        hit.char_name.as_str(),
                        &format_number(hit.damage),
                        &format!(
                            "{}/{} ({:.1}%)",
                            format_number(hit.target_hp_after),
                            format_number(hit.target_max_hp),
                            hit.target_hp_percent
                        ),
                    );
                }
            });
    }

    fn debug_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.debug_open;
        egui::Window::new("实时 Debug 面板")
            .open(&mut open)
            .default_size([780.0, 380.0])
            .resizable(true)
            .vscroll(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.debug_only_hits, "仅显示命中包");
                    ui.label("搜索");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.debug_search)
                            .desired_width(180.0)
                            .hint_text("IP / ID / hex"),
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
                                "{} {} {:?} {}",
                                packet.source,
                                packet.destination,
                                packet.declared_ids,
                                packet.payload_preview
                            );
                            if !self.debug_search.is_empty()
                                && !searchable
                                    .to_lowercase()
                                    .contains(&self.debug_search.to_lowercase())
                            {
                                continue;
                            }
                            egui::CollapsingHeader::new(format!(
                                "{}  {} -> {}  {} B  ids={:?}  hits={}",
                                format_time(packet.timestamp),
                                packet.source,
                                packet.destination,
                                packet.payload_len,
                                packet.declared_ids,
                                packet.parsed_hits
                            ))
                            .show(ui, |ui| {
                                if !packet.note.is_empty() {
                                    ui.label(
                                        RichText::new(&packet.note)
                                            .color(Color32::from_rgb(235, 188, 95)),
                                    );
                                }
                                ui.add(
                                    egui::TextEdit::multiline(&mut packet.payload_preview.clone())
                                        .font(egui::TextStyle::Monospace)
                                        .desired_rows(3)
                                        .interactive(false),
                                );
                            });
                        }
                    });
            });
        self.debug_open = open;
    }
}

impl eframe::App for DpsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        configure_style(ctx);
        self.drain_events();
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::TopBottomPanel::top("summary").show(ctx, |ui| {
            ui.add_space(5.0);
            self.summary_bar(ui);
            ui.separator();
            self.controls(ui);
            ui.add_space(3.0);
        });

        egui::SidePanel::left("party")
            .resizable(true)
            .default_width(390.0)
            .min_width(310.0)
            .show(ctx, |ui| {
                ui.heading("当前队伍");
                ui.separator();
                self.party_panel(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("实时命中");
            ui.separator();
            self.hit_table(ui);
        });

        if self.debug_open {
            self.debug_panel(ctx);
        }
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
    let candidates = [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    let Some((path, bytes)) = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(|bytes| (*path, bytes)))
    else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "chinese".to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "chinese".to_owned());
    }
    let _ = path;
    ctx.set_fonts(fonts);
}

fn configure_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(25, 27, 33);
    visuals.window_fill = Color32::from_rgb(29, 31, 38);
    visuals.extreme_bg_color = Color32::from_rgb(20, 22, 27);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(48));
    ctx.set_visuals(visuals);
}


#[derive(Clone, Copy)]
struct HitTableLayout {
    row_width: f32,
    time_x: f32,
    role_x: f32,
    damage_right_x: f32,
    target_x: f32,
}

impl HitTableLayout {
    fn new(available_width: f32) -> Self {
        const TIME_WIDTH: f32 = 96.0;
        const ROLE_WIDTH: f32 = 90.0;
        const DAMAGE_WIDTH: f32 = 96.0;
        const GAP: f32 = 18.0;
        const MIN_TARGET_WIDTH: f32 = 220.0;

        let time_x = 0.0;
        let role_x = time_x + TIME_WIDTH + GAP;
        let damage_right_x = role_x + ROLE_WIDTH + GAP + DAMAGE_WIDTH;
        let target_x = damage_right_x + GAP;
        let row_width = available_width.max(target_x + MIN_TARGET_WIDTH);

        Self {
            row_width,
            time_x,
            role_x,
            damage_right_x,
            target_x,
        }
    }
}

fn draw_hit_table_header(ui: &mut egui::Ui, layout: HitTableLayout) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(layout.row_width, 22.0),
        egui::Sense::hover(),
    );
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter();
    let font = egui::FontId::proportional(13.0);
    let color = Color32::from_gray(160);

    painter.text(
        egui::pos2(x + layout.time_x, y),
        egui::Align2::LEFT_CENTER,
        "时间",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.role_x, y),
        egui::Align2::LEFT_CENTER,
        "角色",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.damage_right_x, y),
        egui::Align2::RIGHT_CENTER,
        "伤害",
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.target_x, y),
        egui::Align2::LEFT_CENTER,
        "目标 HP",
        font,
        color,
    );
}

fn draw_hit_table_row(
    ui: &mut egui::Ui,
    layout: HitTableLayout,
    time: &str,
    role: &str,
    damage: &str,
    target_hp: &str,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(layout.row_width, 24.0),
        egui::Sense::hover(),
    );
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter();
    let text_color = Color32::from_gray(185);
    let damage_color = Color32::from_rgb(235, 188, 95);
    let mono = egui::FontId::monospace(13.0);
    let proportional = egui::FontId::proportional(13.0);

    painter.text(
        egui::pos2(x + layout.time_x, y),
        egui::Align2::LEFT_CENTER,
        time,
        mono.clone(),
        text_color,
    );
    painter.text(
        egui::pos2(x + layout.role_x, y),
        egui::Align2::LEFT_CENTER,
        role,
        proportional,
        text_color,
    );
    painter.text(
        egui::pos2(x + layout.damage_right_x, y),
        egui::Align2::RIGHT_CENTER,
        damage,
        mono.clone(),
        damage_color,
    );
    painter.text(
        egui::pos2(x + layout.target_x, y),
        egui::Align2::LEFT_CENTER,
        target_hp,
        mono,
        text_color,
    );
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

fn metric(ui: &mut egui::Ui, label: &str, value: String, color: Color32) {
    ui.vertical(|ui| {
        ui.label(RichText::new(value).size(20.0).strong().color(color));
        ui.label(
            RichText::new(label)
                .size(11.0)
                .color(Color32::from_gray(140)),
        );
    });
    ui.add_space(18.0);
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
