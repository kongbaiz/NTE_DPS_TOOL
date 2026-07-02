use super::*;

impl DpsApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        ui_config: UiConfig,
        config_warning: Option<String>,
    ) -> Self {
        install_fonts(&cc.egui_ctx);
        configure_style(&cc.egui_ctx, ui_config.dark_mode);
        let ui_config = ui_config.sanitized();
        let (hotkey, hotkey_receiver) =
            HotkeyHandle::start(cc.egui_ctx.clone(), ui_config.passthrough_hotkey);
        let (sender, receiver) = unbounded();
        let (resource_audit_sender, resource_audit_receiver) = unbounded();
        let (diagnostics_sender, diagnostics_receiver) = unbounded();
        let data_root = data_root();
        let characters_path = data_root.join(CHARACTER_DATA_PATH);
        let (mut characters, character_load_error) =
            match load_characters(characters_path.as_path()) {
                Ok(characters) => (characters, None),
                Err(error) => (
                    HashMap::new(),
                    Some(tf(
                        "Failed to load character data ({}): {}",
                        &[&characters_path.display().to_string(), &error.to_string()],
                    )),
                ),
            };
        fill_missing_character_colors_from_avatars(&mut characters, &data_root);
        let characters = Arc::new(characters);
        let abyss_overview = AbyssOverviewState::load();
        let history = HistoryState::load();
        // Decode the texture sets (avatars, attribute icons, damage digits,
        // reaction glyphs, monster portraits) on a background thread so the window
        // appears immediately instead of blocking on ~6 MB of PNG decode. The maps
        // start empty; every texture lookup in the draw code already falls back when
        // a key is missing, so rows show their color/initial placeholder until the
        // sets stream in. The loader repaints after each set so an idle UI wakes to
        // pick them up.
        let (texture_load_sender, texture_load_receiver) = unbounded();
        {
            let ctx = cc.egui_ctx.clone();
            let root = data_root.clone();
            let avatar_characters = Arc::clone(&characters);
            let monster_ids = abyss_overview.monster_ids();
            thread::spawn(move || {
                let send = |load: TextureLoad| {
                    if texture_load_sender.send(load).is_ok() {
                        ctx.request_repaint();
                    }
                };
                send(TextureLoad::Avatars(load_character_avatars(
                    &ctx,
                    &root,
                    &avatar_characters,
                )));
                send(TextureLoad::Attributes(load_attribute_icons(&ctx, &root)));
                send(TextureLoad::DamageDigits(load_damage_digit_textures(
                    &ctx, &root,
                )));
                send(TextureLoad::Reactions(load_reaction_text_textures(
                    &ctx, &root,
                )));
                send(TextureLoad::Monsters(load_monster_textures(
                    &ctx,
                    &root,
                    &monster_ids,
                )));
            });
        }
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
        // Probe the capture environment (Npcap device list + HTGame.exe NIC) on a
        // background thread so the window appears immediately instead of blocking on
        // device enumeration. `start_live` re-runs `refresh_game_network` on every
        // capture start, so this startup probe only seeds the initial status and
        // device dropdown and can never gate capturing. Results arrive via
        // `drain_device_detection`, guarded so a late result never clobbers a live
        // capture or a user-initiated refresh.
        let manual_capture_device = ui_config.manual_capture_device.clone();
        let devices: Vec<CaptureDevice> = Vec::new();
        let selected_device: usize = 0;
        let game_network: Option<GameNetwork> = None;
        let local_ip = String::new();
        let status = t("Detecting the capture environment...");
        let diagnostic: Option<String> = None;
        let (device_detection_sender, device_detection_receiver) = unbounded();
        {
            let ctx = cc.egui_ctx.clone();
            let manual = manual_capture_device.clone();
            let character_error = character_load_error.clone();
            thread::spawn(move || {
                let detection =
                    detect_capture_environment(manual.as_deref(), character_error.as_deref());
                if device_detection_sender.send(detection).is_ok() {
                    ctx.request_repaint();
                }
            });
        }
        let startup_error = match (config_warning, character_load_error) {
            (Some(config_error), Some(character_error)) => {
                Some(format!("{config_error}\n{character_error}"))
            }
            (Some(error), None) | (None, Some(error)) => Some(error),
            (None, None) => None,
        };
        let last_status_toast = status.clone();
        Self {
            characters,
            avatar_textures: HashMap::new(),
            attribute_textures: HashMap::new(),
            monster_textures: HashMap::new(),
            damage_digit_textures: HashMap::new(),
            reaction_textures: HashMap::new(),
            state: CombatState::default(),
            selected_abyss_half: AbyssHalf::First,
            abyss_compact_mode: false,
            hud_mode: false,
            hud_size_key: None,
            hud_config: ui_config.hud.clone(),
            abyss_overview,
            history,
            resource_audit: ResourceAuditState::default(),
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
            timeline_cache: TimelineCache::default(),
            skill_breakdown_cache: SkillBreakdownCache::default(),
            selected_timeline_char: None,
            selected_skill_breakdown_char: None,
            detail_last_scroll_activity: None,
            devices,
            selected_device,
            manual_capture_device,
            local_ip,
            game_network,
            filter: "udp".to_owned(),
            active_capture_filter: None,
            capture_quality_source: CaptureQualitySource::Unknown,
            include_incoming: true,
            server_damage_calibration: ui_config.server_damage_calibration,
            dps_time_mode: ui_config.dps_time_mode,
            timeline_bucket_seconds: ui_config.timeline_bucket_seconds,
            timeline_dps_view_mode: ui_config.timeline_dps_view_mode,
            capture: None,
            raw_capture: None,
            replay_stop: None,
            replay_thread: None,
            sender,
            receiver,
            resource_audit_sender,
            resource_audit_receiver,
            resource_audit_thread: None,
            diagnostics_sender,
            diagnostics_receiver,
            diagnostics_thread: None,
            diagnostics_report: None,
            diagnostics_running: false,
            texture_load_receiver,
            device_detection_receiver,
            awaiting_device_detection: true,
            capture_log_stats: None,
            paused_events: VecDeque::new(),
            dropped_debug_packets: 0,
            status,
            last_status_toast,
            status_toast: None,
            diagnostic,
            last_error: startup_error,
            last_error_action: None,
            last_error_viewport: egui::ViewportId::ROOT,
            console_open: false,
            console_corner_applied: false,
            console_tab: ConsoleTab::default(),
            debug_only_hits: false,
            debug_search: String::new(),
            character_editor,
            encrypted_ini_editor: EncryptedIniEditorState::default(),
            paused: false,
            language: ui_config.language,
            dark_mode: ui_config.dark_mode,
            always_on_top: ui_config.always_on_top,
            mouse_passthrough: false,
            passthrough_hotkey: ui_config.passthrough_hotkey,
            opacity: ui_config.opacity,
            applied_opacity: None,
            corner_applied_hwnd: None,
            main_window_size: ui_config
                .main_window_size
                .map(egui::Vec2::from)
                .unwrap_or(MAIN_WINDOW_BASE_SIZE),
            abyss_window_size: ui_config
                .abyss_window_size
                .map(egui::Vec2::from)
                .unwrap_or(ABYSS_WINDOW_BASE_SIZE),
            hit_detail_window_size: ui_config
                .hit_detail_window_size
                .map(egui::Vec2::from)
                .unwrap_or(HIT_DETAIL_WINDOW_BASE_SIZE),
            team_hit_detail_window_size: ui_config
                .team_hit_detail_window_size
                .map(egui::Vec2::from)
                .unwrap_or(TEAM_HIT_DETAIL_WINDOW_BASE_SIZE),
            console_window_size: ui_config
                .console_window_size
                .map(egui::Vec2::from)
                .unwrap_or(CONSOLE_WINDOW_BASE_SIZE),
            main_size_restore_frames: 0,
            toolbar_min_content_width: 0.0,
            applied_main_min_size: egui::Vec2::ZERO,
            // eframe may replace the context style after app construction.
            style_dark_mode_applied: None,
            opacity_reapply_frames: 4,
            theme_transition_from: None,
            theme_transition_started_at: None,
            pending_file_dialog: None,
            active_import: None,
            pending_confirmation: None,
            pending_confirmation_viewport: egui::ViewportId::ROOT,
            saved_ui_config: ui_config,
            pending_ui_config: None,
            ui_config_path: config::config_path(),
            native_file_drop: NativeFileDrop::new(),
            last_dropped_file: None,
            hotkey_receiver,
            hotkey,
        }
    }

    pub(crate) fn stop_engine(&mut self) {
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
        self.active_import = None;
    }

    pub(crate) fn reset_combat_session(&mut self) {
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
        self.timeline_cache = TimelineCache::default();
        self.skill_breakdown_cache = SkillBreakdownCache::default();
        self.selected_timeline_char = None;
        self.selected_skill_breakdown_char = None;
        self.detail_last_scroll_activity = None;
        self.paused = false;
        self.paused_events.clear();
        self.dropped_debug_packets = 0;
        self.capture_quality_source = CaptureQualitySource::Unknown;
    }

    pub(crate) fn has_session_data(&self) -> bool {
        !self.state.hits.is_empty()
            || !self.state.packets.is_empty()
            || !self.state.stats.is_empty()
            || self.state.abyss.is_active()
    }

    pub(crate) fn request_reset_combat_session(&mut self) {
        if self.has_session_data() || self.capture.is_some() || self.replay_thread.is_some() {
            self.request_confirmation_for(egui::ViewportId::ROOT, ConfirmationAction::ResetSession);
        } else {
            self.reset_combat_session();
        }
    }

    pub(crate) fn request_start_live(&mut self) {
        if self.has_session_data() {
            self.request_confirmation_for(egui::ViewportId::ROOT, ConfirmationAction::StartLive);
        } else {
            self.start_live();
        }
    }

    pub(crate) fn request_import_file(&mut self, kind: DebugImportKind, path: PathBuf) {
        self.request_import_file_for(kind, path, egui::ViewportId::ROOT);
    }

    pub(crate) fn request_import_file_for(
        &mut self,
        kind: DebugImportKind,
        path: PathBuf,
        viewport: egui::ViewportId,
    ) {
        let action = match kind {
            DebugImportKind::Pcapng => ConfirmationAction::ImportPcapng(path),
            DebugImportKind::CaptureJson => ConfirmationAction::ImportCaptureJson(path),
            DebugImportKind::EncryptedIni => {
                self.load_encrypted_ini_for(path, viewport);
                return;
            }
        };
        if self.has_session_data() || self.capture.is_some() || self.replay_thread.is_some() {
            self.request_confirmation_for(viewport, action);
        } else {
            self.run_confirmation_action_for(action, viewport);
        }
    }

    pub(crate) fn run_confirmation_action_for(
        &mut self,
        action: ConfirmationAction,
        viewport: egui::ViewportId,
    ) {
        match action {
            ConfirmationAction::StartLive => self.start_live(),
            ConfirmationAction::ResetSession => {
                self.stop_engine();
                self.reset_combat_session();
                self.status = t("Stats reset");
            }
            ConfirmationAction::ImportPcapng(path) => self.start_pcapng_import_for(path, viewport),
            ConfirmationAction::ImportCaptureJson(path) => {
                self.start_capture_json_import_for(path, viewport);
            }
            ConfirmationAction::ClearEncryptedIni => {
                self.encrypted_ini_editor = EncryptedIniEditorState::default();
                self.status = t("Encrypted INI editor cleared");
            }
            ConfirmationAction::ReloadEncryptedIni(path) => {
                self.load_encrypted_ini_for(path, viewport)
            }
            ConfirmationAction::DeleteHistory(record_id) => {
                self.delete_history_record_for(record_id, viewport);
            }
            ConfirmationAction::ClearCaptureLogs => self.clear_capture_logs_now(),
        }
    }

    /// Lazily (re)scan `logs/` for raw capture files so the settings panel can show
    /// disk usage without doing file I/O every frame.
    pub(crate) fn refresh_capture_log_stats(&mut self) {
        self.capture_log_stats = Some(capture_logs::scan_capture_logs(Path::new(
            capture_logs::CAPTURE_LOG_DIR,
        )));
    }

    /// Delete the raw capture logs. The active capture's file is held open by the
    /// OS, so it fails to delete and is reported as "占用中" rather than removed.
    fn clear_capture_logs_now(&mut self) {
        let outcome = capture_logs::clear_capture_logs(Path::new(capture_logs::CAPTURE_LOG_DIR));
        self.refresh_capture_log_stats();
        self.status = if outcome.deleted == 0 && outcome.failed == 0 {
            t("No capture files to clear")
        } else if outcome.failed > 0 {
            tf(
                "Cleared {} capture files (freed {}); {} in use and not deleted",
                &[
                    &outcome.deleted.to_string(),
                    &capture_logs::format_bytes(outcome.freed_bytes),
                    &outcome.failed.to_string(),
                ],
            )
        } else {
            tf(
                "Cleared {} capture files, freed {}",
                &[
                    &outcome.deleted.to_string(),
                    &capture_logs::format_bytes(outcome.freed_bytes),
                ],
            )
        };
    }

    pub(crate) fn request_confirmation_for(
        &mut self,
        viewport: egui::ViewportId,
        action: ConfirmationAction,
    ) {
        self.pending_confirmation = Some(action);
        self.pending_confirmation_viewport = viewport;
    }

    pub(crate) fn set_last_error(
        &mut self,
        message: impl Into<String>,
        action: Option<ErrorAction>,
    ) {
        self.set_last_error_for(egui::ViewportId::ROOT, message, action);
    }

    pub(crate) fn set_last_error_for(
        &mut self,
        viewport: egui::ViewportId,
        message: impl Into<String>,
        action: Option<ErrorAction>,
    ) {
        self.last_error = Some(message.into());
        self.last_error_action = action;
        self.last_error_viewport = viewport;
    }

    pub(crate) fn set_last_error_in(
        &mut self,
        ctx: &egui::Context,
        message: impl Into<String>,
        action: Option<ErrorAction>,
    ) {
        self.set_last_error_for(ctx.viewport_id(), message, action);
    }

    pub(crate) fn clear_last_error(&mut self) {
        self.last_error = None;
        self.last_error_action = None;
    }

    pub(crate) fn set_passthrough_hotkey(&mut self, hotkey: PassthroughHotkey) {
        if self.passthrough_hotkey == hotkey {
            return;
        }
        self.passthrough_hotkey = hotkey;
        self.hotkey.set_passthrough_hotkey(hotkey);
        self.status = tf("Mouse passthrough hotkey switched to {}", &[hotkey.label()]);
    }

    pub(crate) fn drain_hotkeys(&mut self, ctx: &egui::Context) {
        let passthrough_key = passthrough_egui_key(self.passthrough_hotkey);
        let passthrough_pressed = ctx.input(|input| input.key_pressed(passthrough_key));
        let import_pressed =
            ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::O));
        #[cfg(not(feature = "no_debug"))]
        let f12_pressed = ctx.input(|input| input.key_pressed(egui::Key::F12));
        if passthrough_pressed {
            self.toggle_mouse_passthrough(ctx);
        }
        if import_pressed {
            self.request_debug_import(ctx, DebugImportKind::Pcapng);
        }
        #[cfg(not(feature = "no_debug"))]
        if f12_pressed {
            self.console_open = !self.console_open;
            if self.console_open {
                self.console_corner_applied = false;
                self.console_tab = ConsoleTab::Packets;
            }
        }
        while let Ok(event) = self.hotkey_receiver.try_recv() {
            match event {
                HotkeyEvent::TogglePassthrough => {
                    self.toggle_mouse_passthrough(ctx);
                }
                #[cfg(not(feature = "no_debug"))]
                HotkeyEvent::ToggleDebug => {
                    self.console_open = !self.console_open;
                    if self.console_open {
                        self.console_corner_applied = false;
                        self.console_tab = ConsoleTab::Packets;
                    }
                }
                HotkeyEvent::RegistrationFailed(shortcut) => {
                    self.diagnostic = Some(tf(
                        "Could not register global hotkey {}; it may be in use by another program",
                        &[&shortcut],
                    ));
                }
            }
        }
    }

    pub(crate) fn set_mouse_passthrough(&mut self, ctx: &egui::Context, enabled: bool) {
        if self.mouse_passthrough == enabled {
            return;
        }
        self.mouse_passthrough = enabled;
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(enabled));
        self.opacity_reapply_frames = 2;
        let hotkey = self.passthrough_hotkey.label();
        self.status = if self.mouse_passthrough {
            if self.hud_mode {
                tf("HUD passthrough on; press {} to enter edit mode", &[hotkey])
            } else {
                tf("Mouse passthrough on; press {} to turn off", &[hotkey])
            }
        } else if self.hud_mode {
            tf(
                "HUD edit mode on; press {} to return to game passthrough",
                &[hotkey],
            )
        } else {
            t("Mouse passthrough off")
        };
    }

    pub(crate) fn toggle_mouse_passthrough(&mut self, ctx: &egui::Context) {
        self.set_mouse_passthrough(ctx, !self.mouse_passthrough);
    }

    pub(crate) fn set_hud_mode(&mut self, ctx: &egui::Context, enabled: bool) {
        if self.hud_mode == enabled {
            return;
        }
        self.hud_mode = enabled;
        if enabled {
            if !self.always_on_top {
                self.always_on_top = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::AlwaysOnTop,
                ));
            }
            self.set_mouse_passthrough(ctx, true);
            self.status = tf(
                "Combat HUD on: always-on-top with mouse passthrough by default; press {} to edit",
                &[self.passthrough_hotkey.label()],
            );
        } else {
            self.set_mouse_passthrough(ctx, false);
            self.status = t("Exited combat HUD");
            // The exit click lands mid-frame — after this frame's HUD size command and
            // the HUD strip render, but before size tracking runs — so `hud_mode` is
            // already false when tracking executes. Suppress it now, otherwise the
            // still-HUD-sized window is written over `main_window_size` and the window
            // then "restores" to that small size instead of its pre-HUD size.
            self.main_size_restore_frames = 8;
        }
    }

    pub(crate) fn toggle_always_on_top(&mut self, ctx: &egui::Context) {
        self.always_on_top = !self.always_on_top;
        let level = if self.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        self.opacity_reapply_frames = 2;
        self.status = if self.always_on_top {
            t("Always-on-top enabled")
        } else {
            t("Always-on-top disabled")
        };
    }

    pub(crate) fn title_bar(&mut self, ui: &mut egui::Ui) {
        let title_height = ui.available_height().max(28.0);
        // The whole title bar is the drag-to-move zone: allocate it first with a
        // drag sense, then draw the dot/title/buttons on top. Interactive widgets
        // (added later) win the pointer where they are, so dragging still works on
        // any empty area — the centered title included.
        let (full_rect, title_drag) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), title_height),
            egui::Sense::click_and_drag(),
        );
        if title_drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let title_status = if self.paused {
            tf(
                "Paused · pending {} · dropped debug packets {}",
                &[
                    &self.paused_events.len().to_string(),
                    &self.dropped_debug_packets.to_string(),
                ],
            )
        } else {
            self.status.clone()
        };

        // Native window controls, right-aligned: minimize · maximize/restore ·
        // close. Drawn first so their extent is known before the title centers
        // itself. The overlay toggles (pin/passthrough/appearance) moved off the
        // title bar onto the live toolbar — see `control_buttons`.
        let mut controls = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(full_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        controls.set_clip_rect(full_rect);
        {
            let ui = &mut controls;
            ui.spacing_mut().item_spacing.x = 2.0;
            if window_control_button(ui, WindowControlIcon::Close, &t("Close")).clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            let maximized = ui
                .input(|input| input.viewport().maximized)
                .unwrap_or(false);
            let (icon, tooltip) = if maximized {
                (WindowControlIcon::Restore, t("Restore"))
            } else {
                (WindowControlIcon::Maximize, t("Maximize"))
            };
            if window_control_button(ui, icon, &tooltip).clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if window_control_button(ui, WindowControlIcon::Minimize, &t("Minimize")).clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }
        let controls_left = controls.min_rect().left();

        // Status dot pinned to the far left; hovering it shows the current status.
        let dot_center = egui::pos2(full_rect.left() + 5.0, full_rect.center().y);
        let dot_rect =
            egui::Rect::from_center_size(dot_center, egui::vec2(12.0, full_rect.height()));
        let dot_response = ui.interact(
            dot_rect,
            ui.id().with("title_status_dot"),
            egui::Sense::hover(),
        );
        ui.painter().circle_filled(
            dot_center,
            3.5,
            status_color(&self.status, self.paused, self.dark_mode),
        );
        dot_response.on_hover_text(title_status);

        // Centered branding, clipped to the gap between the dot and the window
        // controls so a too-narrow window elides it against the buttons instead of
        // painting over them.
        let title_clip = egui::Rect::from_min_max(
            egui::pos2(dot_rect.right(), full_rect.top()),
            egui::pos2(
                (controls_left - 6.0).max(dot_rect.right()),
                full_rect.bottom(),
            ),
        );
        ui.painter().with_clip_rect(title_clip).text(
            full_rect.center(),
            egui::Align2::CENTER_CENTER,
            "NTE DPS TOOL",
            egui::FontId::proportional(13.0),
            theme_accent(self.dark_mode),
        );
    }

    /// Keeps the main window wide enough that the live toolbar's two button groups
    /// can never overlap — even when a longer-text language widens the buttons.
    ///
    /// `toolbar_min_content_width` is remeasured every frame from the real localized
    /// labels ([`Self::control_buttons`]); here it becomes the enforced `MinInnerSize`
    /// (never below the configured floor). Height keeps the configured floor since the
    /// vertical stack only clips, never overlaps. If the window is meaningfully
    /// narrower than the minimum it is nudged back up — this heals a small persisted
    /// size or the size restored after leaving HUD, not just a freshly grown minimum.
    ///
    /// The caller must skip this while a programmatic resize is still settling (see
    /// `main_size_restore_frames`), otherwise it would clamp the in-flight restore to
    /// the minimum instead of the user's larger saved size.
    pub(crate) fn enforce_main_min_size(&mut self, ctx: &egui::Context, maximized: bool) {
        // Panel side margins (10 each) + the animated_controls 2px inset each side +
        // a little slack so the rightmost button keeps clear of the window edge.
        const SIDE_ALLOWANCE: f32 = 28.0;
        // Deadband: after the OS rounds a requested logical size to physical pixels
        // and back, the reported width can sit a hair under the minimum. Only correct
        // a shortfall larger than this, so rounding noise can't set up a per-frame
        // resize oscillation (edge "jitter"). The toolbar reserves a 24px inter-group
        // gap, so being a few px under the enforced minimum still never overlaps.
        const UNDERSIZE_DEADBAND: f32 = 6.0;
        let min_width =
            (self.toolbar_min_content_width + SIDE_ALLOWANCE).max(config::MAIN_WINDOW_MIN_SIZE[0]);
        let min_size = egui::vec2(min_width.ceil(), config::MAIN_WINDOW_MIN_SIZE[1]);
        if (min_size - self.applied_main_min_size).length() > 0.5 {
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min_size));
            self.applied_main_min_size = min_size;
        }
        // winit clamps only *future* user resizes to the minimum, so a window that is
        // already too small (startup default, stale saved size, HUD-exit restore, or a
        // language switch that grew the minimum) must be nudged wider here. Skipped
        // while maximized (would drop that state) and until the viewport reports a real
        // size (an early degenerate rect must not shrink it).
        if !maximized {
            let current = ctx.content_rect().size();
            if current.x >= 1.0 && current.y >= 1.0 && current.x < min_size.x - UNDERSIZE_DEADBAND {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    min_size.x,
                    current.y.max(min_size.y),
                )));
            }
        }
    }

    /// Compact title strip for HUD mode: a drag zone plus the two controls that
    /// matter while positioning the overlay. It is hidden completely while
    /// click-through is active so the combat readout sits directly on the game.
    pub(crate) fn hud_title_bar(&mut self, ui: &mut egui::Ui) {
        if self.mouse_passthrough {
            return;
        }
        let (full_rect, drag) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ui.available_height().max(24.0)),
            egui::Sense::click_and_drag(),
        );
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        // A solid rail makes the edit strip easy to grab after the pass-through
        // hotkey disables viewport mouse pass-through.
        let painter = ui.painter();
        painter.rect_filled(
            full_rect,
            egui::CornerRadius {
                nw: 8,
                ne: 8,
                sw: 0,
                se: 0,
            },
            Color32::from_rgb(14, 16, 20),
        );
        painter.hline(
            full_rect.x_range(),
            full_rect.bottom() - 0.5,
            Stroke::new(1.0, Color32::from_rgb(39, 201, 146)),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(full_rect.shrink2(egui::vec2(8.0, 0.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.label(
            RichText::new("NTE DPS")
                .size(10.5)
                .strong()
                .color(Color32::from_rgb(218, 224, 228)),
        );
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let passthrough_hint = tf(
                "{} toggles anytime; while passthrough is on you can't click buttons, so press {} to turn it off before exiting",
                &[
                    self.passthrough_hotkey.label(),
                    self.passthrough_hotkey.label(),
                ],
            );
            if ui
                .small_button(t("Exit"))
                .on_hover_text(t("Return to the normal window"))
                .clicked()
            {
                self.set_hud_mode(ui.ctx(), false);
            }
            // A plain small button to match Exit: this strip only renders while
            // passthrough is off (it returns early otherwise), so a selectable
            // toggle's active state would never show here — it just read as a
            // heavier, out-of-place control on the compact rail.
            if ui
                .small_button(t("Passthrough"))
                .on_hover_ui(|ui| {
                    // The HUD is its own small OS window (HUD_WINDOW_WIDTH), not
                    // a panel inside a larger one — a tooltip can't spill past
                    // its edges the way it could in the normal window, so wrap
                    // well short of that width instead of relying on the
                    // default single-line-then-clip tooltip sizing.
                    ui.set_max_width(HUD_WINDOW_WIDTH - 100.0);
                    ui.label(passthrough_hint);
                })
                .clicked()
            {
                self.toggle_mouse_passthrough(ui.ctx());
            }
        });
    }

    pub(crate) fn start_live(&mut self) {
        self.stop_engine();
        self.active_capture_filter = None;
        if let Err(error) = self.refresh_game_network() {
            self.set_last_error(error, Some(ErrorAction::RefreshNetwork));
            return;
        }
        let Some(device) = self.devices.get(self.selected_device).cloned() else {
            self.set_last_error(
                t("No usable capture device; confirm Npcap is installed"),
                Some(ErrorAction::RefreshNetwork),
            );
            return;
        };
        let local_ip = self.game_network.as_ref().map(|network| network.local_ip);
        let capture_filter = self.filter.clone();
        self.reset_combat_session();
        self.capture_quality_source = CaptureQualitySource::Live;
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
        self.status = t("Starting live capture...");
    }

    pub(crate) fn refresh_game_network(&mut self) -> Result<(), String> {
        // A user-initiated refresh owns the device state from here on; drop any
        // still-pending startup probe so it can't clobber this result.
        self.awaiting_device_detection = false;
        self.devices = list_devices().inspect_err(|error| {
            self.diagnostic = Some(error.clone());
        })?;
        if let Some(name) = self.manual_capture_device.clone() {
            return self.apply_manual_capture_device(&name);
        }
        let (index, network) = detect_game_device(&self.devices).inspect_err(|error| {
            self.diagnostic = Some(error.clone());
        })?;
        self.selected_device = index;
        self.local_ip = network.local_ip.to_string();
        self.status = t("Game detected, ready");
        self.diagnostic = None;
        self.game_network = Some(network);
        Ok(())
    }

    /// Manual capture mode: pin capture to the chosen NIC and best-effort resolve the game's local
    /// IP for direction inference. A missing game connection is non-fatal — capture still proceeds
    /// and `infer_outgoing` falls back to its public/private heuristic. Only a vanished NIC aborts.
    pub(crate) fn apply_manual_capture_device(&mut self, name: &str) -> Result<(), String> {
        let Some(index) = self.devices.iter().position(|device| device.name == name) else {
            let message = tf(
                "The manually selected NIC ({}) is currently unavailable; reselect in settings or switch back to auto",
                &[name],
            );
            self.diagnostic = Some(message.clone());
            self.game_network = None;
            self.local_ip.clear();
            self.status = t("Manual NIC unavailable");
            return Err(message);
        };
        self.selected_device = index;
        match detect_game_network() {
            Ok(network) => {
                self.local_ip = network.local_ip.to_string();
                self.game_network = Some(network);
                self.status = t("Ready (manual NIC)");
                self.diagnostic = None;
            }
            Err(error) => {
                self.local_ip.clear();
                self.game_network = None;
                self.status = t("Manual NIC selected (no game connection detected)");
                self.diagnostic = Some(error);
            }
        }
        Ok(())
    }

    pub(crate) fn start_pcapng_import_for(&mut self, path: PathBuf, viewport: egui::ViewportId) {
        self.stop_engine();
        self.raw_capture = None;
        self.active_capture_filter = None;
        self.reset_combat_session();
        self.capture_quality_source = CaptureQualitySource::PcapngReplay;
        let local_ip_hint = self
            .game_network
            .as_ref()
            .map(|network| network.local_ip)
            .or_else(|| self.local_ip.parse::<Ipv4Addr>().ok());
        let stop = Arc::new(AtomicBool::new(false));
        self.active_import = Some(ActiveImport {
            kind: DebugImportKind::Pcapng,
            path: path.clone(),
            started_at: Instant::now(),
            viewport,
        });
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
            || t("Importing and parsing pcapng (heuristic direction)..."),
            |ip| {
                tf(
                    "Importing and parsing pcapng (local IP {} filter/direction)...",
                    &[&ip.to_string()],
                )
            },
        );
    }

    pub(crate) fn start_capture_json_import_for(
        &mut self,
        path: PathBuf,
        viewport: egui::ViewportId,
    ) {
        self.stop_engine();
        self.raw_capture = None;
        self.active_capture_filter = None;
        self.reset_combat_session();
        self.capture_quality_source = CaptureQualitySource::JsonReplay;
        let stop = Arc::new(AtomicBool::new(false));
        self.active_import = Some(ActiveImport {
            kind: DebugImportKind::CaptureJson,
            path: path.clone(),
            started_at: Instant::now(),
            viewport,
        });
        self.replay_thread = Some(import_capture_json(path, self.sender.clone(), stop.clone()));
        self.replay_stop = Some(stop);
        self.status = t("Importing capture JSON...");
    }

    pub(crate) fn process_file_drops(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
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

    pub(crate) fn import_dropped_file(&mut self, path: PathBuf) {
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
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        match extension.as_deref() {
            Some("pcapng") => self.request_import_file(DebugImportKind::Pcapng, path),
            Some("json") => self.request_import_file(DebugImportKind::CaptureJson, path),
            _ => {
                let name = file_display_name(&path);
                self.set_last_error(
                    tf(
                        "Unsupported dropped file: {}\nCurrently .pcapng and .json are supported",
                        &[&name],
                    ),
                    Some(ErrorAction::OpenPcapng),
                );
            }
        }
    }

    /// Switch the live UI language and reload its locale map. The change is picked up
    /// by the next frame; `current_ui_config` includes it so the debounced save
    /// persists the choice to the config file.
    pub(crate) fn set_language(&mut self, language: Language) {
        self.language = language;
        i18n::set_language(language);
    }

    pub(crate) fn current_ui_config(&self) -> UiConfig {
        UiConfig {
            language: self.language,
            opacity: self.opacity,
            dark_mode: self.dark_mode,
            always_on_top: self.always_on_top,
            server_damage_calibration: self.server_damage_calibration,
            manual_capture_device: self.manual_capture_device.clone(),
            dps_time_mode: self.dps_time_mode,
            timeline_bucket_seconds: self.timeline_bucket_seconds,
            timeline_dps_view_mode: self.timeline_dps_view_mode,
            hud: self.hud_config.clone(),
            passthrough_hotkey: self.passthrough_hotkey,
            main_window_size: Some([self.main_window_size.x, self.main_window_size.y]),
            abyss_window_size: Some([self.abyss_window_size.x, self.abyss_window_size.y]),
            hit_detail_window_size: Some([
                self.hit_detail_window_size.x,
                self.hit_detail_window_size.y,
            ]),
            team_hit_detail_window_size: Some([
                self.team_hit_detail_window_size.x,
                self.team_hit_detail_window_size.y,
            ]),
            console_window_size: Some([self.console_window_size.x, self.console_window_size.y]),
        }
        .sanitized()
    }

    pub(crate) fn ui_config_save_plan(
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

    pub(crate) fn persist_ui_config(&mut self) {
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
                    self.set_last_error(
                        tf(
                            "Failed to save UI config; check permissions or disk space: {}\n{}",
                            &[&error, &self.ui_config_path.display().to_string()],
                        ),
                        Some(ErrorAction::OpenConsole),
                    );
                    self.pending_ui_config = Some((pending, now + UI_CONFIG_SAVE_RETRY_DELAY));
                }
            },
        }
    }

    pub(crate) fn persist_ui_config_on_shutdown(&mut self) {
        let current = self.current_ui_config();
        if let Some((pending, _)) = self.pending_ui_config.take() {
            let _ = config::save(&self.ui_config_path, &pending);
            return;
        }
        if current != self.saved_ui_config {
            let _ = config::save(&self.ui_config_path, &current);
        }
    }

    pub(crate) fn request_debug_import(&mut self, ctx: &egui::Context, kind: DebugImportKind) {
        let purpose = FileDialogPurpose::DebugImport { kind };
        match kind {
            DebugImportKind::Pcapng => {
                let filter = t("Wireshark capture");
                self.spawn_file_dialog(ctx, purpose, move |owner| {
                    with_owner(
                        rfd::FileDialog::new().add_filter(filter, &["pcapng"]),
                        owner,
                    )
                    .pick_file()
                });
            }
            DebugImportKind::CaptureJson => {
                let filter = t("NTE exported capture");
                self.spawn_file_dialog(ctx, purpose, move |owner| {
                    with_owner(rfd::FileDialog::new().add_filter(filter, &["json"]), owner)
                        .pick_file()
                });
            }
            DebugImportKind::EncryptedIni => {
                let ini_filter = t("NTE encrypted INI");
                let all_filter = t("All files");
                self.spawn_file_dialog(ctx, purpose, move |owner| {
                    with_owner(
                        rfd::FileDialog::new()
                            .add_filter(ini_filter, &["ini"])
                            .add_filter(all_filter, &["*"]),
                        owner,
                    )
                    .pick_file()
                });
            }
        }
    }

    /// Run a native file dialog on a worker thread and remember what to do with
    /// the picked path (see [`FileDialogPurpose`]); [`Self::poll_file_dialog`]
    /// picks up the result. Only one dialog may be open at a time — further
    /// requests are ignored until it closes.
    ///
    /// `dialog` receives the root window as a [`DialogOwner`] so it can call
    /// `.set_parent(owner)`: an owned window always renders above its owner
    /// regardless of topmost/z-order, which is what keeps the dialog from
    /// appearing hidden behind an always-on-top window. This deliberately avoids
    /// `clear_process_windows_topmost`/`SetWindowPos`-based approaches — those
    /// deadlock on this app's wgpu backend when run on the UI thread (a same-
    /// thread `SetWindowPos` synchronously re-enters `logic()` via WndProc) and
    /// still block forever when moved to a worker thread (the cross-thread call
    /// waits on a UI-thread message that never gets drained).
    pub(crate) fn spawn_file_dialog(
        &mut self,
        ctx: &egui::Context,
        purpose: FileDialogPurpose,
        dialog: impl FnOnce(Option<DialogOwner>) -> Option<PathBuf> + Send + 'static,
    ) {
        if self.pending_file_dialog.is_some() {
            return;
        }
        let owner = DialogOwner::from_hwnd(self.corner_applied_hwnd);
        let (sender, receiver) = unbounded();
        let waker = ctx.clone();
        thread::spawn(move || {
            let picked = dialog(owner);
            let _ = sender.send(picked);
            // Wake an idle UI so poll_file_dialog sees the result promptly.
            waker.request_repaint();
        });
        self.pending_file_dialog = Some(PendingFileDialog {
            purpose,
            viewport: ctx.viewport_id(),
            receiver,
        });
        ctx.request_repaint();
    }

    pub(crate) fn poll_file_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_file_dialog else {
            return;
        };
        let result = match pending.receiver.try_recv() {
            Ok(result) => result,
            // Fallback wake in case the worker's repaint races this frame.
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
                return;
            }
            Err(TryRecvError::Disconnected) => None,
        };
        let Some(PendingFileDialog {
            purpose, viewport, ..
        }) = self.pending_file_dialog.take()
        else {
            return;
        };
        // Nudge opacity to reapply in case focus moved while the dialog was open.
        self.opacity_reapply_frames = 2;
        ctx.request_repaint();
        let Some(path) = result else {
            return;
        };
        match purpose {
            FileDialogPurpose::DebugImport { kind } => match kind {
                DebugImportKind::Pcapng | DebugImportKind::CaptureJson => {
                    self.request_import_file_for(kind, path, viewport);
                }
                DebugImportKind::EncryptedIni => self.load_encrypted_ini_for(path, viewport),
            },
            FileDialogPurpose::TeamDpsImportAll => self.finish_team_dps_import(viewport, &path),
            FileDialogPurpose::TeamDpsImportLine { upper } => {
                self.finish_team_dps_line_import(viewport, upper, &path);
            }
            FileDialogPurpose::TeamDpsExport { json } => {
                self.finish_team_dps_export(viewport, &path, &json);
            }
            FileDialogPurpose::CaptureInfoExport => {
                self.finish_capture_info_export(viewport, &path);
            }
            FileDialogPurpose::RawCaptureExport => self.finish_raw_capture_export(viewport, &path),
        }
    }

    pub(crate) fn drain_events(&mut self) {
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
    pub(crate) fn buffer_paused_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Packet(_) => {
                self.dropped_debug_packets = self.dropped_debug_packets.saturating_add(1);
            }
            EngineEvent::Hit(_)
            | EngineEvent::HitFollowUp(_)
            | EngineEvent::HitDamageCorrection(_)
            | EngineEvent::Abyss(_)
            | EngineEvent::TimeStop(_) => {
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
    pub(crate) fn enforce_engine_queue_hard_cap(&mut self) {
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

    pub(crate) fn shed_event_backlog(&mut self, started: Instant) {
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

    pub(crate) fn drain_pending_events(&mut self) {
        while let Some(event) = self.paused_events.pop_front() {
            self.apply_engine_event(event);
        }
        while let Ok(event) = self.receiver.try_recv() {
            self.apply_engine_event(event);
        }
    }

    pub(crate) fn apply_engine_event(&mut self, event: EngineEvent) {
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
                self.timeline_cache = TimelineCache::default();
                self.skill_breakdown_cache = SkillBreakdownCache::default();
                if let AbyssEvent::Stage { half, .. } = &event {
                    self.selected_abyss_half = *half;
                    self.abyss_compact_mode = true;
                } else if matches!(&event, AbyssEvent::Success { .. } | AbyssEvent::Exit { .. }) {
                    self.abyss_compact_mode = false;
                }
                self.state.apply_abyss_event(event);
            }
            EngineEvent::TimeStop(event) => {
                self.timeline_cache = TimelineCache::default();
                self.state.apply_time_stop_event(event);
            }
            EngineEvent::Status(status) => self.status = status,
            EngineEvent::Warning(warning) => {
                self.diagnostic = Some(tf(
                    "Some resources failed to load; features degraded: {}",
                    &[&warning],
                ));
            }
            EngineEvent::Error(error) => {
                self.status = t("Run failed");
                let action = import_error_action(&error);
                let viewport = self
                    .active_import
                    .as_ref()
                    .map_or(egui::ViewportId::ROOT, |task| task.viewport);
                self.set_last_error_for(viewport, humanize_engine_error(&error), action);
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
                    self.active_import = None;
                    self.status = t("Import complete; see parse quality on the diagnostics page");
                } else {
                    self.status = t("Stopped");
                }
            }
        }
    }

    pub(crate) fn update_status_toast(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.last_status_toast != self.status {
            self.last_status_toast = self.status.clone();
            if !self.status.trim().is_empty() {
                self.status_toast = Some(StatusToast {
                    text: self.status.clone(),
                    shown_until: now + STATUS_TOAST_DURATION,
                });
            }
        }

        if let Some(toast) = &self.status_toast {
            if toast.shown_until <= now {
                self.status_toast = None;
            } else {
                ctx.request_repaint_after(toast.shown_until.saturating_duration_since(now));
            }
        }
    }

    pub(crate) fn show_status_toast(&mut self, ctx: &egui::Context) {
        let Some(toast) = &self.status_toast else {
            return;
        };
        let now = Instant::now();
        if toast.shown_until <= now {
            self.status_toast = None;
            return;
        }

        let color = status_color(&toast.text, self.paused, self.dark_mode);
        let text = toast.text.clone();
        // Bottom-anchored, click-through toast: it never covers the top controls/metric cards, and
        // `interactable(false)` means clicks always pass through to the UI beneath even while it is
        // visible. A touch of translucency keeps any content underneath legible.
        let card = shadcn_card(self.dark_mode);
        let fill = Color32::from_rgba_unmultiplied(card.r(), card.g(), card.b(), 235);
        egui::Area::new(egui::Id::new("status_toast"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -14.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(fill)
                    .stroke(Stroke::new(1.0, color.gamma_multiply(0.85)))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        // The HUD is its own small OS window (HUD_WINDOW_WIDTH), not a
                        // panel inside the larger normal window — content wider than
                        // that clips at the window edge instead of just looking cramped.
                        let max_width = if self.hud_mode {
                            HUD_WINDOW_WIDTH - 40.0
                        } else {
                            420.0
                        };
                        ui.set_max_width(max_width);
                        ui.horizontal(|ui| {
                            let (dot_rect, _) =
                                ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot_rect.center(), 4.0, color);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text)
                                        .size(11.5)
                                        .color(shadcn_foreground(self.dark_mode)),
                                )
                                .wrap(),
                            );
                        });
                    });
            });
    }

    pub(crate) fn export_capture_info(&mut self, ctx: &egui::Context) {
        self.drain_pending_events();
        if self.state.hits.is_empty() && self.state.packets.is_empty() {
            self.set_last_error_in(
                ctx,
                t("No capture info to export"),
                Some(ErrorAction::OpenConsole),
            );
            return;
        }
        if self.capture.is_some() || self.replay_thread.is_some() {
            self.set_last_error_in(
                ctx,
                t("Stop capture or replay first, then export this capture info"),
                None,
            );
            return;
        }

        let filter = t("Capture info JSON");
        self.spawn_file_dialog(ctx, FileDialogPurpose::CaptureInfoExport, move |owner| {
            with_owner(
                rfd::FileDialog::new()
                    .add_filter(filter, &["json"])
                    .set_file_name(default_export_filename()),
                owner,
            )
            .save_file()
        });
    }

    fn finish_capture_info_export(&mut self, viewport: egui::ViewportId, path: &Path) {
        // The UI stayed live while the dialog was open, so re-check that no
        // capture or replay started in the meantime.
        if self.capture.is_some() || self.replay_thread.is_some() {
            self.set_last_error_for(
                viewport,
                t("Stop capture or replay first, then export this capture info"),
                None,
            );
            return;
        }
        match atomic_write_file(path, |writer| {
            let mut out = IoFmtWriter::new(writer);
            self.write_capture_export_json(&mut out);
            out.finish()
        }) {
            Ok(()) => {
                self.status = t("Capture info exported");
                self.clear_last_error();
            }
            Err(error) => {
                self.set_last_error_for(
                    viewport,
                    tf("Failed to export capture info: {}", &[&error.to_string()]),
                    None,
                );
            }
        }
    }

    pub(crate) fn export_raw_capture(&mut self, ctx: &egui::Context) {
        if self.capture.is_some() {
            self.set_last_error_in(
                ctx,
                t("Stop capture first, then save the full PCAPNG"),
                None,
            );
            return;
        }
        if self.raw_capture.is_none() {
            self.set_last_error_in(ctx, t("No full PCAPNG to save"), None);
            return;
        }
        let default_file_name = format!("nte_raw_{}.pcapng", Local::now().format("%Y%m%d_%H%M%S"));
        let filter = t("Full raw capture");
        self.spawn_file_dialog(ctx, FileDialogPurpose::RawCaptureExport, move |owner| {
            with_owner(
                rfd::FileDialog::new()
                    .add_filter(filter, &["pcapng"])
                    .set_file_name(default_file_name),
                owner,
            )
            .save_file()
        });
    }

    fn finish_raw_capture_export(&mut self, viewport: egui::ViewportId, destination: &Path) {
        // The UI stayed live while the dialog was open; the buffer may have been
        // cleared (or a capture restarted) in the meantime.
        if self.capture.is_some() {
            self.set_last_error_for(
                viewport,
                t("Stop capture first, then save the full PCAPNG"),
                None,
            );
            return;
        }
        let Some(raw_capture) = self.raw_capture.as_ref() else {
            self.set_last_error_for(viewport, t("No full PCAPNG to save"), None);
            return;
        };
        match raw_capture.save(destination) {
            Ok((packet_count, captured_bytes)) => {
                let file_name = destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.to_owned())
                    .unwrap_or_else(|| t("PCAPNG file"));
                self.status = tf(
                    "Saved the full capture to {} ({} packets, {} bytes)",
                    &[
                        &file_name,
                        &packet_count.to_string(),
                        &captured_bytes.to_string(),
                    ],
                );
                self.clear_last_error();
            }
            Err(error) => {
                self.set_last_error_for(
                    viewport,
                    tf("Failed to save the full capture: {}", &[&error.to_string()]),
                    None,
                );
            }
        }
    }

    pub(crate) fn write_capture_export_json(&self, mut out: &mut dyn std::fmt::Write) {
        let subtract_time_stop = self.subtract_time_stop_for_dps();
        let duration = self.state_duration_for_current_mode().max(0.001);
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
        writeln!(
            &mut out,
            "    \"dps\": {},",
            json_f64(self.state_dps_for_current_mode())
        )
        .ok();
        writeln!(
            &mut out,
            "    \"duration_seconds\": {},",
            json_f64(duration)
        )
        .ok();
        writeln!(
            &mut out,
            "    \"dps_time_mode\": {},",
            json_string(self.dps_time_mode.label())
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
            let row_duration = self
                .state
                .character_duration_with_time_stop(row, subtract_time_stop);
            let row_dps = self
                .state
                .character_dps_with_time_stop(row, subtract_time_stop);
            writeln!(&mut out, "    {{").ok();
            writeln!(&mut out, "      \"char_id\": {},", row.char_id).ok();
            writeln!(&mut out, "      \"name\": {},", json_string(&row.name)).ok();
            writeln!(&mut out, "      \"hits\": {},", row.hits).ok();
            writeln!(&mut out, "      \"damage\": {},", json_f64(row.damage)).ok();
            writeln!(&mut out, "      \"dps\": {},", json_f64(row_dps)).ok();
            writeln!(
                &mut out,
                "      \"duration_seconds\": {},",
                json_f64(row_duration)
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
        write_abyss_half_json(
            &mut out,
            "first_half",
            &self.state.abyss.first_half,
            subtract_time_stop,
            true,
        );
        write_abyss_half_json(
            &mut out,
            "second_half",
            &self.state.abyss.second_half,
            subtract_time_stop,
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

    pub(crate) fn selected_party_state(&self) -> Option<&PartyCombatState> {
        self.state
            .abyss
            .is_active()
            .then(|| self.state.abyss.half(self.selected_abyss_half))
    }

    pub(crate) fn subtract_time_stop_for_dps(&self) -> bool {
        matches!(self.dps_time_mode, DpsTimeMode::TimeStopAdjusted)
    }

    pub(crate) fn party_duration_for_current_mode(&self, party: &PartyCombatState) -> f64 {
        party.duration_with_time_stop(self.subtract_time_stop_for_dps())
    }

    pub(crate) fn party_dps_for_current_mode(&self, party: &PartyCombatState) -> f64 {
        party.dps_with_time_stop(self.subtract_time_stop_for_dps())
    }

    pub(crate) fn state_duration_for_current_mode(&self) -> f64 {
        self.state
            .duration_with_time_stop(self.subtract_time_stop_for_dps())
    }

    pub(crate) fn state_dps_for_current_mode(&self) -> f64 {
        self.state
            .dps_with_time_stop(self.subtract_time_stop_for_dps())
    }

    pub(crate) fn character_duration_for_current_source(&self, row: &CharacterStats) -> f64 {
        if let Some(party) = self.selected_party_state() {
            party.character_duration_with_time_stop(row, self.subtract_time_stop_for_dps())
        } else {
            self.state
                .character_duration_with_time_stop(row, self.subtract_time_stop_for_dps())
        }
    }

    pub(crate) fn character_dps_for_current_source(&self, row: &CharacterStats) -> f64 {
        if let Some(party) = self.selected_party_state() {
            party.character_dps_with_time_stop(row, self.subtract_time_stop_for_dps())
        } else {
            self.state
                .character_dps_with_time_stop(row, self.subtract_time_stop_for_dps())
        }
    }

    pub(crate) fn detail_source(&self) -> (HitDetailSource, u64) {
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

    pub(crate) fn note_detail_scroll_activity(&mut self, ctx: &egui::Context) {
        let scrolling = ctx.input(|input| {
            input.is_scrolling()
                || input.smooth_scroll_delta() != egui::Vec2::ZERO
                || ((self.hit_detail_char_id.is_some() || self.team_hit_detail_open)
                    && input.pointer.primary_down())
        });
        if scrolling {
            self.detail_last_scroll_activity = Some(Instant::now());
        }
    }

    pub(crate) fn detail_scroll_active(&self) -> bool {
        self.detail_last_scroll_activity
            .is_some_and(|last| last.elapsed() < DETAIL_CACHE_REFRESH_DELAY)
    }

    pub(crate) fn cached_skill_summaries(&mut self, char_id: u32) -> Vec<SkillDamageSummary> {
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

    pub(crate) fn cached_timeline_series(&mut self) -> TimelineSeries {
        let (source, generation) = self.detail_source();
        let subtract_time_stop = self.subtract_time_stop_for_dps();
        let bucket_seconds = config::sanitize_timeline_bucket_seconds(self.timeline_bucket_seconds);
        if (bucket_seconds - self.timeline_bucket_seconds).abs() > f32::EPSILON {
            self.timeline_bucket_seconds = bucket_seconds;
        }
        let key = TimelineCacheKey {
            source,
            generation,
            subtract_time_stop,
            bucket_millis: timeline_bucket_millis(bucket_seconds),
        };
        if self.timeline_cache.key.as_ref() != Some(&key) {
            let series = match source {
                HitDetailSource::Global => self
                    .state
                    .timeline(bucket_seconds as f64, subtract_time_stop),
                HitDetailSource::AbyssFirst => self.abyss_half_timeline_series(
                    AbyssHalf::First,
                    bucket_seconds as f64,
                    subtract_time_stop,
                ),
                HitDetailSource::AbyssSecond => self.abyss_half_timeline_series(
                    AbyssHalf::Second,
                    bucket_seconds as f64,
                    subtract_time_stop,
                ),
            };
            self.timeline_cache = TimelineCache {
                key: Some(key),
                series,
            };
        }
        self.timeline_cache.series.clone()
    }

    pub(crate) fn abyss_half_timeline_series(
        &self,
        half: AbyssHalf,
        bucket_seconds: f64,
        subtract_time_stop: bool,
    ) -> TimelineSeries {
        let mut series = self
            .state
            .abyss
            .half(half)
            .timeline(bucket_seconds, subtract_time_stop);
        if let (Some(start), Some(end)) = (series.start_timestamp, series.end_timestamp) {
            series.markers = self.state.abyss.timeline_markers_for_half(half, start, end);
        }
        series
    }

    pub(crate) fn cached_skill_breakdown(&mut self, char_id: Option<u32>) -> SkillBreakdown {
        let (source, generation) = self.detail_source();
        let key = SkillBreakdownCacheKey {
            source,
            generation,
            char_id,
        };
        if self.skill_breakdown_cache.key.as_ref() != Some(&key) {
            let breakdown = crate::engine::model::summarize_skill_breakdown(
                detail_hits_for_source(&self.state, source),
                char_id,
            );
            self.skill_breakdown_cache = SkillBreakdownCache {
                key: Some(key),
                breakdown,
            };
        }
        self.skill_breakdown_cache.breakdown.clone()
    }

    pub(crate) fn current_quality_summary(&self) -> CaptureQualitySummary {
        self.state
            .capture_quality_summary(self.capture_quality_source)
    }

    pub(crate) fn request_resource_audit(&mut self) {
        if self.resource_audit.loading {
            return;
        }
        self.resource_audit.loading = true;
        self.resource_audit.message = t("Checking runtime resources...");
        let sender = self.resource_audit_sender.clone();
        self.resource_audit_thread = Some(thread::spawn(move || {
            let summary = audit_runtime_resources();
            let _ = sender.send(summary);
        }));
    }

    /// Pick up texture sets decoded by the background loader thread and swap them
    /// into the live maps. Until a set arrives its map stays empty and draw-sites
    /// fall back gracefully, so this never blocks the first paint.
    pub(crate) fn drain_texture_loads(&mut self) {
        while let Ok(load) = self.texture_load_receiver.try_recv() {
            match load {
                TextureLoad::Avatars(map) => self.avatar_textures = map,
                TextureLoad::Attributes(map) => self.attribute_textures = map,
                TextureLoad::DamageDigits(map) => self.damage_digit_textures = map,
                TextureLoad::Reactions(map) => self.reaction_textures = map,
                TextureLoad::Monsters(map) => self.monster_textures = map,
            }
        }
    }

    /// Apply the startup capture-environment probe once it completes on its
    /// background thread. Guarded so a late result never overwrites a capture/replay
    /// already in flight or a device list a user-initiated refresh has populated.
    pub(crate) fn drain_device_detection(&mut self) {
        if !self.awaiting_device_detection {
            return;
        }
        let Ok(detection) = self.device_detection_receiver.try_recv() else {
            return;
        };
        self.awaiting_device_detection = false;
        if self.capture.is_some() || self.replay_thread.is_some() {
            return;
        }
        self.devices = detection.devices;
        self.selected_device = detection.selected_device;
        self.game_network = detection.game_network;
        self.local_ip = detection.local_ip;
        self.status = detection.status;
        self.diagnostic = detection.diagnostic;
    }

    pub(crate) fn drain_resource_audit(&mut self) {
        while let Ok(summary) = self.resource_audit_receiver.try_recv() {
            let error_count = summary.error_count();
            let warning_count = summary.warning_count();
            self.resource_audit.summary = Some(summary);
            self.resource_audit.loading = false;
            self.resource_audit.message = tf(
                "Resource check complete: {} errors, {} warnings",
                &[&error_count.to_string(), &warning_count.to_string()],
            );
            if let Some(thread) = self.resource_audit_thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub(crate) fn request_capture_diagnostics(&mut self) {
        if self.diagnostics_running {
            return;
        }
        self.diagnostics_running = true;
        let sender = self.diagnostics_sender.clone();
        let snapshot = self.diagnostic_snapshot();
        self.diagnostics_thread = Some(thread::spawn(move || {
            let report = run_capture_diagnostics(snapshot);
            let _ = sender.send(report);
        }));
    }

    pub(crate) fn drain_capture_diagnostics(&mut self) {
        while let Ok(report) = self.diagnostics_receiver.try_recv() {
            let failed = report.failed_count();
            let warnings = report.warning_count();
            self.diagnostics_report = Some(report);
            self.diagnostics_running = false;
            self.status = tf(
                "Diagnostics complete: {} failed, {} warnings",
                &[&failed.to_string(), &warnings.to_string()],
            );
            if let Some(thread) = self.diagnostics_thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub(crate) fn diagnostic_snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            capture_running: self.capture.is_some(),
            replay_running: self.replay_thread.is_some(),
            active_capture_filter: self.active_capture_filter.clone(),
            raw_packet_count: self
                .raw_capture
                .as_ref()
                .map_or(0, RawCaptureBuffer::packet_count),
            parsed_packet_count: self.state.packets.len(),
            hit_count: self.state.hits.len(),
            include_incoming: self.include_incoming,
            server_damage_calibration: self.server_damage_calibration,
            last_diagnostic: self.diagnostic.clone(),
        }
    }
}

/// Probe the capture environment: enumerate Npcap devices, then either honor the
/// persisted manual NIC override or auto-detect the HTGame.exe NIC. Folds in any
/// character-load error so the seeded status diagnostic matches the previous
/// synchronous startup behavior exactly. Pure aside from the OS queries, so it is
/// safe to run on the startup background thread.
fn detect_capture_environment(
    manual_capture_device: Option<&str>,
    character_load_error: Option<&str>,
) -> DeviceDetection {
    let (devices, device_error) = match list_devices() {
        Ok(devices) => (devices, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let (mut selected_device, mut game_network, mut status, mut diagnostic) = match device_error {
        Some(error) => (0, None, t("Capture environment unavailable"), Some(error)),
        None => match detect_game_device(&devices) {
            Ok((index, network)) => (index, Some(network), t("Ready"), None),
            Err(error) => (0, None, t("Game not detected"), Some(error)),
        },
    };
    // Apply the persisted manual NIC override (VPN fallback). The saved choice is kept even when
    // the interface is momentarily absent, so it re-engages once the adapter is back.
    if let Some(name) = manual_capture_device.filter(|_| !devices.is_empty()) {
        match devices.iter().position(|device| device.name == name) {
            Some(index) => {
                selected_device = index;
                match detect_game_network() {
                    Ok(network) => {
                        game_network = Some(network);
                        status = t("Ready (manual NIC)");
                        diagnostic = None;
                    }
                    Err(error) => {
                        game_network = None;
                        status = t("Manual NIC selected (no game connection detected)");
                        diagnostic = Some(error);
                    }
                }
            }
            None => {
                game_network = None;
                status = t("Manual NIC unavailable");
                diagnostic = Some(tf(
                    "The manually selected NIC ({}) is currently unavailable; reselect in settings or switch back to auto",
                    &[name],
                ));
            }
        }
    }
    if let Some(error) = character_load_error {
        diagnostic = Some(match diagnostic {
            Some(existing) => format!("{error}\n{existing}"),
            None => error.to_owned(),
        });
    }
    let local_ip = game_network
        .as_ref()
        .map(|network| network.local_ip.to_string())
        .unwrap_or_default();
    DeviceDetection {
        devices,
        selected_device,
        game_network,
        local_ip,
        status,
        diagnostic,
    }
}
