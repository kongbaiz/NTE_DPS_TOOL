use super::*;

impl DpsApp {
    pub(crate) fn abyss_selector(&mut self, ui: &mut egui::Ui) {
        if !self.state.abyss.is_active() {
            return;
        }
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 28.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().interact_size.y = 28.0;
                let floor = self.state.abyss.floor.map_or_else(
                    || t("Abyss"),
                    |floor| tf("Abyss Floor {}", &[&floor.to_string()]),
                );
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
                stable_selectable_value(
                    ui,
                    &mut self.selected_abyss_half,
                    AbyssHalf::First,
                    RichText::new(t(AbyssHalf::First.label())).size(13.0),
                );
                stable_selectable_value(
                    ui,
                    &mut self.selected_abyss_half,
                    AbyssHalf::Second,
                    RichText::new(t(AbyssHalf::Second.label())).size(13.0),
                );
                if self.state.abyss.success_at.is_some() {
                    ui.separator();
                    ui.label(
                        RichText::new(t("Challenge Cleared"))
                            .color(semantic_success(self.dark_mode)),
                    );
                }
                if self.abyss_compact_mode {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(t("Expand")).clicked() {
                            self.abyss_compact_mode = false;
                        }
                    });
                }
            },
        );
        ui.add_space(3.0);
    }

    pub(crate) fn summary_bar(&mut self, ui: &mut egui::Ui) {
        let (duration, dps, total_damage, total_damage_taken) =
            if let Some(party) = self.selected_party_state() {
                (
                    self.party_duration_for_current_mode(party),
                    self.party_dps_for_current_mode(party),
                    party.total_damage,
                    party.total_damage_taken,
                )
            } else {
                (
                    self.state_duration_for_current_mode(),
                    self.state_dps_for_current_mode(),
                    self.state.total_damage,
                    self.state.total_damage_taken,
                )
            };
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.columns(4, |columns| {
            compact_metric(
                &mut columns[0],
                &t("Team DPS"),
                format_number(dps),
                theme_accent(self.dark_mode),
                true,
            );
            let total_color = columns[1].visuals().text_color();
            compact_metric(
                &mut columns[1],
                &t("Total Damage"),
                format_number(total_damage),
                total_color,
                true,
            );
            compact_metric(
                &mut columns[2],
                &t("Total Damage Taken"),
                format_number(total_damage_taken),
                semantic_danger(self.dark_mode),
                false,
            );
            let time_color = columns[3].visuals().text_color();
            compact_metric(
                &mut columns[3],
                &t("Time"),
                format!("{duration:.1}s"),
                time_color,
                false,
            );
        });
    }

    /// Live-combat toolbar: capture lifecycle plus the overlay-shrink toggle.
    /// Everything else (settings, team data, abyss tables, debug) moved into the
    /// console window — see [`Self::console_panel`] — to keep this bar uncrowded.
    pub(crate) fn controls(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.spacing_mut().button_padding = egui::vec2(14.0, 4.0);
        ui.horizontal_centered(|ui| self.control_buttons(ui));
    }

    pub(crate) fn control_buttons(&mut self, ui: &mut egui::Ui) {
        // Measure both groups at the current language in an invisible sizing pass so
        // the enforced window minimum (see `enforce_main_min_size`) can grow to fit
        // them. Without this, a longer translation would let the right-aligned
        // toggles overflow back over the lifecycle buttons when the window is dragged
        // narrow — the overlap this guards against.
        let lifecycle_width =
            self.measure_toolbar_group("toolbar_lifecycle", ui, Self::lifecycle_buttons);
        let toggles_width =
            self.measure_toolbar_group("toolbar_toggles", ui, Self::overlay_toggle_buttons);
        // Raw widths plus a comfortable gap between the two groups.
        self.toolbar_min_content_width = lifecycle_width + toggles_width + 24.0;

        self.lifecycle_buttons(ui);
        // Overlay toggles that used to live on the title bar, right-aligned so the
        // capture-lifecycle buttons keep the left edge. The measured minimum above
        // guarantees room for both groups, so this right-to-left group never overflows
        // back over the lifecycle buttons.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            self.overlay_toggle_buttons(ui)
        });
    }

    /// Natural width of a toolbar button group at the current language, measured in an
    /// invisible sizing pass (left-to-right, unbounded) so it reflects the real
    /// localized labels without disturbing the visible layout or reserving space.
    fn measure_toolbar_group(
        &mut self,
        salt: &str,
        ui: &mut egui::Ui,
        build: impl FnOnce(&mut Self, &mut egui::Ui),
    ) -> f32 {
        let mut sizing = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(salt)
                .sizing_pass()
                .invisible()
                .max_rect(egui::Rect::from_min_size(
                    ui.max_rect().min,
                    egui::vec2(10_000.0, ui.available_height().max(1.0)),
                ))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        sizing.spacing_mut().item_spacing = ui.spacing().item_spacing;
        sizing.spacing_mut().button_padding = ui.spacing().button_padding;
        build(self, &mut sizing);
        sizing.min_rect().width()
    }

    /// Capture-lifecycle buttons on the left of the toolbar: start/stop, reset,
    /// pause/resume, the abyss collapse toggle, HUD and console.
    pub(crate) fn lifecycle_buttons(&mut self, ui: &mut egui::Ui) {
        if self.capture.is_none() && self.replay_thread.is_none() {
            if ui
                .add(primary_button(t("Start"), self.dark_mode))
                .on_hover_text(t(
                    "Auto-detect the HTGame.exe connection and NIC, then start live capture",
                ))
                .clicked()
            {
                self.request_start_live();
            }
        } else if ui
            .add(
                egui::Button::new(
                    RichText::new(t("Stop"))
                        .strong()
                        .color(semantic_danger(self.dark_mode)),
                )
                .stroke(Stroke::new(1.0, semantic_danger(self.dark_mode))),
            )
            .on_hover_text(t("Stop the current live capture or import replay"))
            .clicked()
        {
            self.stop_engine();
            self.drain_pending_events();
        }
        if ui
            .button(t("Reset"))
            .on_hover_text(t(
                "Clear the current stats; you will be asked to confirm first",
            ))
            .clicked()
        {
            self.request_reset_combat_session();
        }
        if ui
            .add(
                egui::Button::selectable(
                    self.paused,
                    if self.paused { t("Resume") } else { t("Pause") },
                )
                .frame_when_inactive(true),
            )
            .on_hover_text(t(
                "Pause UI processing; resuming catches up on buffered hit events",
            ))
            .clicked()
        {
            self.paused = !self.paused;
        }
        if self.state.abyss.is_active()
            && ui
                .button(t("Collapse"))
                .on_hover_text(t("Collapse the abyss line selector and toolbar"))
                .clicked()
        {
            self.abyss_compact_mode = true;
        }
        if ui
            .button("HUD")
            .on_hover_text(t("Switch to the backing-less combat HUD (overlays the game · exit from the appearance menu)"))
            .clicked()
        {
            self.set_hud_mode(ui.ctx(), true);
        }
        if ui
            .button(t("Console"))
            .on_hover_text(t(
                "Settings · team data · abyss values · character/INI · debug (F12)",
            ))
            .clicked()
        {
            self.console_open = true;
            self.console_corner_applied = false;
        }
    }

    /// Overlay toggles on the right of the toolbar: appearance menu, passthrough, pin.
    /// Added appearance→passthrough→pin so a right-to-left layout renders them
    /// pin · passthrough · appearance (a left-to-right sizing pass measures the same
    /// total width regardless of order).
    pub(crate) fn overlay_toggle_buttons(&mut self, ui: &mut egui::Ui) {
        self.appearance_menu(ui);
        let passthrough_label = if self.mouse_passthrough {
            t("Passthrough on")
        } else {
            t("Passthrough")
        };
        if ui
            .add(
                egui::Button::selectable(self.mouse_passthrough, passthrough_label)
                    .frame_when_inactive(true),
            )
            .on_hover_text(tf(
                "{} toggles mouse passthrough anytime",
                &[self.passthrough_hotkey.label()],
            ))
            .clicked()
        {
            self.toggle_mouse_passthrough(ui.ctx());
        }
        if ui
            .add(egui::Button::selectable(self.always_on_top, t("Pin")).frame_when_inactive(true))
            .on_hover_text(t("Keep the main window above the game"))
            .clicked()
        {
            self.toggle_always_on_top(ui.ctx());
        }
    }

    /// Appearance dropdown (opacity · theme · Combat HUD), shared by the live
    /// toolbar. Moved off the title bar; see [`Self::control_buttons`].
    pub(crate) fn appearance_menu(&mut self, ui: &mut egui::Ui) {
        let (appearance_response, _) = egui::containers::menu::MenuButton::from_button(
            egui::Button::new(t("Appearance")),
        )
        .ui(ui, |ui| {
            ui.set_min_width(190.0);
            ui.horizontal(|ui| {
                ui.label(t("Opacity"));
                ui.add(
                    egui::Slider::new(&mut self.opacity, 0.35..=1.0)
                        .show_value(true)
                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
                );
            });
            if ui
                .button(if self.dark_mode {
                    t("Switch to light")
                } else {
                    t("Switch to dark")
                })
                .clicked()
            {
                self.theme_transition_from = Some(shadcn_background(self.dark_mode));
                self.theme_transition_started_at = Some(ui.input(|input| input.time));
                self.dark_mode = !self.dark_mode;
                ui.close();
            }
            ui.separator();
            if ui
                .add(
                    egui::Button::selectable(self.hud_mode, t("Combat HUD"))
                        .frame_when_inactive(true),
                )
                .on_hover_text(t("Backing-less HUD, overlaid directly on the game"))
                .clicked()
            {
                self.set_hud_mode(ui.ctx(), !self.hud_mode);
                ui.close();
            }
        });
        appearance_response.on_hover_text(t("Adjust opacity, theme and HUD mode"));
    }

    pub(crate) fn animated_controls(&mut self, ui: &mut egui::Ui) {
        let expanded = !self.abyss_compact_mode || !self.state.abyss.is_active();
        let progress = ui.ctx().animate_bool_with_time(
            egui::Id::new("main_controls_expanded"),
            expanded,
            0.22,
        );
        if progress <= 0.001 {
            return;
        }

        let full_height = MAIN_CONTROLS_SINGLE_ROW_HEIGHT;
        let content_top_offset = 2.5;
        let visible_height = full_height * ease_out_cubic(progress);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), visible_height),
            egui::Sense::hover(),
        );
        let full_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(2.0, content_top_offset),
            egui::vec2((rect.width() - 4.0).max(0.0), full_height),
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
    }

    pub(crate) fn animated_party_content(&mut self, ui: &mut egui::Ui) {
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
                RichText::new(t("Team"))
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
                        egui::Button::new(t("Team Combat Details")),
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

    pub(crate) fn import_loading_content(&mut self, ui: &mut egui::Ui) {
        let available_rect = ui.available_rect_before_wrap();
        ui.allocate_rect(available_rect, egui::Sense::hover());
        let card_size = egui::vec2(
            360.0_f32.min(available_rect.width()),
            190.0_f32.min(available_rect.height()),
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
        content.add_space((content_rect.height() - 126.0).max(0.0) * 0.5);
        content.add(
            egui::Spinner::new()
                .size(28.0)
                .color(theme_accent(self.dark_mode)),
        );
        content.add_space(8.0);
        content.label(
            RichText::new(t("Importing and parsing capture"))
                .size(15.0)
                .strong()
                .color(shadcn_foreground(self.dark_mode)),
        );
        content.add_space(2.0);
        if let Some(task) = &self.active_import {
            let elapsed = task.started_at.elapsed().as_secs();
            content.label(
                RichText::new(format!(
                    "{} · {} · {}s",
                    t(task.kind.label()),
                    file_display_name(&task.path),
                    elapsed
                ))
                .size(11.0)
                .color(content.visuals().weak_text_color()),
            );
        }
        content.label(
            RichText::new(tf(
                "Parsed {} damage records · {} packets",
                &[
                    &self.state.hits.len().to_string(),
                    &self.state.packets.len().to_string(),
                ],
            ))
            .size(11.0)
            .color(content.visuals().weak_text_color()),
        );
        content.add_space(8.0);
        if content.button(t("Cancel Import")).clicked() {
            self.stop_engine();
            self.status = t("Import canceled");
        }
    }

    pub(crate) fn paint_theme_transition(&mut self, ctx: &egui::Context) {
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
        .rect_filled(ctx.content_rect(), 0.0, overlay);
        ctx.request_repaint();
    }

    /// Party-member rows (damage desc) plus the team totals shared by the party
    /// panel and the HUD. Returns owned rows so callers can paint without holding
    /// a borrow on `state`. Tuple: `(rows, total_damage, team_dps, duration)`.
    /// Character display name for the active UI language, falling back to `fallback`
    /// (the name baked at parse time) when the character table has no usable entry.
    /// Thin wrapper over [`character_display_name`] for `DpsApp` call sites.
    pub(crate) fn localized_character_name(&self, char_id: u32, fallback: &str) -> String {
        character_display_name(&self.characters, char_id, fallback)
    }

    pub(crate) fn party_readout(&self) -> (Vec<CharacterStats>, f64, f64, f64) {
        let prep = |stats: &HashMap<u32, CharacterStats>,
                    hits: &VecDeque<crate::engine::model::Hit>,
                    total: f64,
                    dps: f64,
                    duration: f64| {
            let mut rows: Vec<CharacterStats> = stats
                .values()
                .filter(|row| is_party_member_row(row, hits))
                .cloned()
                .collect();
            rows.sort_by(|left, right| right.damage.total_cmp(&left.damage));
            (rows, total, dps, duration)
        };
        let (mut rows, total, dps, duration) = if let Some(party) = self.selected_party_state() {
            prep(
                &party.stats,
                &party.hits,
                party.total_damage,
                self.party_dps_for_current_mode(party),
                self.party_duration_for_current_mode(party),
            )
        } else {
            prep(
                &self.state.stats,
                &self.state.hits,
                self.state.total_damage,
                self.state_dps_for_current_mode(),
                self.state_duration_for_current_mode(),
            )
        };
        // Resolve each row's display name to the active language; the baked name
        // (Chinese, from parse time) is the fallback for unknown characters.
        for row in &mut rows {
            row.name = self.localized_character_name(row.char_id, &row.name);
        }
        (rows, total, dps, duration)
    }

    /// Cheap count of party-member rows (no clone), for HUD window sizing.
    pub(crate) fn party_member_count(&self) -> usize {
        if let Some(party) = self.selected_party_state() {
            party
                .stats
                .values()
                .filter(|row| is_party_member_row(row, &party.hits))
                .count()
        } else {
            self.state
                .stats
                .values()
                .filter(|row| is_party_member_row(row, &self.state.hits))
                .count()
        }
    }

    pub(crate) fn hud_visible_row_count(&self) -> usize {
        if self.hud_config.show_character_rows {
            self.party_member_count().min(TEAM_DPS_MAX_MEMBERS)
        } else {
            0
        }
    }

    pub(crate) fn hud_status_row_visible(&self) -> bool {
        (self.hud_config.show_abyss_half && self.state.abyss.is_active())
            || self.hud_config.show_passthrough_state
    }

    pub(crate) fn party_panel(&mut self, ui: &mut egui::Ui) {
        let (rows, total_damage, _, _) = self.party_readout();
        let row_height = party_row_height(ui.available_height(), rows.len());
        if rows.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 40.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new(t("Waiting for damage data"))
                            .color(ui.visuals().weak_text_color()),
                    );
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

    /// Combat HUD optimized for quick scanning: one team DPS header and a compact
    /// horizontal damage-share board. Details stay in the normal window.
    pub(crate) fn hud_panel(&mut self, ui: &mut egui::Ui) {
        const ACCENT: Color32 = Color32::from_rgb(44, 214, 150);
        const TEXT: Color32 = Color32::from_rgb(242, 246, 248);
        const MUTED: Color32 = Color32::from_rgb(176, 187, 194);

        let (mut rows, total_damage, team_dps, duration) = self.party_readout();
        if self.hud_config.show_character_rows {
            rows.truncate(TEAM_DPS_MAX_MEMBERS);
        } else {
            rows.clear();
        }
        let area = ui.available_rect_before_wrap();
        let left = area.left();
        let width = area.width().min(HUD_WINDOW_WIDTH - 16.0);
        let right = left + width;
        let painter = ui.painter().clone();
        let colors = HudPaintColors {
            accent: ACCENT,
            text: TEXT,
            muted: MUTED,
        };

        if rows.is_empty() && total_damage <= 0.0 && team_dps <= 0.0 {
            let empty = egui::Rect::from_min_size(
                egui::pos2(left, area.top()),
                egui::vec2(width.min(210.0), 38.0),
            );
            paint_haloed(
                &painter,
                egui::pos2(empty.left(), empty.center().y),
                egui::Align2::LEFT_CENTER,
                t("Waiting for damage data"),
                egui::FontId::proportional(13.0),
                MUTED,
            );
            ui.allocate_rect(empty, egui::Sense::hover());
            return;
        }

        let mut top = area.top();
        if self.hud_config.show_title {
            top += self.hud_title_readout_row(&painter, left, top, width, TEXT, MUTED);
        }
        if self.hud_config.has_summary_row() {
            top += self.hud_summary_row(
                &painter,
                egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, 50.0)),
                HudSummaryValues {
                    total_damage,
                    team_dps,
                    duration,
                    damage_taken: self.current_damage_taken_for_hud(),
                },
                &rows,
                colors,
            );
        }
        if self.hud_status_row_visible() {
            top += self.hud_status_row(&painter, left, top, width, TEXT, MUTED);
        }
        if !rows.is_empty() {
            top += self.hud_character_rows(
                ui,
                &painter,
                HudRowsLayout {
                    left,
                    right,
                    top,
                    width,
                },
                &rows,
                total_damage,
            );
        }
        if self.hud_config.show_mini_timeline {
            top += self.hud_mini_timeline(&painter, left, top, width, ACCENT, MUTED);
        }

        ui.allocate_rect(
            egui::Rect::from_min_max(area.min, egui::pos2(right, top)),
            egui::Sense::hover(),
        );
    }

    pub(crate) fn hud_title_readout_row(
        &self,
        painter: &egui::Painter,
        left: f32,
        top: f32,
        width: f32,
        text: Color32,
        muted: Color32,
    ) -> f32 {
        let rect = egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, 20.0));
        paint_haloed(
            painter,
            egui::pos2(rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            "NTE DPS",
            egui::FontId::proportional(12.0),
            text,
        );
        paint_haloed(
            painter,
            egui::pos2(rect.right(), rect.center().y),
            egui::Align2::RIGHT_CENTER,
            t(self.dps_time_mode.label()),
            egui::FontId::proportional(10.5),
            muted,
        );
        22.0
    }

    pub(crate) fn hud_summary_row(
        &self,
        painter: &egui::Painter,
        header: egui::Rect,
        values: HudSummaryValues,
        rows: &[CharacterStats],
        colors: HudPaintColors,
    ) -> f32 {
        let track_color = Color32::from_black_alpha(96);
        if self.hud_config.show_team_dps {
            let label = if self.hud_config.show_duration {
                format!("{} · {:.1}s", t("Team DPS"), values.duration)
            } else {
                t("Team DPS")
            };
            paint_haloed(
                painter,
                egui::pos2(header.left(), header.top() + 12.0),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(10.5),
                colors.muted,
            );
            paint_haloed(
                painter,
                egui::pos2(header.left(), header.bottom() - 17.0),
                egui::Align2::LEFT_CENTER,
                format_number(values.team_dps),
                egui::FontId::proportional(26.0),
                colors.accent,
            );
        } else if self.hud_config.show_duration {
            paint_haloed(
                painter,
                egui::pos2(header.left(), header.center().y),
                egui::Align2::LEFT_CENTER,
                format!("{} {:.1}s", t("Time"), values.duration),
                egui::FontId::monospace(14.0),
                colors.text,
            );
        }

        let right_label = match (
            self.hud_config.show_total_damage,
            self.hud_config.show_damage_taken,
        ) {
            (true, true) => Some((
                t("Total Damage / Taken"),
                format!(
                    "{} / {}",
                    format_number(values.total_damage),
                    format_number(values.damage_taken)
                ),
            )),
            (true, false) => Some((t("Total Damage"), format_number(values.total_damage))),
            (false, true) => Some((t("Total Damage Taken"), format_number(values.damage_taken))),
            (false, false) => None,
        };
        if let Some((label, value)) = right_label {
            paint_haloed(
                painter,
                egui::pos2(header.right(), header.top() + 12.0),
                egui::Align2::RIGHT_CENTER,
                label,
                egui::FontId::proportional(10.5),
                colors.muted,
            );
            paint_haloed(
                painter,
                egui::pos2(header.right(), header.bottom() - 17.0),
                egui::Align2::RIGHT_CENTER,
                value,
                egui::FontId::monospace(13.0),
                colors.text,
            );
        }

        let share_strip = egui::Rect::from_min_size(
            egui::pos2(header.left(), header.bottom() - 3.0),
            egui::vec2(header.width(), 2.0),
        );
        painter.rect_filled(share_strip, 1.0, track_color);
        if self.hud_config.show_character_rows && values.total_damage > 0.0 {
            let mut seg_left = share_strip.left();
            for (index, row) in rows.iter().enumerate() {
                let frac = (row.damage / values.total_damage) as f32;
                let seg_width = share_strip.width() * frac;
                if seg_width <= 0.5 {
                    continue;
                }
                let seg = egui::Rect::from_min_size(
                    egui::pos2(seg_left, share_strip.top()),
                    egui::vec2(seg_width, share_strip.height()),
                );
                painter.rect_filled(
                    seg,
                    1.0,
                    character_color(row.char_id, &self.characters, index),
                );
                seg_left += seg_width;
            }
        }
        56.0
    }

    pub(crate) fn hud_status_row(
        &self,
        painter: &egui::Painter,
        left: f32,
        top: f32,
        width: f32,
        text: Color32,
        muted: Color32,
    ) -> f32 {
        let rect = egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, 18.0));
        if self.hud_config.show_abyss_half && self.state.abyss.is_active() {
            paint_haloed(
                painter,
                egui::pos2(rect.left(), rect.center().y),
                egui::Align2::LEFT_CENTER,
                t(self.selected_abyss_half.label()),
                egui::FontId::proportional(11.0),
                text,
            );
        }
        if self.hud_config.show_passthrough_state {
            let label = if self.mouse_passthrough {
                t("Passthrough")
            } else {
                t("Edit")
            };
            paint_haloed(
                painter,
                egui::pos2(rect.right(), rect.center().y),
                egui::Align2::RIGHT_CENTER,
                label,
                egui::FontId::proportional(11.0),
                muted,
            );
        }
        22.0
    }

    pub(crate) fn hud_character_rows(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        layout: HudRowsLayout,
        rows: &[CharacterStats],
        total_damage: f64,
    ) -> f32 {
        const TEXT: Color32 = Color32::from_rgb(242, 246, 248);
        let track_color = Color32::from_black_alpha(96);
        let row_h = 24.0;
        let row_gap = 4.0;
        let mut row_top = layout.top;
        for (index, row) in rows.iter().enumerate() {
            let color = character_color(row.char_id, &self.characters, index);
            let row_dps = self.character_dps_for_current_source(row);
            let row_rect = egui::Rect::from_min_size(
                egui::pos2(layout.left, row_top),
                egui::vec2(layout.width, row_h),
            );
            let center_y = row_rect.center().y;
            painter.rect_filled(
                egui::Rect::from_min_size(row_rect.min, egui::vec2(3.0, row_h)),
                1.5,
                color,
            );

            let avatar = egui::Rect::from_min_size(
                egui::pos2(row_rect.left() + 8.0, center_y - 9.0),
                egui::vec2(18.0, 18.0),
            );
            self.draw_hud_avatar(ui, avatar, row.char_id, color, &row.name);

            let name_x = avatar.right() + 7.0;
            let bar_left = row_rect.left() + 104.0;
            let bar_right = row_rect.right() - 96.0;
            let clipped = painter.with_clip_rect(egui::Rect::from_min_max(
                egui::pos2(name_x, row_rect.top()),
                egui::pos2(bar_left - 7.0, row_rect.bottom()),
            ));
            paint_haloed(
                &clipped,
                egui::pos2(name_x, center_y),
                egui::Align2::LEFT_CENTER,
                &row.name,
                egui::FontId::proportional(12.0),
                TEXT,
            );

            let share = if total_damage > 0.0 {
                row.damage / total_damage * 100.0
            } else {
                0.0
            };
            let track = egui::Rect::from_min_max(
                egui::pos2(bar_left, center_y - 3.5),
                egui::pos2(bar_right.max(bar_left + 8.0), center_y + 3.5),
            );
            painter.rect_filled(track, 3.5, track_color);
            let fill_width = if total_damage > 0.0 {
                track.width() * (share as f32 / 100.0)
            } else {
                0.0
            };
            if fill_width > 0.5 {
                painter.rect_filled(
                    egui::Rect::from_min_size(track.min, egui::vec2(fill_width, track.height())),
                    3.5,
                    color,
                );
            }
            paint_haloed(
                painter,
                egui::pos2(row_rect.right() - 8.0, center_y),
                egui::Align2::RIGHT_CENTER,
                format!("{share:.1}%"),
                egui::FontId::proportional(10.5),
                color,
            );
            paint_haloed(
                painter,
                egui::pos2(layout.right - 52.0, center_y),
                egui::Align2::RIGHT_CENTER,
                format_number(row_dps),
                egui::FontId::monospace(12.0),
                TEXT,
            );
            row_top += row_h + row_gap;
        }
        row_top - layout.top
    }

    pub(crate) fn hud_mini_timeline(
        &mut self,
        painter: &egui::Painter,
        left: f32,
        top: f32,
        width: f32,
        accent: Color32,
        muted: Color32,
    ) -> f32 {
        let rect = egui::Rect::from_min_size(egui::pos2(left, top + 4.0), egui::vec2(width, 34.0));
        let baseline_y = rect.bottom() - 5.0;
        painter.hline(
            rect.x_range(),
            baseline_y,
            Stroke::new(1.0, Color32::from_black_alpha(110)),
        );
        let timeline = self.cached_timeline_series();
        let peak = timeline
            .buckets
            .iter()
            .map(|bucket| bucket.dps)
            .fold(0.0, f64::max);
        if peak > 0.0 && timeline.buckets.len() > 1 {
            let duration = timeline
                .buckets
                .last()
                .map_or(1.0, |bucket| bucket.end_offset)
                .max(0.001);
            let mut previous = None;
            for bucket in &timeline.buckets {
                let x = rect.left() + rect.width() * (bucket.end_offset / duration) as f32;
                let y = baseline_y - (rect.height() - 8.0) * (bucket.dps / peak) as f32;
                let point = egui::pos2(x, y);
                if let Some(previous) = previous {
                    painter.line_segment([previous, point], Stroke::new(1.4, accent));
                }
                previous = Some(point);
            }
        }
        paint_haloed(
            painter,
            egui::pos2(rect.left(), rect.top() + 6.0),
            egui::Align2::LEFT_CENTER,
            "DPS",
            egui::FontId::proportional(9.5),
            muted,
        );
        42.0
    }

    pub(crate) fn current_damage_taken_for_hud(&self) -> f64 {
        self.selected_party_state()
            .map_or(self.state.total_damage_taken, |party| {
                party.total_damage_taken
            })
    }

    /// Square avatar for the HUD (image, else a colored tile with the initial),
    /// with a dark edge so it separates from a bright game scene.
    pub(crate) fn draw_hud_avatar(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        char_id: u32,
        color: Color32,
        name: &str,
    ) {
        let radius_f = rect.width() * 0.24;
        let texture = self
            .characters
            .get(&char_id)
            .and_then(|character| character.avatar.as_deref())
            .and_then(|avatar| self.avatar_textures.get(avatar));
        if let Some(texture) = texture {
            ui.put(
                rect,
                egui::Image::new((texture.id(), rect.size())).corner_radius(radius_f as u8),
            );
        } else {
            ui.painter()
                .rect_filled(rect, radius_f, color.gamma_multiply(0.85));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                name.chars().next().unwrap_or('?').to_string(),
                egui::FontId::proportional(rect.height() * 0.52),
                contrast_text(color),
            );
        }
        ui.painter().rect_stroke(
            rect,
            radius_f,
            Stroke::new(1.0, Color32::from_black_alpha(150)),
            egui::StrokeKind::Inside,
        );
    }

    pub(crate) fn draw_party_row(
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
        let duration = self.character_duration_for_current_source(row);
        let dps = self.character_dps_for_current_source(row);
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
        // Grows with the (now height-adaptive) row so tall cards don't leave the
        // avatar marooned in empty space; normal-height rows keep the 40px avatar.
        let avatar_size = (row_height - 8.0).clamp(32.0, 56.0);
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
            tf(
                "{} hits · {}",
                &[&row.hits.to_string(), &format!("{duration:.1}s")],
            ),
            egui::FontId::monospace(10.5),
            ui.visuals().weak_text_color(),
        );
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y - 8.0),
            egui::Align2::RIGHT_CENTER,
            format!("{} DPS", format_number(dps)),
            egui::FontId::monospace(12.0),
            theme_accent(self.dark_mode),
        );
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y + 9.0),
            egui::Align2::RIGHT_CENTER,
            tf(
                "Damage {} · Share {}% · Taken {}",
                &[
                    &format_number(row.damage),
                    &format!("{share:.1}"),
                    &format_number(row.damage_taken),
                ],
            ),
            egui::FontId::monospace(10.5),
            ui.visuals().weak_text_color(),
        );
        if response
            .on_hover_text(t("View combat details in a separate window"))
            .clicked()
        {
            self.hit_detail_char_id = Some(row.char_id);
            self.hit_detail_filter = HitDetailFilter::All;
            self.hit_detail_skill_filter.clear();
            self.hit_detail_corner_applied = false;
        }
    }
}
