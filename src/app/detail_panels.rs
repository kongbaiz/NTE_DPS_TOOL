use super::*;

impl DpsApp {
    pub(crate) fn character_hits(
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
                        RichText::new(t("No hit records under the current filter"))
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

    pub(crate) fn team_hits(&mut self, ui: &mut egui::Ui, filter: HitDetailFilter) {
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
                        RichText::new(t("No hit records under the current filter"))
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
                    let char_name = self.localized_character_name(hit.char_id, &hit.char_name);
                    draw_team_hit_row(
                        ui,
                        layout,
                        hit,
                        max_damage,
                        color,
                        TeamHitRowAssets {
                            char_name: &char_name,
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

    pub(crate) fn team_hit_detail_panel(&mut self, ctx: &egui::Context) {
        let viewport_id = team_hit_detail_viewport_id();
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
                    self.party_duration_for_current_mode(party),
                    self.party_dps_for_current_mode(party),
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
                    self.state_duration_for_current_mode(),
                    self.state_dps_for_current_mode(),
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
            tf(
                "Team Combat Details - {}",
                &[&t(self.selected_abyss_half.label())],
            )
        } else {
            t("Team Combat Details")
        };
        let close_requested = ctx.show_viewport_immediate(
            viewport_id,
            secondary_viewport_builder(
                &title,
                self.team_hit_detail_window_size,
                config::TEAM_HIT_DETAIL_WINDOW_MIN_SIZE,
                self.team_hit_detail_corner_applied,
            ),
            |ctx, _class| {
                if !self.team_hit_detail_corner_applied {
                    apply_rounding_to_process_windows();
                    self.team_hit_detail_corner_applied = true;
                }
                let close_clicked = secondary_title_panel(ctx, &title);
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show_inside(ctx, |ui| {
                        egui::Frame::new()
                            .fill(shadcn_card(self.dark_mode))
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .corner_radius(10)
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                let text_color = ui.visuals().text_color();
                                let (label_out, label_count, label_taken, label_time) = (
                                    t("Total Output"),
                                    t("Output Count"),
                                    t("Total Damage Taken"),
                                    t("Combat Time"),
                                );
                                draw_hit_metric_row(
                                    ui,
                                    [
                                        (
                                            label_out.as_str(),
                                            format_number(total_damage),
                                            theme_accent(self.dark_mode),
                                        ),
                                        ("DPS", format_number(dps), theme_accent(self.dark_mode)),
                                        (
                                            label_count.as_str(),
                                            outgoing_count.to_string(),
                                            text_color,
                                        ),
                                        (
                                            label_taken.as_str(),
                                            format_number(total_damage_taken),
                                            semantic_danger(self.dark_mode),
                                        ),
                                        (
                                            label_time.as_str(),
                                            format!("{duration:.1}s"),
                                            text_color,
                                        ),
                                    ],
                                );
                                draw_direction_summary(ui, direction_summary);
                            });
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().interact_size.y = 28.0;
                            ui.spacing_mut().button_padding.y = 4.0;
                            ui.add(
                                egui::Label::new(
                                    RichText::new(t("Hit Type"))
                                        .strong()
                                        .color(ui.visuals().weak_text_color()),
                                )
                                .selectable(false),
                            );
                            stable_selectable_value(
                                ui,
                                &mut self.team_hit_detail_filter,
                                HitDetailFilter::All,
                                tf("All {}", &[&(outgoing_count + incoming_count).to_string()]),
                            );
                            stable_selectable_value(
                                ui,
                                &mut self.team_hit_detail_filter,
                                HitDetailFilter::Outgoing,
                                tf("Outgoing {}", &[&outgoing_count.to_string()]),
                            );
                            stable_selectable_value(
                                ui,
                                &mut self.team_hit_detail_filter,
                                HitDetailFilter::Incoming,
                                tf("Taken {}", &[&incoming_count.to_string()]),
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
                track_window_size(ctx, &mut self.team_hit_detail_window_size);
                window_resize_grips(ctx);
                self.show_viewport_dialogs(ctx);
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.team_hit_detail_open = false;
            self.team_hit_detail_corner_applied = false;
            self.retarget_dialogs(viewport_id, egui::ViewportId::ROOT);
        }
    }

    pub(crate) fn abyss_overview_panel(&mut self, ctx: &egui::Context) {
        let viewport_id = abyss_overview_viewport_id();
        let close_requested = ctx.show_viewport_immediate(
            viewport_id,
            secondary_viewport_builder(
                t("Abyss Monster Stats"),
                self.abyss_window_size,
                config::ABYSS_WINDOW_MIN_SIZE,
                self.abyss_overview_corner_applied,
            ),
            |ctx, _class| {
                if !self.abyss_overview_corner_applied {
                    apply_rounding_to_process_windows();
                    self.abyss_overview_corner_applied = true;
                }
                let close_clicked = secondary_title_panel(ctx, &t("Abyss Monster Stats"));
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show_inside(ctx, |ui| {
                        self.abyss_overview_contents(ui);
                    });
                track_window_size(ctx, &mut self.abyss_window_size);
                window_resize_grips(ctx);
                self.show_viewport_dialogs(ctx);
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.abyss_overview_open = false;
            self.abyss_overview_corner_applied = false;
            self.retarget_dialogs(viewport_id, egui::ViewportId::ROOT);
        }
    }

    pub(crate) fn abyss_overview_contents(&mut self, ui: &mut egui::Ui) {
        self.abyss_overview.ensure_selection();
        let Some(dataset) = self.abyss_overview.dataset.as_ref() else {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), ui.available_height()),
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.label(
                        RichText::new(t("Abyss monster stats table not loaded"))
                            .size(18.0)
                            .strong()
                            .color(semantic_danger(self.dark_mode)),
                    );
                    if let Some(error) = &self.abyss_overview.load_error {
                        ui.add_space(6.0);
                        ui.label(RichText::new(error).color(ui.visuals().weak_text_color()));
                    }
                    ui.add_space(10.0);
                    if ui.button(t("Reload")).clicked() {
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
                    season.name.clone(),
                    season
                        .floors
                        .iter()
                        .map(|floor| {
                            (
                                floor.floor,
                                floor.name.clone(),
                                floor.monster_count(),
                                floor.wave_count(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let floor_count = season_nav
            .iter()
            .map(|(_, _, floors)| floors.len())
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
            .map(AbyssFloor::monster_count)
            .sum::<u32>();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(tf(
                    "{} seasons · {} floors · {} abyss enemies",
                    &[
                        &season_count.to_string(),
                        &floor_count.to_string(),
                        &total_monsters.to_string(),
                    ],
                ))
                .size(12.0)
                .strong()
                .color(shadcn_foreground(self.dark_mode)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(t("Reload")).clicked() {
                    self.abyss_overview.reload();
                }
            });
        });
        ui.add_space(8.0);

        let available = ui.available_size();
        let (main_rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
        // Wide enough for a floor name plus the "{n} monsters · {n} waves" count in
        // either language; capped so it never eats the detail pane on small windows.
        let nav_width = (main_rect.width() * 0.30).clamp(180.0, 240.0);
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
                RichText::new(t("Select an abyss site"))
                    .color(content_ui.visuals().weak_text_color()),
            );
            return;
        };
        self.abyss_floor_contents(&mut content_ui, &floor, selected_monster.as_ref());
    }

    /// Snapshot the current global combat session into a prediction team. Returns `None`
    /// when there is no measured output yet (DPS is zero), so callers can keep the
    /// "import data first" prompt.
    /// Snapshot the team for one prediction line. During an abyss run each line's
    /// characters live in their own half (上行线 = first, 下行线 = second), so we
    /// must read from that half — the global `state` aggregates both lines and
    /// would hand back one merged team for either line. Outside the abyss (大世界)
    /// there is only the global state.
    pub(crate) fn snapshot_current_team(&self, upper: bool) -> Option<TeamDps> {
        if self.state.abyss.is_active() {
            let half = if upper {
                AbyssHalf::First
            } else {
                AbyssHalf::Second
            };
            let party = self.state.abyss.half(half);
            snapshot_team_from_stats(
                party.dps_with_time_stop(self.subtract_time_stop_for_dps()),
                party.duration_with_time_stop(self.subtract_time_stop_for_dps()),
                party.stats.values(),
            )
        } else {
            snapshot_team_from_stats(
                self.state_dps_for_current_mode(),
                self.state_duration_for_current_mode(),
                self.state.stats.values(),
            )
        }
    }

    pub(crate) fn apply_line_prediction_action(
        &mut self,
        ctx: &egui::Context,
        upper: bool,
        action: LinePredictionAction,
    ) {
        let team = match action {
            LinePredictionAction::None => return,
            LinePredictionAction::ImportCurrent => self.snapshot_current_team(upper),
            LinePredictionAction::Clear => None,
            LinePredictionAction::ImportFile => {
                // The dialog runs on a worker thread; the file is applied in
                // finish_team_dps_line_import once it comes back.
                self.request_team_dps_file(ctx, FileDialogPurpose::TeamDpsImportLine { upper });
                return;
            }
        };
        if upper {
            self.abyss_overview.upper_team = team;
        } else {
            self.abyss_overview.lower_team = team;
        }
    }

    /// Ask for a team DPS data file; the parse/apply happens in
    /// [`Self::poll_file_dialog`] once the worker-thread dialog returns.
    fn request_team_dps_file(&mut self, ctx: &egui::Context, purpose: FileDialogPurpose) {
        let filter = t("NTE team data");
        self.spawn_file_dialog(ctx, purpose, move |owner| {
            with_owner(rfd::FileDialog::new().add_filter(filter, &["json"]), owner).pick_file()
        });
    }

    fn load_team_dps_file(path: &Path) -> Result<TeamDpsExport, String> {
        std::fs::read_to_string(path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<TeamDpsExport>(&text).map_err(|error| error.to_string())
            })
    }

    /// Completion of the per-line 预测导入: prefer that line's team from the
    /// file, then the matching upper/lower, then the single team.
    pub(crate) fn finish_team_dps_line_import(
        &mut self,
        viewport: egui::ViewportId,
        upper: bool,
        path: &Path,
    ) {
        let export = match Self::load_team_dps_file(path) {
            Ok(export) => export,
            Err(error) => {
                self.set_last_error_for(
                    viewport,
                    tf("Failed to import DPS data: {}", &[&error]),
                    None,
                );
                return;
            }
        };
        let preferred = if upper { export.upper } else { export.lower };
        let Some(team) = preferred.or(export.single) else {
            self.set_last_error_for(
                viewport,
                t("The DPS data file has no team usable for this line"),
                Some(ErrorAction::OpenTeamDpsImport),
            );
            return;
        };
        if upper {
            self.abyss_overview.upper_team = Some(team);
        } else {
            self.abyss_overview.lower_team = Some(team);
        }
    }

    /// Main-window "导入 DPS 数据": load a team DPS file into the abyss prediction.
    /// A dual file fills 上行线/下行线 separately; a single-team file fills both.
    pub(crate) fn import_team_dps(&mut self, ctx: &egui::Context) {
        self.request_team_dps_file(ctx, FileDialogPurpose::TeamDpsImportAll);
    }

    pub(crate) fn finish_team_dps_import(&mut self, viewport: egui::ViewportId, path: &Path) {
        let export = match Self::load_team_dps_file(path) {
            Ok(export) => export,
            Err(error) => {
                self.set_last_error_for(
                    viewport,
                    tf("Failed to import DPS data: {}", &[&error]),
                    Some(ErrorAction::OpenTeamDpsImport),
                );
                return;
            }
        };
        let upper = export.upper.clone().or_else(|| export.single.clone());
        let lower = export.lower.clone().or_else(|| export.single.clone());
        if upper.is_none() && lower.is_none() {
            self.set_last_error_for(
                viewport,
                t("The DPS data file has no team data"),
                Some(ErrorAction::OpenTeamDpsImport),
            );
            return;
        }
        if upper.is_some() {
            self.abyss_overview.upper_team = upper;
        }
        if lower.is_some() {
            self.abyss_overview.lower_team = lower;
        }
        self.status = t("Imported DPS team data");
        self.clear_last_error();
    }

    /// Main-window "导出队伍数据": write a compact JSON with the current session as
    /// `single` and real-time abyss halves as `upper`/`lower` when available.
    /// Prediction-panel teams are kept as a fallback for manually imported data.
    /// No packets or per-hit data, latest DPS only, serialized without indentation.
    pub(crate) fn export_team_dps(&mut self, ctx: &egui::Context) {
        let Some(export) = build_team_dps_export(
            &self.state,
            &self.abyss_overview,
            self.subtract_time_stop_for_dps(),
        ) else {
            self.set_last_error_in(
                ctx,
                t("No team data to export; capture first or set the abyss teams"),
                None,
            );
            return;
        };
        let Ok(json) = serde_json::to_string(&export) else {
            self.set_last_error_in(ctx, t("Failed to serialize team data"), None);
            return;
        };
        let default_name = format!("nte_team_dps_{}.json", Local::now().format("%Y%m%d_%H%M%S"));
        let filter = t("NTE team data");
        self.spawn_file_dialog(
            ctx,
            FileDialogPurpose::TeamDpsExport { json },
            move |owner| {
                with_owner(
                    rfd::FileDialog::new()
                        .add_filter(filter, &["json"])
                        .set_file_name(default_name),
                    owner,
                )
                .save_file()
            },
        );
    }

    pub(crate) fn finish_team_dps_export(
        &mut self,
        viewport: egui::ViewportId,
        path: &Path,
        json: &str,
    ) {
        match atomic_write_text(path, json) {
            Ok(()) => {
                self.status = t("Team data exported");
                self.clear_last_error();
            }
            Err(error) => self.set_last_error_for(
                viewport,
                tf("Failed to export team data: {}", &[&error.to_string()]),
                None,
            ),
        }
    }

    pub(crate) fn save_current_history_summary(&mut self, ctx: &egui::Context) {
        let Some(summary) = self.state.session_summary(
            self.capture_quality_source,
            self.dps_time_mode.label(),
            self.subtract_time_stop_for_dps(),
        ) else {
            self.set_last_error_in(
                ctx,
                t("No combat summary to save; capture first or import a replay"),
                None,
            );
            return;
        };
        match history::save_summary(summary) {
            Ok(record) => {
                self.history.reload();
                self.history.selected_id = Some(record.id);
                self.history.ensure_selection();
                self.history.message = t("This summary saved");
                self.status = t("History summary saved");
                self.clear_last_error();
            }
            Err(error) => {
                self.set_last_error_in(
                    ctx,
                    tf("Failed to save history summary: {}", &[&error.to_string()]),
                    None,
                );
            }
        }
    }

    pub(crate) fn delete_history_record_for(
        &mut self,
        record_id: String,
        viewport: egui::ViewportId,
    ) {
        match history::delete_record(&record_id) {
            Ok(true) => {
                self.history.reload();
                self.history.message = t("History summary deleted");
                self.status = t("History summary deleted");
                self.clear_last_error();
            }
            Ok(false) => {
                self.set_last_error_for(viewport, t("No history summary found to delete"), None);
            }
            Err(error) => {
                self.set_last_error_for(
                    viewport,
                    tf(
                        "Failed to delete history summary: {}",
                        &[&error.to_string()],
                    ),
                    None,
                );
            }
        }
    }

    pub(crate) fn abyss_floor_contents(
        &mut self,
        ui: &mut egui::Ui,
        floor: &AbyssFloor,
        selected_monster: Option<&AbyssMonsterEntry>,
    ) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} · {}",
                    abyss_season_label(floor.season, floor.season_name.as_deref()),
                    abyss_floor_label(floor)
                ))
                .size(16.0)
                .strong()
                .color(shadcn_foreground(self.dark_mode)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.abyss_overview.search)
                        .hint_text(t("Search monster / ID"))
                        .desired_width(190.0),
                );
                let has_team = self.abyss_overview.upper_team.is_some()
                    || self.abyss_overview.lower_team.is_some();
                if ui
                    .add_enabled(has_team, egui::Button::new(t("Swap Teams")))
                    .on_hover_text(t(
                        "Swap the ascending-line and descending-line prediction teams",
                    ))
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
                    || monster.attribute_id.to_ascii_lowercase().contains(&query)
                    || monster.monster_id.to_ascii_lowercase().contains(&query)
                    || monster
                        .monster_pool_id
                        .as_deref()
                        .is_some_and(|pool| pool.to_ascii_lowercase().contains(&query))
            })
            .collect::<Vec<_>>();
        if monsters.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 92.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new(t("No matching monster"))
                            .color(ui.visuals().weak_text_color()),
                    );
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
            let mut upper_target_seconds = self.abyss_overview.upper_target_seconds;
            let mut lower_target_seconds = self.abyss_overview.lower_target_seconds;
            let can_import = self.state_dps_for_current_mode() > 0.0;
            let mut upper_action = LinePredictionAction::None;
            let mut lower_action = LinePredictionAction::None;
            if !upper_line.is_empty() || !lower_line.is_empty() {
                let upper_result = draw_abyss_line_section(
                    ui,
                    &t("Ascending Line"),
                    &upper_line,
                    selected_pack_id.as_deref(),
                    &mut self.abyss_overview.selected_monster_pack_id,
                    &self.monster_textures,
                    self.dark_mode,
                    Some(LinePredictionView {
                        team: upper_team.as_ref(),
                        line_hp: abyss_line_hp_total(upper_line.iter().copied()),
                        target_seconds: upper_target_seconds,
                        can_import,
                        avatar_textures: &self.avatar_textures,
                        characters: &self.characters,
                    }),
                );
                upper_action = upper_result.action;
                upper_target_seconds = upper_result.target_seconds;
                ui.add_space(6.0);
                let lower_result = draw_abyss_line_section(
                    ui,
                    &t("Descending Line"),
                    &lower_line,
                    selected_pack_id.as_deref(),
                    &mut self.abyss_overview.selected_monster_pack_id,
                    &self.monster_textures,
                    self.dark_mode,
                    Some(LinePredictionView {
                        team: lower_team.as_ref(),
                        line_hp: abyss_line_hp_total(lower_line.iter().copied()),
                        target_seconds: lower_target_seconds,
                        can_import,
                        avatar_textures: &self.avatar_textures,
                        characters: &self.characters,
                    }),
                );
                lower_action = lower_result.action;
                lower_target_seconds = lower_result.target_seconds;
            }
            self.abyss_overview.upper_target_seconds = upper_target_seconds;
            self.abyss_overview.lower_target_seconds = lower_target_seconds;
            if !unassigned.is_empty() {
                if !upper_line.is_empty() || !lower_line.is_empty() {
                    ui.add_space(6.0);
                }
                draw_abyss_line_section(
                    ui,
                    &t("Full floor config"),
                    &unassigned,
                    selected_pack_id.as_deref(),
                    &mut self.abyss_overview.selected_monster_pack_id,
                    &self.monster_textures,
                    self.dark_mode,
                    None,
                );
            }
            let ctx = ui.ctx().clone();
            self.apply_line_prediction_action(&ctx, true, upper_action);
            self.apply_line_prediction_action(&ctx, false, lower_action);
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
                        RichText::new(t("Click a monster card to see all stat fields"))
                            .color(ui.visuals().weak_text_color()),
                    );
                },
            );
        }
    }

    pub(crate) fn hit_detail_panel(&mut self, ctx: &egui::Context, char_id: u32) {
        let viewport_id = hit_detail_viewport_id();
        let stats = if let Some(party) = self.selected_party_state() {
            party.stats.get(&char_id).cloned()
        } else {
            self.state.stats.get(&char_id).cloned()
        };
        let Some(mut stats) = stats else {
            self.hit_detail_char_id = None;
            self.hit_detail_corner_applied = false;
            return;
        };
        // Display the character's name in the active UI language (window title + header).
        stats.name = self.localized_character_name(char_id, &stats.name);
        let stats_duration = self.character_duration_for_current_source(&stats);
        let stats_dps = self.character_dps_for_current_source(&stats);
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
        let title = tf("{} - Combat Details", &[&stats.name]);
        let close_requested = ctx.show_viewport_immediate(
            viewport_id,
            secondary_viewport_builder(
                &title,
                self.hit_detail_window_size,
                config::HIT_DETAIL_WINDOW_MIN_SIZE,
                self.hit_detail_corner_applied,
            ),
            |ctx, _class| {
                if !self.hit_detail_corner_applied {
                    apply_rounding_to_process_windows();
                    self.hit_detail_corner_applied = true;
                }
                let close_clicked = secondary_title_panel(ctx, &title);
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show_inside(ctx, |ui| {
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
                                                        RichText::new(tf(
                                                            "Character ID {}",
                                                            &[&char_id.to_string()],
                                                        ))
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
                                                let (
                                                    label_out,
                                                    label_count,
                                                    label_taken,
                                                    label_time,
                                                ) = (
                                                    t("Total Output"),
                                                    t("Output Count"),
                                                    t("Total Damage Taken"),
                                                    t("Combat Time"),
                                                );
                                                draw_hit_metric_row(
                                                    ui,
                                                    [
                                                        (
                                                            label_out.as_str(),
                                                            format_number(stats.damage),
                                                            theme_accent(self.dark_mode),
                                                        ),
                                                        (
                                                            "DPS",
                                                            format_number(stats_dps),
                                                            theme_accent(self.dark_mode),
                                                        ),
                                                        (
                                                            label_count.as_str(),
                                                            outgoing_count.to_string(),
                                                            text_color,
                                                        ),
                                                        (
                                                            label_taken.as_str(),
                                                            format_number(stats.damage_taken),
                                                            semantic_danger(self.dark_mode),
                                                        ),
                                                        (
                                                            label_time.as_str(),
                                                            format!("{stats_duration:.1}s"),
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
                            ui.add(
                                egui::Label::new(
                                    RichText::new(t("Damage Type"))
                                        .strong()
                                        .color(ui.visuals().weak_text_color()),
                                )
                                .selectable(false),
                            );
                            stable_selectable_value(
                                ui,
                                &mut self.hit_detail_filter,
                                HitDetailFilter::All,
                                tf("All {}", &[&(outgoing_count + incoming_count).to_string()]),
                            );
                            stable_selectable_value(
                                ui,
                                &mut self.hit_detail_filter,
                                HitDetailFilter::Outgoing,
                                tf("Outgoing {}", &[&outgoing_count.to_string()]),
                            );
                            stable_selectable_value(
                                ui,
                                &mut self.hit_detail_filter,
                                HitDetailFilter::Incoming,
                                tf("Taken {}", &[&incoming_count.to_string()]),
                            );
                            ui.separator();
                            ui.add(
                                egui::Label::new(
                                    RichText::new(t("Specific Move"))
                                        .strong()
                                        .color(ui.visuals().weak_text_color()),
                                )
                                .selectable(false),
                            );
                            ui.scope(|ui| {
                                ui.spacing_mut().interact_size.y = 27.0;
                                ui.spacing_mut().button_padding.y = 2.0;
                                egui::ComboBox::from_id_salt(("hit_skill_filter", char_id))
                                    .width(240.0)
                                    .selected_text(if self.hit_detail_skill_filter.is_empty() {
                                        t("All moves")
                                    } else {
                                        self.hit_detail_skill_filter.clone()
                                    })
                                    .show_ui(ui, |ui| {
                                        stable_popup_selectable_value(
                                            ui,
                                            &mut self.hit_detail_skill_filter,
                                            String::new(),
                                            t("All moves"),
                                        );
                                        for summary in &skill_summaries {
                                            stable_popup_selectable_value(
                                                ui,
                                                &mut self.hit_detail_skill_filter,
                                                summary.name.clone(),
                                                format!(
                                                    "{}  {}",
                                                    summary.name,
                                                    tf("{} hits", &[&summary.hits.to_string()])
                                                ),
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
                track_window_size(ctx, &mut self.hit_detail_window_size);
                window_resize_grips(ctx);
                self.show_viewport_dialogs(ctx);
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.hit_detail_char_id = None;
            self.hit_detail_corner_applied = false;
            self.retarget_dialogs(viewport_id, egui::ViewportId::ROOT);
        }
    }
}
