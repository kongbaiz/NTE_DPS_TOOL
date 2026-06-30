use super::*;

impl DpsApp {
    pub(crate) fn console_panel(&mut self, ctx: &egui::Context) {
        let viewport_id = console_viewport_id();
        let close_requested = ctx.show_viewport_immediate(
            viewport_id,
            egui::ViewportBuilder::default()
                .with_title("NTE 控制台")
                .with_inner_size(scaled_window_size(
                    CONSOLE_WINDOW_BASE_SIZE,
                    self.console_window_scale,
                ))
                .with_window_level(egui::WindowLevel::AlwaysOnTop)
                .with_decorations(false)
                // Not transparent and not resizable on purpose — see
                // window_scale_stepper for the Windows resize-crash rationale.
                .with_resizable(false),
            |ctx, _class| {
                if !self.console_corner_applied {
                    apply_rounding_to_process_windows();
                    self.console_corner_applied = true;
                }
                let mut close_clicked = false;
                egui::Panel::top("console_title_bar")
                    .exact_size(34.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ctx.style().visuals.panel_fill)
                            .stroke(Stroke::new(1.0, shadcn_border(self.dark_mode)))
                            .inner_margin(egui::Margin::symmetric(10, 3)),
                    )
                    .show_inside(ctx, |ui| {
                        close_clicked = secondary_title_bar(
                            ui,
                            "NTE 控制台",
                            &mut self.console_window_scale,
                            CONSOLE_WINDOW_BASE_SIZE,
                        );
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(shadcn_background(self.dark_mode))
                            .inner_margin(egui::Margin::same(10)),
                    )
                    .show_inside(ctx, |ui| {
                        self.console_contents(ui);
                    });
                self.show_viewport_dialogs(ctx);
                close_clicked || ctx.input(|input| input.viewport().close_requested())
            },
        );
        if close_requested {
            self.console_open = false;
            self.console_corner_applied = false;
            self.retarget_dialogs(viewport_id, egui::ViewportId::ROOT);
        }
    }

    pub(crate) fn console_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::Settings, "设置");
            stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::Timeline, "时间轴");
            stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::Skills, "技能");
            stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::History, "历史");
            stable_selectable_value(
                ui,
                &mut self.console_tab,
                ConsoleTab::Characters,
                "角色数据",
            );
            stable_selectable_value(
                ui,
                &mut self.console_tab,
                ConsoleTab::EncryptedIni,
                "加密 INI",
            );
            // Genuine capture debugging — only reachable in debug builds.
            #[cfg(not(feature = "no_debug"))]
            {
                ui.separator();
                stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::Packets, "封包");
                stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::Resources, "资源");
                stable_selectable_value(ui, &mut self.console_tab, ConsoleTab::Diagnostics, "诊断");
            }
        });
        ui.separator();
        match self.console_tab {
            ConsoleTab::Settings => self.settings_contents(ui),
            ConsoleTab::Timeline => self.timeline_contents(ui),
            ConsoleTab::Skills => self.skills_contents(ui),
            ConsoleTab::History => self.history_contents(ui),
            ConsoleTab::Characters => self.debug_characters_contents(ui),
            ConsoleTab::EncryptedIni => self.debug_encrypted_ini_contents(ui),
            ConsoleTab::Packets => self.debug_packets_contents(ui),
            ConsoleTab::Resources => self.resource_audit_contents(ui),
            ConsoleTab::Diagnostics => self.diagnostics_contents(ui),
        }
    }

    pub(crate) fn timeline_contents(&mut self, ui: &mut egui::Ui) {
        self.abyss_selector(ui);
        inline_controls(ui, |ui| {
            ui.label(inline_text("统计间隔", ui.visuals().weak_text_color()));
            let mut bucket_seconds =
                config::sanitize_timeline_bucket_seconds(self.timeline_bucket_seconds);
            let changed = ui
                .add_sized(
                    egui::vec2(220.0, INLINE_CONTROL_HEIGHT),
                    egui::Slider::new(
                        &mut bucket_seconds,
                        TIMELINE_BUCKET_SECONDS_MIN..=TIMELINE_BUCKET_SECONDS_MAX,
                    )
                    .step_by(0.1)
                    .suffix("s")
                    .show_value(true),
                )
                .on_hover_text("每个统计点覆盖的秒数；越小越细，越大越平滑")
                .changed();
            if changed {
                self.timeline_bucket_seconds =
                    config::sanitize_timeline_bucket_seconds(bucket_seconds);
                self.timeline_cache = TimelineCache::default();
            }
            ui.separator();
            ui.label(inline_text("曲线", ui.visuals().weak_text_color()));
            for mode in TimelineDpsViewMode::all() {
                stable_selectable_value(ui, &mut self.timeline_dps_view_mode, *mode, mode.label());
            }
        });
        ui.add_space(6.0);
        let timeline = self.cached_timeline_series();
        if timeline.buckets.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 120.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(RichText::new("等待伤害数据").color(ui.visuals().weak_text_color()));
                },
            );
            return;
        }

        let peak_dps = timeline
            .buckets
            .iter()
            .map(|bucket| bucket.dps)
            .fold(0.0, f64::max);
        let duration = timeline
            .buckets
            .last()
            .map_or(0.0, |bucket| bucket.end_offset);
        if !matches!(self.timeline_dps_view_mode, TimelineDpsViewMode::Characters)
            || self.selected_timeline_char.is_some_and(|char_id| {
                !timeline.buckets.iter().any(|bucket| {
                    bucket
                        .role_damage
                        .iter()
                        .any(|role| role.char_id == char_id)
                })
            })
        {
            self.selected_timeline_char = None;
        }
        ui.columns(4, |columns| {
            compact_metric(
                &mut columns[0],
                "总伤害",
                format_number(timeline.total_damage),
                theme_accent(self.dark_mode),
                true,
            );
            compact_metric(
                &mut columns[1],
                "峰值 DPS",
                format_number(peak_dps),
                theme_accent(self.dark_mode),
                true,
            );
            let bucket_color = columns[2].visuals().text_color();
            compact_metric(
                &mut columns[2],
                "战斗时间",
                format!("{duration:.1}s"),
                bucket_color,
                false,
            );
            let interval_color = columns[3].visuals().text_color();
            compact_metric(
                &mut columns[3],
                "时停区间",
                timeline.time_stop_intervals.len().to_string(),
                interval_color,
                false,
            );
        });
        ui.add_space(8.0);
        // Read-only combat segmentation: the capture's outgoing damage split into
        // separate fights wherever an idle gap exceeds the threshold. Derived from
        // the same timeline buckets the chart uses, so it never touches live state.
        let segments = summarize_combat_segments(&timeline, COMBAT_SEGMENT_GAP_SECONDS);
        if segments.len() > 1 {
            let dark_mode = self.dark_mode;
            ui.horizontal_wrapped(|ui| {
                ui.label(inline_text(
                    format!(
                        "战斗分段 · {} 段（间隔 >{:.0}s 自动切分）",
                        segments.len(),
                        COMBAT_SEGMENT_GAP_SECONDS
                    ),
                    ui.visuals().weak_text_color(),
                ));
                for (index, segment) in segments.iter().enumerate() {
                    draw_combat_segment_chip(ui, index + 1, segment, dark_mode);
                }
            });
            ui.add_space(6.0);
        }
        let chart_height = (ui.available_height() - 30.0).max(260.0);
        draw_timeline_chart(
            ui,
            &timeline,
            self.timeline_dps_view_mode,
            chart_height,
            &mut self.selected_timeline_char,
            self.dark_mode,
            &self.characters,
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "当前保留窗口 · {:.1}s · {}s 统计间隔 · {} 个采样点 · {} 个事件标记",
                duration,
                format_timeline_seconds(timeline.bucket_seconds),
                timeline.buckets.len(),
                timeline.markers.len()
            ))
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
        );
    }

    pub(crate) fn skills_contents(&mut self, ui: &mut egui::Ui) {
        self.abyss_selector(ui);
        let breakdown = self.cached_skill_breakdown(None);
        if breakdown.rows.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 120.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new("等待技能归因数据").color(ui.visuals().weak_text_color()),
                    );
                },
            );
            return;
        }

        let mut characters = aggregate_skill_characters(&breakdown.rows);
        if let Some(selected) = self.selected_skill_breakdown_char
            && !characters.iter().any(|row| row.char_id == selected)
        {
            self.selected_skill_breakdown_char = None;
        }
        let content_height = ui.available_height().max(420.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), content_height),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(220.0, content_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.label(
                            RichText::new("角色")
                                .strong()
                                .color(shadcn_foreground(self.dark_mode)),
                        );
                        ui.add_space(4.0);
                        if ui
                            .selectable_label(self.selected_skill_breakdown_char.is_none(), "全队")
                            .clicked()
                        {
                            self.selected_skill_breakdown_char = None;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("skill_character_list")
                            .max_height((content_height - 64.0).max(160.0))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for (index, row) in characters.iter_mut().enumerate() {
                                    let selected =
                                        self.selected_skill_breakdown_char == Some(row.char_id);
                                    let label = format!(
                                        "{}  {} · {:.1}%",
                                        row.name,
                                        format_number(row.damage),
                                        if breakdown.total_damage > 0.0 {
                                            row.damage / breakdown.total_damage * 100.0
                                        } else {
                                            0.0
                                        }
                                    );
                                    if ui.selectable_label(selected, label).clicked() {
                                        self.selected_skill_breakdown_char = Some(row.char_id);
                                    }
                                    row.color =
                                        character_color(row.char_id, &self.characters, index);
                                }
                            });
                    },
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), content_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let selected_char = self.selected_skill_breakdown_char;
                        let visible_rows = breakdown
                            .rows
                            .iter()
                            .filter(|row| {
                                selected_char.is_none_or(|char_id| row.char_id == char_id)
                            })
                            .collect::<Vec<_>>();
                        let visible_total = visible_rows.iter().map(|row| row.damage).sum::<f64>();
                        ui.columns(4, |columns| {
                            compact_metric(
                                &mut columns[0],
                                "归因伤害",
                                format_number(visible_total),
                                theme_accent(self.dark_mode),
                                true,
                            );
                            let skill_count_color = columns[1].visuals().text_color();
                            compact_metric(
                                &mut columns[1],
                                "技能项",
                                visible_rows.len().to_string(),
                                skill_count_color,
                                false,
                            );
                            let unmapped_color = if breakdown.unknown.unmapped_skill_hits > 0 {
                                semantic_warning(self.dark_mode)
                            } else {
                                columns[2].visuals().text_color()
                            };
                            compact_metric(
                                &mut columns[2],
                                "待映射",
                                breakdown.unknown.unmapped_skill_hits.to_string(),
                                unmapped_color,
                                false,
                            );
                            let candidate_color = if breakdown.unknown.unknown_direction_hits > 0 {
                                semantic_warning(self.dark_mode)
                            } else {
                                columns[3].visuals().text_color()
                            };
                            compact_metric(
                                &mut columns[3],
                                "候选方向",
                                breakdown.unknown.unknown_direction_hits.to_string(),
                                candidate_color,
                                false,
                            );
                        });
                        ui.add_space(8.0);
                        let show_diagnostics = has_unknown_attribution(&breakdown);
                        let diagnostics_budget = if show_diagnostics { 130.0 } else { 0.0 };
                        let row_list_height =
                            (ui.available_height() - diagnostics_budget).max(220.0);
                        draw_skill_breakdown_rows(
                            ui,
                            &visible_rows,
                            visible_total,
                            row_list_height,
                            self.dark_mode,
                            &self.characters,
                        );
                        if show_diagnostics {
                            ui.add_space(8.0);
                            draw_unknown_attribution(ui, &breakdown, self.dark_mode);
                        }
                    },
                );
            },
        );
    }

    pub(crate) fn history_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("保存本次摘要")
                .on_hover_text("保存脱敏统计摘要，不包含封包、payload、IP、端口或本机路径")
                .clicked()
            {
                self.save_current_history_summary(ui.ctx());
            }
            if ui.button("重新加载").clicked() {
                self.history.reload();
                self.history.message = "历史列表已刷新".to_owned();
            }
            ui.label(
                RichText::new(format!("{} 条记录", self.history.records.len()))
                    .color(ui.visuals().weak_text_color()),
            );
            if self.history.skipped_files > 0 {
                ui.label(
                    RichText::new(format!("跳过 {} 个损坏文件", self.history.skipped_files))
                        .color(semantic_warning(self.dark_mode)),
                );
            }
            if !self.history.message.is_empty() {
                ui.label(
                    RichText::new(&self.history.message).color(ui.visuals().weak_text_color()),
                );
            }
        });
        ui.add_space(6.0);

        if self.history.records.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 160.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(RichText::new("暂无历史摘要").color(ui.visuals().weak_text_color()));
                },
            );
            return;
        }

        let content_height = ui.available_height().max(420.0);
        let record_rows = self
            .history
            .records
            .iter()
            .map(|record| {
                (
                    record.id.clone(),
                    record.display_time(),
                    record.short_party_label(),
                    record.summary.total_dps,
                    record.summary.total_damage,
                )
            })
            .collect::<Vec<_>>();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), content_height),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(300.0, content_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.label(
                            RichText::new("记录")
                                .strong()
                                .color(shadcn_foreground(self.dark_mode)),
                        );
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("history_record_list")
                            .max_height((content_height - 24.0).max(160.0))
                            .auto_shrink([false, false])
                            .show_rows(ui, 64.0, record_rows.len(), |ui, row_range| {
                                for row_index in row_range {
                                    let (id, time, party, dps, damage) = &record_rows[row_index];
                                    let selected = self.history.selected_id.as_deref() == Some(id);
                                    let label = format!(
                                        "{time}\n{party}\n{} DPS · {}",
                                        format_number(*dps),
                                        format_number(*damage)
                                    );
                                    if ui.selectable_label(selected, label).clicked() {
                                        self.history.selected_id = Some(id.clone());
                                    }
                                }
                            });
                    },
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), content_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("history_detail_compare")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                let selected = self.history.selected_record().cloned();
                                if let Some(record) = selected {
                                    self.history_detail_contents(ui, &record);
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.add_space(8.0);
                                    self.history_compare_contents(ui);
                                }
                            });
                    },
                );
            },
        );
    }

    pub(crate) fn history_detail_contents(&mut self, ui: &mut egui::Ui, record: &HistoryRecord) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(record.display_time())
                    .strong()
                    .color(shadcn_foreground(self.dark_mode)),
            );
            ui.label(
                RichText::new(format!("· {}", record.summary.dps_time_mode))
                    .color(ui.visuals().weak_text_color()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("删除")
                    .on_hover_text("删除这条本地历史摘要")
                    .clicked()
                {
                    self.request_confirmation_for(
                        ui.ctx().viewport_id(),
                        ConfirmationAction::DeleteHistory(record.id.clone()),
                    );
                }
                if let Some(team) = record.lower_team_dps()
                    && ui.button("设为下行预测").clicked()
                {
                    self.abyss_overview.lower_team = Some(team);
                    self.history.message = "已设为下行线预测队伍".to_owned();
                }
                if let Some(team) = record.upper_team_dps()
                    && ui.button("设为上行预测").clicked()
                {
                    self.abyss_overview.upper_team = Some(team);
                    self.history.message = "已设为上行线预测队伍".to_owned();
                }
            });
        });
        ui.add_space(6.0);
        ui.columns(4, |columns| {
            let damage_color = columns[1].visuals().text_color();
            let duration_color = columns[2].visuals().text_color();
            let quality_color = columns[3].visuals().text_color();
            compact_metric(
                &mut columns[0],
                "总 DPS",
                format_number(record.summary.total_dps),
                theme_accent(self.dark_mode),
                true,
            );
            compact_metric(
                &mut columns[1],
                "总伤害",
                format_number(record.summary.total_damage),
                damage_color,
                false,
            );
            compact_metric(
                &mut columns[2],
                "战斗时间",
                format_clear_seconds(record.summary.duration_seconds),
                duration_color,
                false,
            );
            compact_metric(
                &mut columns[3],
                "解析质量",
                format!(
                    "{} 命中 / {} 待映射",
                    record.summary.quality.hit_count, record.summary.quality.unmapped_skill_hits
                ),
                quality_color,
                false,
            );
        });
        ui.add_space(8.0);
        if record.summary.abyss.first_half.is_some() || record.summary.abyss.second_half.is_some() {
            if let Some(half) = &record.summary.abyss.first_half {
                let visual = HistoryVisualContext {
                    dark_mode: self.dark_mode,
                    characters: &self.characters,
                    avatar_textures: &self.avatar_textures,
                };
                draw_history_abyss_half(ui, half, visual);
            }
            if record.summary.abyss.first_half.is_some()
                && record.summary.abyss.second_half.is_some()
            {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
            }
            if let Some(half) = &record.summary.abyss.second_half {
                let visual = HistoryVisualContext {
                    dark_mode: self.dark_mode,
                    characters: &self.characters,
                    avatar_textures: &self.avatar_textures,
                };
                draw_history_abyss_half(ui, half, visual);
            }
        } else {
            draw_history_summary_rows(
                ui,
                "角色",
                &record.summary.characters,
                "技能",
                &record.summary.skills,
                HistoryVisualContext {
                    dark_mode: self.dark_mode,
                    characters: &self.characters,
                    avatar_textures: &self.avatar_textures,
                },
            );
        }
    }

    pub(crate) fn history_compare_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("对比")
                .strong()
                .color(shadcn_foreground(self.dark_mode)),
        );
        let choices = self
            .history
            .records
            .iter()
            .map(|record| {
                (
                    record.id.clone(),
                    format!("{} · {}", record.display_time(), record.party_label()),
                )
            })
            .collect::<Vec<_>>();
        // Stack the two selectors so they never overflow the panel horizontally; each combo's width
        // tracks the available width (clamped) and truncates long labels.
        let combo_width = (ui.available_width() - 56.0).clamp(180.0, 460.0);
        egui::Grid::new("history_compare_selectors")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("基准").color(ui.visuals().weak_text_color()));
                history_record_combo(
                    ui,
                    "history_compare_left",
                    &mut self.history.compare_left_id,
                    &choices,
                    combo_width,
                );
                ui.end_row();
                ui.label(RichText::new("对比").color(ui.visuals().weak_text_color()));
                history_record_combo(
                    ui,
                    "history_compare_right",
                    &mut self.history.compare_right_id,
                    &choices,
                    combo_width,
                );
                ui.end_row();
            });
        let Some((left, right, comparison)) = self.history.compare_records() else {
            ui.label(RichText::new("请选择两条不同记录").color(ui.visuals().weak_text_color()));
            return;
        };
        if left.summary.dps_time_mode != right.summary.dps_time_mode {
            ui.label(
                RichText::new("两条记录的 DPS 时间口径不同，请谨慎比较")
                    .color(semantic_warning(self.dark_mode)),
            );
        }
        ui.columns(3, |columns| {
            delta_metric(
                &mut columns[0],
                "总 DPS 差异",
                comparison.total_dps_delta,
                self.dark_mode,
            );
            delta_metric(
                &mut columns[1],
                "总伤害差异",
                comparison.total_damage_delta,
                self.dark_mode,
            );
            delta_metric(
                &mut columns[2],
                "时间差异",
                comparison.duration_delta,
                self.dark_mode,
            );
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("角色差异").color(ui.visuals().weak_text_color()));
                for row in &comparison.character_deltas {
                    ui.horizontal(|ui| {
                        ui.add_sized([120.0, 20.0], egui::Label::new(&row.name));
                        ui.monospace(format_signed_number(row.delta_dps));
                    });
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.label(RichText::new("技能差异").color(ui.visuals().weak_text_color()));
                for row in &comparison.skill_deltas {
                    ui.horizontal(|ui| {
                        ui.add_sized([190.0, 20.0], egui::Label::new(&row.name).truncate());
                        ui.monospace(format_signed_number(row.delta_damage));
                    });
                }
            });
        });
    }

    pub(crate) fn debug_encrypted_ini_contents(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("打开 INI").clicked() {
                self.request_debug_import(ui.ctx(), DebugImportKind::EncryptedIni);
            }
            let can_save = self.encrypted_ini_editor.path.is_some();
            if ui
                .add_enabled(can_save, egui::Button::new("保存为加密 INI"))
                .clicked()
            {
                self.save_encrypted_ini(ui.ctx());
            }
            if ui
                .add_enabled(can_save, egui::Button::new("重新载入"))
                .clicked()
                && let Some(path) = self.encrypted_ini_editor.path.clone()
            {
                if self.encrypted_ini_editor.dirty {
                    self.request_confirmation_for(
                        ui.ctx().viewport_id(),
                        ConfirmationAction::ReloadEncryptedIni(path),
                    );
                } else {
                    self.load_encrypted_ini_in(ui.ctx(), path);
                }
            }
            if ui.button("清空").clicked() {
                if self.encrypted_ini_editor.dirty {
                    self.request_confirmation_for(
                        ui.ctx().viewport_id(),
                        ConfirmationAction::ClearEncryptedIni,
                    );
                } else {
                    self.run_confirmation_action_for(
                        ConfirmationAction::ClearEncryptedIni,
                        ui.ctx().viewport_id(),
                    );
                }
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
                        stable_popup_selectable_value(
                            ui,
                            &mut self.encrypted_ini_editor.key,
                            key,
                            key.label(),
                        );
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

    pub(crate) fn load_encrypted_ini_in(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.load_encrypted_ini_for(path, ctx.viewport_id());
    }

    pub(crate) fn load_encrypted_ini_for(&mut self, path: PathBuf, viewport: egui::ViewportId) {
        match EncryptedIniEditorState::load(path) {
            Ok(editor) => {
                self.encrypted_ini_editor = editor;
                self.clear_last_error();
            }
            Err(error) => {
                self.encrypted_ini_editor.message = error.clone();
                self.set_last_error_for(viewport, error, Some(ErrorAction::OpenEncryptedIni));
            }
        }
    }

    pub(crate) fn save_encrypted_ini(&mut self, ctx: &egui::Context) {
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
                self.set_last_error_in(ctx, self.encrypted_ini_editor.message.clone(), None);
                return;
            }
        };
        if let Err(error) = atomic_write_text(&path, &encrypted) {
            self.encrypted_ini_editor.message = format!("保存 {} 失败: {error}", path.display());
            self.set_last_error_in(ctx, self.encrypted_ini_editor.message.clone(), None);
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
        self.clear_last_error();
    }

    /// Manual capture-NIC override UI (Settings tab). Automatic detection is the default; checking
    /// the box pins capture to a chosen interface as a VPN fallback. The choice persists via
    /// `UiConfig` and re-applies the game network through `refresh_game_network` so it takes effect
    /// on the next capture.
    pub(crate) fn capture_device_selector(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            if self.devices.is_empty() {
                let mut unchecked = false;
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(&mut unchecked, "手动指定网卡（VPN 兜底）"),
                );
                ui.colored_label(
                    semantic_warning(self.dark_mode),
                    "未发现可用网卡，请确认已安装 Npcap 后点击刷新",
                );
                if ui.button("刷新网卡列表").clicked() {
                    let _ = self.refresh_game_network();
                }
                return;
            }

            let mut manual = self.manual_capture_device.is_some();
            if ui
                .checkbox(&mut manual, "手动指定网卡")
                .on_hover_text(
                    "开启 VPN 时自动识别可能选错网卡；勾选后固定使用所选网卡，重新抓包后生效",
                )
                .changed()
            {
                // A non-empty device list guarantees a default, so manual mode is never left
                // checked-but-empty.
                self.manual_capture_device = manual
                    .then(|| {
                        self.devices
                            .get(self.selected_device)
                            .or_else(|| self.devices.first())
                            .map(|device| device.name.clone())
                    })
                    .flatten();
                let _ = self.refresh_game_network();
            }

            if self.manual_capture_device.is_none() {
                return;
            }

            let mut chosen = self.manual_capture_device.clone();
            let selected_text = chosen
                .as_deref()
                .and_then(|name| self.devices.iter().find(|device| device.name == name))
                .map_or_else(|| "请选择网卡".to_owned(), capture_device_label);
            egui::ComboBox::from_id_salt("manual_capture_device")
                .width(300.0)
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.set_min_width(300.0);
                    for device in &self.devices {
                        ui.selectable_value(
                            &mut chosen,
                            Some(device.name.clone()),
                            capture_device_label(device),
                        );
                    }
                });
            if chosen != self.manual_capture_device {
                self.manual_capture_device = chosen;
                let _ = self.refresh_game_network();
            }

            if ui
                .button("刷新网卡列表")
                .on_hover_text("重新枚举网卡")
                .clicked()
            {
                let _ = self.refresh_game_network();
            }

            // Self-contained status hint, independent of the shared diagnostic field.
            let resolved = self
                .manual_capture_device
                .as_deref()
                .is_some_and(|name| self.devices.iter().any(|device| device.name == name));
            if !resolved {
                ui.colored_label(
                    semantic_warning(self.dark_mode),
                    "所选网卡当前不可用，请重新选择或点击刷新",
                );
            } else if self.game_network.is_none() {
                ui.weak("未检测到游戏连接，将按公网/内网方向启发式解析");
            }
        });
    }

    /// First-class settings promoted out of the old debug "环境" tab: parse
    /// options, team DPS import/export, and an entry to the abyss value tables.
    /// Always available (not gated behind the debug feature).
    pub(crate) fn settings_contents(&mut self, ui: &mut egui::Ui) {
        let previous_hud_config = self.hud_config.clone();
        egui::CollapsingHeader::new("解析设置")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("settings_parse")
                    .num_columns(2)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("BPF 过滤");
                        ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(260.0))
                            .on_hover_text("抓包过滤表达式，重新抓包后生效");
                        ui.end_row();
                        ui.label("采集网卡");
                        self.capture_device_selector(ui);
                        ui.end_row();
                        ui.label("伤害来源");
                        ui.checkbox(
                            &mut self.server_damage_calibration,
                            "使用服务端 HP 差值校准",
                        )
                        .on_hover_text(
                            "重新抓包或重新导入后生效；只在服务端 HP 同步能与单条命中明确配对时覆盖伤害数值",
                        );
                        ui.end_row();
                        ui.label("DPS 时间");
                        let mut dps_time_mode = self.dps_time_mode;
                        egui::ComboBox::from_id_salt("dps_time_mode")
                            .width(150.0)
                            .selected_text(dps_time_mode.label())
                            .show_ui(ui, |ui| {
                                ui.set_min_width(150.0);
                                for option in DpsTimeMode::all() {
                                    stable_popup_selectable_value(
                                        ui,
                                        &mut dps_time_mode,
                                        *option,
                                        option.label(),
                                    );
                                }
                            })
                            .response
                            .on_hover_text(dps_time_mode.description());
                        if dps_time_mode != self.dps_time_mode {
                            self.dps_time_mode = dps_time_mode;
                            self.character_hit_cache = HitDetailCache::default();
                            self.team_hit_cache = HitDetailCache::default();
                        }
                        ui.end_row();
                        ui.label("穿透热键");
                        let mut hotkey = self.passthrough_hotkey;
                        egui::ComboBox::from_id_salt("passthrough_hotkey")
                            .width(PASSTHROUGH_HOTKEY_COMBO_WIDTH)
                            .selected_text(hotkey.label())
                            .show_ui(ui, |ui| {
                                ui.set_min_width(PASSTHROUGH_HOTKEY_COMBO_WIDTH);
                                ui.set_max_width(PASSTHROUGH_HOTKEY_COMBO_WIDTH);
                                for option in PassthroughHotkey::all() {
                                    stable_popup_selectable_value(
                                        ui,
                                        &mut hotkey,
                                        *option,
                                        option.label(),
                                    );
                                }
                            });
                        if hotkey != self.passthrough_hotkey {
                            self.set_passthrough_hotkey(hotkey);
                        }
                        ui.end_row();
                    });
            });
        egui::CollapsingHeader::new("HUD")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("settings_hud")
                    .num_columns(2)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("顶部");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.hud_config.show_title, "标题");
                            ui.checkbox(&mut self.hud_config.show_team_dps, "DPS");
                            ui.checkbox(&mut self.hud_config.show_duration, "时间");
                            ui.checkbox(&mut self.hud_config.show_total_damage, "总伤害");
                            ui.checkbox(&mut self.hud_config.show_damage_taken, "受击");
                        });
                        ui.end_row();
                        ui.label("模块");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.hud_config.show_character_rows, "角色排行");
                            ui.checkbox(&mut self.hud_config.show_abyss_half, "深渊");
                            ui.checkbox(&mut self.hud_config.show_passthrough_state, "穿透");
                            ui.checkbox(&mut self.hud_config.show_mini_timeline, "曲线");
                        });
                        ui.end_row();
                        ui.label("角色数");
                        ui.add(
                            egui::DragValue::new(&mut self.hud_config.max_characters)
                                .range(HUD_MAX_CHARACTERS_MIN..=HUD_MAX_CHARACTERS_MAX)
                                .speed(1),
                        );
                        ui.end_row();
                        ui.label("预设");
                        ui.horizontal(|ui| {
                            if ui.button("精简").clicked() {
                                self.hud_config = HudConfig::minimal();
                            }
                            if ui.button("标准").clicked() {
                                self.hud_config = HudConfig::default();
                            }
                            if ui.button("详细").clicked() {
                                self.hud_config = HudConfig::detailed();
                            }
                        });
                        ui.end_row();
                    });
            });
        self.hud_config = self.hud_config.clone().sanitized();
        if self.hud_config != previous_hud_config {
            self.hud_size_key = None;
        }
        egui::CollapsingHeader::new("队伍数据")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button("导入 DPS 数据")
                        .on_hover_text("导入队伍 DPS 数据（json），用于深渊通关预测")
                        .clicked()
                    {
                        self.import_team_dps(ui.ctx());
                    }
                    if ui
                        .button("导出队伍数据")
                        .on_hover_text("导出当前队伍与深渊上下队伍的 DPS（json，不含封包）")
                        .clicked()
                    {
                        self.export_team_dps(ui.ctx());
                    }
                });
                ui.small("导入/导出与场景无关，大世界与深渊均可使用");
            });
        egui::CollapsingHeader::new("抓包文件")
            .default_open(false)
            .show(ui, |ui| {
                if self.capture_log_stats.is_none() {
                    self.refresh_capture_log_stats();
                }
                let stats = self.capture_log_stats.unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "原始抓包：{} 个 · {}",
                        stats.count,
                        capture_logs::format_bytes(stats.total_bytes)
                    ));
                    if ui.button("刷新").clicked() {
                        self.refresh_capture_log_stats();
                    }
                    if ui
                        .add_enabled(stats.count > 0, egui::Button::new("清空"))
                        .clicked()
                    {
                        self.request_confirmation_for(
                            ui.ctx().viewport_id(),
                            ConfirmationAction::ClearCaptureLogs,
                        );
                    }
                });
                ui.small("实时抓包会把原始帧写入 logs/nte_raw_*.pcapng；清空不影响统计与历史。");
            });
        egui::CollapsingHeader::new("深渊数值")
            .default_open(true)
            .show(ui, |ui| {
                if ui
                    .button("打开深渊数值表")
                    .on_hover_text("以独立窗口打开，便于与实时 DPS 并排查看")
                    .clicked()
                {
                    self.abyss_overview_open = true;
                    self.abyss_overview.ensure_selection();
                }
            });
    }

    /// Runtime resource coverage for maintainers. This only checks distributable
    /// `res/` files and never touches client export paths or resource keys.
    pub(crate) fn resource_audit_contents(&mut self, ui: &mut egui::Ui) {
        if self.resource_audit.summary.is_none() && !self.resource_audit.loading {
            self.request_resource_audit();
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.resource_audit.loading, egui::Button::new("刷新检查"))
                .clicked()
            {
                self.request_resource_audit();
            }
            if self.resource_audit.loading {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label("正在检查运行资源");
            } else if !self.resource_audit.message.is_empty() {
                ui.label(
                    RichText::new(&self.resource_audit.message)
                        .color(ui.visuals().weak_text_color()),
                );
            }
            if let Some(summary) = &self.resource_audit.summary
                && ui.button("复制脱敏报告").clicked()
            {
                ui.ctx().copy_text(summary.redacted_text());
            }
        });
        ui.add_space(6.0);
        let Some(summary) = self.resource_audit.summary.as_ref() else {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 160.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new("等待资源检查结果").color(ui.visuals().weak_text_color()),
                    );
                },
            );
            return;
        };
        ui.columns(4, |columns| {
            compact_metric(
                &mut columns[0],
                "错误",
                summary.error_count().to_string(),
                semantic_danger(self.dark_mode),
                true,
            );
            compact_metric(
                &mut columns[1],
                "警告",
                summary.warning_count().to_string(),
                semantic_warning(self.dark_mode),
                true,
            );
            compact_metric(
                &mut columns[2],
                "角色/技能",
                format!(
                    "{} / {}",
                    summary.counts.characters, summary.counts.skill_damage
                ),
                theme_accent(self.dark_mode),
                false,
            );
            let abyss_reaction_color = columns[3].visuals().text_color();
            compact_metric(
                &mut columns[3],
                "深渊/反应",
                format!(
                    "{} / {}",
                    summary.counts.abyss_monsters, summary.counts.reactions
                ),
                abyss_reaction_color,
                false,
            );
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("等级");
            egui::ComboBox::from_id_salt("resource_audit_severity_filter")
                .width(120.0)
                .selected_text(self.resource_audit.severity_filter.label())
                .show_ui(ui, |ui| {
                    stable_popup_selectable_value(
                        ui,
                        &mut self.resource_audit.severity_filter,
                        ResourceAuditSeverityFilter::All,
                        ResourceAuditSeverityFilter::All.label(),
                    );
                    stable_popup_selectable_value(
                        ui,
                        &mut self.resource_audit.severity_filter,
                        ResourceAuditSeverityFilter::Error,
                        ResourceAuditSeverityFilter::Error.label(),
                    );
                    stable_popup_selectable_value(
                        ui,
                        &mut self.resource_audit.severity_filter,
                        ResourceAuditSeverityFilter::Warning,
                        ResourceAuditSeverityFilter::Warning.label(),
                    );
                });
            ui.label("分类");
            egui::ComboBox::from_id_salt("resource_audit_category_filter")
                .width(120.0)
                .selected_text(self.resource_audit.category_filter.label())
                .show_ui(ui, |ui| {
                    stable_popup_selectable_value(
                        ui,
                        &mut self.resource_audit.category_filter,
                        ResourceAuditCategoryFilter::All,
                        ResourceAuditCategoryFilter::All.label(),
                    );
                    for category in ResourceAuditCategory::all() {
                        stable_popup_selectable_value(
                            ui,
                            &mut self.resource_audit.category_filter,
                            ResourceAuditCategoryFilter::Category(*category),
                            category.label(),
                        );
                    }
                });
        });
        let filtered = summary
            .items
            .iter()
            .filter(|item| self.resource_audit.severity_filter.matches(item.severity))
            .filter(|item| self.resource_audit.category_filter.matches(item.category))
            .collect::<Vec<_>>();
        ui.add_space(6.0);
        if filtered.is_empty() {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 120.0),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        RichText::new("当前筛选下没有资源缺口")
                            .color(ui.visuals().weak_text_color()),
                    );
                },
            );
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("resource_audit_rows")
            .max_height((ui.available_height() - 12.0).max(180.0))
            .auto_shrink([false, false])
            .show_rows(ui, 44.0, filtered.len(), |ui, visible_rows| {
                for item in &filtered[visible_rows] {
                    draw_resource_audit_row(ui, item, self.dark_mode);
                }
            });
    }

    /// Read-only capture diagnostics plus raw-capture import/export. Genuine
    /// debugging — only reachable via the debug-gated "诊断" tab.
    pub(crate) fn diagnostics_contents(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("diagnostics_contents_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                self.diagnostics_contents_inner(ui);
            });
    }

    pub(crate) fn diagnostics_contents_inner(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("采集环境")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("diagnostics_environment")
                    .num_columns(2)
                    .spacing([14.0, 5.0])
                    .show(ui, |ui| {
                        ui.label("网卡");
                        let device_label = self
                            .devices
                            .get(self.selected_device)
                            .map(|device| {
                                if device.description.is_empty() {
                                    device.name.as_str()
                                } else {
                                    device.description.as_str()
                                }
                            })
                            .unwrap_or("未检测到");
                        let mode_suffix = if self.manual_capture_device.is_some() {
                            "（手动）"
                        } else {
                            "（自动）"
                        };
                        ui.monospace(format!("{device_label}{mode_suffix}"));
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
                                let file = capture.path().map_or_else(
                                    || "写入不可用".to_owned(),
                                    |path| {
                                        path.file_name()
                                            .and_then(|name| name.to_str())
                                            .unwrap_or("原始抓包文件")
                                            .to_owned()
                                    },
                                );
                                format!("{} 包 · {file}", capture.packet_count())
                            },
                        );
                        ui.monospace(raw_capture_label);
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("重新检测").clicked()
                        && let Err(error) = self.refresh_game_network()
                    {
                        self.set_last_error_in(ui.ctx(), error, Some(ErrorAction::RefreshNetwork));
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
        ui.add_space(8.0);
        egui::CollapsingHeader::new("自动诊断向导")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.diagnostics_running, egui::Button::new("运行诊断"))
                        .clicked()
                    {
                        self.request_capture_diagnostics();
                    }
                    if self.diagnostics_running {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label("正在检查 Npcap、游戏连接和当前抓包状态");
                    }
                    if let Some(report) = &self.diagnostics_report
                        && ui.button("复制脱敏报告").clicked()
                    {
                        ui.ctx().copy_text(report.redacted_text());
                    }
                });
                ui.add_space(4.0);
                if let Some(report) = &self.diagnostics_report {
                    draw_diagnostic_report(ui, report, self.dark_mode);
                } else {
                    ui.label(
                        RichText::new("点击运行诊断后，会逐项检查采集环境并给出下一步建议")
                            .color(ui.visuals().weak_text_color()),
                    );
                }
            });
        ui.add_space(8.0);
        let quality = self.current_quality_summary();
        draw_capture_quality_summary(ui, &quality, self.dark_mode);
    }

    pub(crate) fn debug_packets_contents(&mut self, ui: &mut egui::Ui) {
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

    pub(crate) fn debug_characters_contents(&mut self, ui: &mut egui::Ui) {
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
                    ui.spacing_mut().item_spacing.y = 7.0;
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
                        let avatar =
                            row.map_or_else(String::new, |row| json_string_field(row, "avatar"));
                        let color =
                            row.map_or_else(String::new, |row| json_string_field(row, "color"));
                        let searchable =
                            format!("{id} {name_zh} {name_en} {attribute}").to_lowercase();
                        if !search.is_empty() && !searchable.contains(&search) {
                            continue;
                        }
                        let selected =
                            self.character_editor.selected_id.as_deref() == Some(id.as_str());
                        let fallback_color = parse_hex_color(color.trim()).unwrap_or_else(|| {
                            character_color(
                                id.parse::<u32>().unwrap_or_default(),
                                self.characters.as_ref(),
                                0,
                            )
                        });
                        let dark_mode = self.dark_mode;
                        let fallback_color = readable_accent(fallback_color, dark_mode);
                        let clicked = {
                            let avatar_texture =
                                self.character_editor_avatar_texture(ui.ctx(), &avatar);
                            draw_character_editor_card(
                                ui,
                                CharacterEditorCard {
                                    id: &id,
                                    name_zh: &name_zh,
                                    name_en: &name_en,
                                    attribute: &attribute,
                                    avatar_texture,
                                    selected,
                                    fallback_color,
                                    dark_mode,
                                },
                            )
                            .clicked()
                        };
                        if clicked {
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
                        .width(CHARACTER_ATTRIBUTE_COMBO_WIDTH)
                        .selected_text(if self.character_editor.form.attribute.is_empty() {
                            "未设置"
                        } else {
                            self.character_editor.form.attribute.as_str()
                        })
                        .show_ui(ui, |ui| {
                            ui.set_min_width(CHARACTER_ATTRIBUTE_COMBO_WIDTH);
                            ui.set_max_width(CHARACTER_ATTRIBUTE_COMBO_WIDTH);
                            stable_popup_selectable_value(
                                ui,
                                &mut self.character_editor.form.attribute,
                                String::new(),
                                "未设置",
                            );
                            for attribute in CHARACTER_ATTRIBUTES {
                                stable_popup_selectable_value(
                                    ui,
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

    pub(crate) fn character_editor_avatar_texture(
        &mut self,
        ctx: &egui::Context,
        avatar: &str,
    ) -> Option<&egui::TextureHandle> {
        let avatar = avatar.trim();
        if avatar.is_empty() {
            return None;
        }
        if !self.avatar_textures.contains_key(avatar)
            && let Some(texture) = load_image_texture(ctx, &data_root(), avatar, "character-avatar")
        {
            self.avatar_textures.insert(avatar.to_owned(), texture);
        }
        self.avatar_textures.get(avatar)
    }

    pub(crate) fn save_character_editor(&mut self, ctx: &egui::Context) {
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
                self.clear_last_error();
            }
            Err(error) => {
                self.character_editor.message = format!("文件已写入，但重新加载失败: {error}");
                self.character_editor.dirty = true;
            }
        }
    }

    pub(crate) fn show_viewport_dialogs(&mut self, ctx: &egui::Context) {
        self.show_confirmation_dialog(ctx);
        self.show_error_window(ctx);
    }

    pub(crate) fn show_confirmation_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_confirmation.as_ref() else {
            return;
        };
        if self.pending_confirmation_viewport != ctx.viewport_id() {
            return;
        }
        let (title, message, confirm_label) = confirmation_content(action);
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(confirm_label).clicked() {
                        confirmed = true;
                    }
                    if ui.button("取消").clicked() {
                        cancelled = true;
                    }
                });
            });
        if confirmed {
            if let Some(action) = self.pending_confirmation.take() {
                self.run_confirmation_action_for(action, ctx.viewport_id());
            }
        } else if cancelled {
            self.pending_confirmation = None;
        }
    }

    pub(crate) fn show_error_window(&mut self, ctx: &egui::Context) {
        let Some(error) = self.last_error.clone() else {
            return;
        };
        if self.last_error_viewport != ctx.viewport_id() {
            return;
        }
        let action = self.last_error_action;
        let mut run_action = None;
        let mut close = false;
        egui::Window::new("错误")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(error);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if let Some(action) = action
                        && ui.button(error_action_label(action)).clicked()
                    {
                        run_action = Some(action);
                    }
                    if ui.button("关闭").clicked() {
                        close = true;
                    }
                });
            });
        if let Some(action) = run_action {
            self.clear_last_error();
            self.run_error_action(ctx, action);
        } else if close {
            self.clear_last_error();
        }
    }

    pub(crate) fn run_error_action(&mut self, ctx: &egui::Context, action: ErrorAction) {
        match action {
            ErrorAction::RefreshNetwork => {
                if let Err(error) = self.refresh_game_network() {
                    self.set_last_error_in(ctx, error, Some(ErrorAction::RefreshNetwork));
                }
            }
            ErrorAction::OpenPcapng => self.request_debug_import(ctx, DebugImportKind::Pcapng),
            ErrorAction::OpenCaptureJson => {
                self.request_debug_import(ctx, DebugImportKind::CaptureJson);
            }
            ErrorAction::OpenEncryptedIni => {
                self.request_debug_import(ctx, DebugImportKind::EncryptedIni);
            }
            ErrorAction::OpenTeamDpsImport => self.import_team_dps(ctx),
            ErrorAction::OpenConsole => {
                self.console_open = true;
                self.console_corner_applied = false;
            }
        }
    }

    pub(crate) fn retarget_dialogs(&mut self, from: egui::ViewportId, to: egui::ViewportId) {
        if self.last_error.is_some() && self.last_error_viewport == from {
            self.last_error_viewport = to;
        }
        if self.pending_confirmation.is_some() && self.pending_confirmation_viewport == from {
            self.pending_confirmation_viewport = to;
        }
    }
}

/// Compact chip for one detected combat segment in the timeline page.
fn draw_combat_segment_chip(
    ui: &mut egui::Ui,
    index: usize,
    segment: &CombatSegment,
    dark_mode: bool,
) {
    egui::Frame::popup(ui.style())
        .fill(shadcn_card(dark_mode))
        .stroke(Stroke::new(1.0, shadcn_border(dark_mode)))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!(
                    "段{index} · {}~{} · {} · {} DPS",
                    format_timeline_seconds(segment.start_offset),
                    format_timeline_seconds(segment.end_offset),
                    format_number(segment.total_damage),
                    format_number(segment.dps),
                ))
                .size(11.0),
            );
        });
}
