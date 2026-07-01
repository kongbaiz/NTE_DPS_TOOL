use super::*;

pub(crate) fn snapshot_party_team(
    party: &PartyCombatState,
    subtract_time_stop: bool,
) -> Option<TeamDps> {
    snapshot_team_from_stats(
        party.dps_with_time_stop(subtract_time_stop),
        party.duration_with_time_stop(subtract_time_stop),
        party.stats.values(),
    )
}

pub(crate) fn build_team_dps_export(
    state: &CombatState,
    abyss_overview: &AbyssOverviewState,
    subtract_time_stop: bool,
) -> Option<TeamDpsExport> {
    let single = snapshot_team_from_stats(
        state.dps_with_time_stop(subtract_time_stop),
        state.duration_with_time_stop(subtract_time_stop),
        state.stats.values(),
    );
    let upper = snapshot_party_team(&state.abyss.first_half, subtract_time_stop)
        .or_else(|| abyss_overview.upper_team.clone());
    let lower = snapshot_party_team(&state.abyss.second_half, subtract_time_stop)
        .or_else(|| abyss_overview.lower_team.clone());
    if single.is_none() && upper.is_none() && lower.is_none() {
        return None;
    }
    Some(TeamDpsExport {
        version: TEAM_DPS_EXPORT_VERSION,
        single,
        upper,
        lower,
    })
}

pub(crate) fn snapshot_team_from_stats<'a>(
    dps: f64,
    duration: f64,
    stats: impl IntoIterator<Item = &'a CharacterStats>,
) -> Option<TeamDps> {
    if dps <= 0.0 {
        return None;
    }
    // A capture can contain non-party pseudo rows; keep only the top contributors
    // and use the same duration as the exported team DPS for comparable numbers.
    let shared_duration = duration.max(1.0);
    let mut members: Vec<&CharacterStats> = stats
        .into_iter()
        .filter(|stats| stats.char_id != 0 && stats.char_id < 900_000 && stats.damage > 0.0)
        .collect();
    members.sort_by(|left, right| {
        right
            .damage
            .total_cmp(&left.damage)
            .then_with(|| left.char_id.cmp(&right.char_id))
    });
    members.truncate(TEAM_DPS_MAX_MEMBERS);
    Some(TeamDps {
        dps,
        members: members
            .into_iter()
            .map(|stats| TeamDpsMember {
                id: stats.char_id,
                dps: stats.damage / shared_duration,
                name: stats.name.clone(),
            })
            .collect(),
    })
}

pub(crate) type AbyssSeasonNavEntry = (u32, Option<String>, Vec<(u32, Option<String>, u32, usize)>);

pub(crate) fn draw_abyss_floor_nav(
    ui: &mut egui::Ui,
    season_nav: &[AbyssSeasonNavEntry],
    selected_season: &mut Option<u32>,
    selected_floor: &mut Option<u32>,
    selected_monster_pack_id: &mut Option<String>,
    expanded_season: &mut Option<u32>,
) {
    ui.label(
        RichText::new(t("Sites"))
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt("abyss_all_season_floor_nav")
        .auto_shrink([false, false])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for (season, name, floors) in season_nav {
                let expanded = *expanded_season == Some(*season);
                let selected_in_season = *selected_season == Some(*season);
                let season_label = format!(
                    "{} {} ·  {}",
                    if expanded { "▼" } else { "▶" },
                    abyss_season_label(*season, name.as_deref()),
                    tf("{} floors", &[&floors.len().to_string()])
                );
                if ui
                    .add_sized(
                        egui::vec2(ui.available_width(), 28.0),
                        egui::Button::selectable(selected_in_season || expanded, season_label)
                            .frame_when_inactive(true),
                    )
                    .clicked()
                {
                    *expanded_season = if expanded { None } else { Some(*season) };
                }
                ui.add_space(3.0);
                if expanded {
                    ui.indent(("abyss_season_floors", season), |ui| {
                        for (floor, floor_name, monster_count, wave_count) in floors {
                            let selected = *selected_season == Some(*season)
                                && *selected_floor == Some(*floor);
                            if draw_abyss_floor_nav_row(
                                ui,
                                selected,
                                *floor,
                                floor_name.as_deref(),
                                *monster_count,
                                *wave_count,
                            ) {
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

pub(crate) fn draw_abyss_floor_nav_row(
    ui: &mut egui::Ui,
    selected: bool,
    floor: u32,
    floor_name: Option<&str>,
    monster_count: u32,
    wave_count: usize,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::click());
    let visuals = ui.visuals();
    if selected || response.hovered() {
        let fill = if selected {
            visuals.selection.bg_fill
        } else {
            visuals.widgets.hovered.bg_fill
        };
        ui.painter().rect_filled(rect.shrink(1.0), 5.0, fill);
    }

    let text_color = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    let weak_color = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.weak_text_color()
    };
    // Two-column row (name left, count right) inside a narrow nav. Text length
    // varies by language, so measure the count and clip each side to its own
    // region — this keeps them from overlapping regardless of width.
    let name_text = abyss_floor_nav_label(floor, floor_name);
    let count_text = tf(
        "{} monsters · {} waves",
        &[&monster_count.to_string(), &wave_count.to_string()],
    );
    let name_font = egui::FontId::proportional(13.0);
    let count_font = egui::FontId::proportional(12.0);
    let count_width = ui
        .painter()
        .layout_no_wrap(count_text.clone(), count_font.clone(), weak_color)
        .size()
        .x
        .clamp(0.0, (rect.width() - 44.0).max(0.0));
    let count_left = rect.right() - 8.0 - count_width;
    let painter = ui.painter();
    painter
        .with_clip_rect(egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.top()),
            egui::pos2((count_left - 6.0).max(rect.left() + 8.0), rect.bottom()),
        ))
        .text(
            rect.left_center() + egui::vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            name_text,
            name_font,
            text_color,
        );
    painter
        .with_clip_rect(egui::Rect::from_min_max(
            egui::pos2(count_left, rect.top()),
            egui::pos2(rect.right() - 8.0, rect.bottom()),
        ))
        .text(
            rect.right_center() - egui::vec2(8.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            count_text,
            count_font,
            weak_color,
        );

    response.clicked()
}

pub(crate) fn abyss_season_label(season: u32, name: Option<&str>) -> String {
    name.filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| tf("Season {}", &[&season.to_string()]))
}

pub(crate) fn abyss_floor_label(floor: &AbyssFloor) -> String {
    floor
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| tf("Floor {}", &[&floor.floor.to_string()]))
}

pub(crate) fn abyss_floor_nav_label(floor: u32, name: Option<&str>) -> String {
    name.filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| tf("Floor {}", &[&floor.to_string()]))
}

/// What a line section's prediction control was clicked to do this frame.
#[derive(PartialEq, Eq)]
pub(crate) enum LinePredictionAction {
    None,
    ImportCurrent,
    ImportFile,
    Clear,
}

pub(crate) struct LinePredictionResult {
    pub(crate) action: LinePredictionAction,
    pub(crate) target_seconds: f64,
}

/// Inputs for the per-line clear-time prediction shown in a line section header.
pub(crate) struct LinePredictionView<'a> {
    pub(crate) team: Option<&'a TeamDps>,
    pub(crate) line_hp: f64,
    pub(crate) target_seconds: f64,
    pub(crate) can_import: bool,
    pub(crate) avatar_textures: &'a HashMap<String, egui::TextureHandle>,
    pub(crate) characters: &'a HashMap<u32, CharacterInfo>,
}

pub(crate) fn abyss_monster_count(monsters: &[&AbyssMonsterEntry]) -> u32 {
    monsters.iter().map(|monster| monster.count).sum()
}

pub(crate) fn predicted_clear_seconds(line_hp: f64, team: &TeamDps) -> Option<f64> {
    (team.dps > 0.0 && line_hp > 0.0).then(|| line_hp / team.dps)
}

pub(crate) fn sanitize_prediction_target_seconds(seconds: f64) -> f64 {
    if seconds.is_finite() {
        seconds.clamp(1.0, 600.0)
    } else {
        90.0
    }
}

pub(crate) fn format_clear_seconds(seconds: f64) -> String {
    if seconds >= 60.0 {
        let minutes = (seconds / 60.0).floor();
        let rest = seconds - minutes * 60.0;
        tf(
            "{}m{}s",
            &[&format!("{minutes:.0}"), &format!("{rest:04.1}")],
        )
    } else {
        tf("{}s", &[&format!("{seconds:.1}")])
    }
}

pub(crate) fn draw_team_avatar(
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

pub(crate) fn draw_line_prediction_header(
    ui: &mut egui::Ui,
    view: &LinePredictionView,
    dark_mode: bool,
) -> LinePredictionResult {
    let mut action = LinePredictionAction::None;
    let mut target_seconds = sanitize_prediction_target_seconds(view.target_seconds);
    ui.add_space(10.0);
    let weak_color = ui.visuals().weak_text_color();
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
            Some(seconds) => tf("Est. {}", &[&format_clear_seconds(seconds)]),
            None => t("Est. —"),
        };
        ui.label(
            RichText::new(time_text)
                .size(INLINE_CONTROL_TEXT_SIZE)
                .strong()
                .color(theme_accent(dark_mode)),
        );
        ui.label(inline_text(
            format!("· {} DPS", format_number(team.dps)),
            weak_color,
        ));
        // Auto-sized to its (localized) label instead of a fixed pixel width — a
        // width tuned for the Chinese text clips the longer English translation.
        if ui
            .button(t("Clear"))
            .on_hover_text(t("Clear this line's prediction team"))
            .clicked()
        {
            action = LinePredictionAction::Clear;
        }
    } else {
        if view.can_import
            && ui
                .button(t("Predict with Current Team"))
                .on_hover_text(t(
                    "Set the team measured this session as this line's prediction team",
                ))
                .clicked()
        {
            action = LinePredictionAction::ImportCurrent;
        }
        if !view.can_import {
            ui.label(inline_text(
                t("Import data to predict clear time"),
                weak_color,
            ));
        }
    }
    ui.add_space(4.0);
    ui.label(inline_text(t("Target"), weak_color));
    ui.add_sized(
        egui::vec2(72.0, INLINE_CONTROL_HEIGHT),
        egui::DragValue::new(&mut target_seconds)
            .range(1.0..=600.0)
            .speed(1.0)
            .suffix("s"),
    )
    .on_hover_text(t("Back-calculate the DPS needed for this target time"));
    if let Some(required_dps) = required_dps_for_target_time(view.line_hp, target_seconds) {
        ui.label(inline_text(
            tf("Need {} DPS", &[&format_number(required_dps)]),
            weak_color,
        ));
    }
    // Per-line file import is always available (the "Import Separately" button): load a
    // DPS data file into just this line. Auto-sized — see the Clear/Predict buttons above.
    if ui
        .button(t("Import Separately"))
        .on_hover_text(t("Import a DPS data file for this line only"))
        .clicked()
    {
        action = LinePredictionAction::ImportFile;
    }
    LinePredictionResult {
        action,
        target_seconds: sanitize_prediction_target_seconds(target_seconds),
    }
}

pub(crate) fn draw_abyss_wave_prediction(
    ui: &mut egui::Ui,
    monsters: &[&AbyssMonsterEntry],
    team: Option<&TeamDps>,
    dark_mode: bool,
) {
    let waves = line_hp_by_wave(monsters.iter().copied());
    if waves.is_empty() {
        return;
    }
    let total_hp = waves.iter().map(|wave| wave.hp).sum::<f64>().max(1.0);
    let predictions = team
        .map(|team| predict_wave_clear_times(&waves, team.dps))
        .unwrap_or_default();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let width = ui.available_width().max(120.0);
        let height = 12.0;
        for (index, wave) in waves.iter().enumerate() {
            let segment_width = ((wave.hp / total_hp) as f32 * width).max(18.0);
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(segment_width, height), egui::Sense::hover());
            let color =
                theme_accent(dark_mode).gamma_multiply((0.45 + index as f32 * 0.09).min(0.92));
            ui.painter().rect_filled(rect, 2.0, color);
            let prediction = predictions.get(index);
            let label = wave
                .wave
                .map_or_else(|| t("Unwaved"), |wave| tf("Wave {}", &[&wave.to_string()]));
            let mut hover = tf(
                "{}\n{} enemies\nHP {}",
                &[
                    &label,
                    &wave.monster_count.to_string(),
                    &format_number(wave.hp),
                ],
            );
            if let Some(prediction) = prediction {
                let _ = write!(
                    hover,
                    "\n{}",
                    tf(
                        "Est. {}, cumulative {}",
                        &[
                            &format_clear_seconds(prediction.seconds),
                            &format_clear_seconds(prediction.cumulative_seconds),
                        ],
                    )
                );
            }
            response.on_hover_text(hover);
        }
    });
}

// UI draw helper: each argument is a distinct, unrelated input (selection state,
// textures, theme, prediction), so grouping them into a struct would not aid
// readability.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_abyss_line_section(
    ui: &mut egui::Ui,
    title: &str,
    monsters: &[&AbyssMonsterEntry],
    selected_pack_id: Option<&str>,
    selected_target: &mut Option<String>,
    monster_textures: &HashMap<String, egui::TextureHandle>,
    dark_mode: bool,
    prediction: Option<LinePredictionView>,
) -> LinePredictionResult {
    const SLOT_COUNT: usize = 6;
    const GAP: f32 = 6.0;
    let mut result = LinePredictionResult {
        action: LinePredictionAction::None,
        target_seconds: prediction.as_ref().map_or(90.0, |view| {
            sanitize_prediction_target_seconds(view.target_seconds)
        }),
    };
    egui::Frame::new()
        .fill(shadcn_card(dark_mode))
        .stroke(Stroke::new(1.0, shadcn_border(dark_mode)))
        .corner_radius(7)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            inline_controls(ui, |ui| {
                ui.label(
                    RichText::new(title)
                        .size(INLINE_CONTROL_TEXT_SIZE)
                        .strong()
                        .color(shadcn_foreground(dark_mode)),
                );
                ui.label(
                    RichText::new(tf(
                        "{} enemies · {} kinds",
                        &[
                            &abyss_monster_count(monsters).to_string(),
                            &monsters.len().to_string(),
                        ],
                    ))
                    .size(INLINE_CONTROL_TEXT_SIZE)
                    .color(ui.visuals().weak_text_color()),
                );
                if let Some(view) = prediction.as_ref() {
                    result = draw_line_prediction_header(ui, view, dark_mode);
                }
            });
            if let Some(view) = prediction.as_ref() {
                draw_abyss_wave_prediction(ui, monsters, view.team, dark_mode);
            }
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
    result
}

pub(crate) fn draw_abyss_monster_chip(
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
            "{} ×{}  HP {}",
            monster_wave_label(monster),
            monster.count,
            format_stat_value(abyss_monster_total_hp(monster))
        ),
        egui::FontId::monospace(9.0),
        ui.visuals().weak_text_color(),
    );

    response.on_hover_text(format!(
        "{} ×{}\n{}",
        monster.name,
        monster.count,
        monster_line_label(monster)
    ))
}

pub(crate) fn draw_abyss_empty_chip(ui: &mut egui::Ui, width: f32, dark_mode: bool) {
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

pub(crate) fn draw_abyss_more_chip(ui: &mut egui::Ui, count: usize, width: f32, dark_mode: bool) {
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
    response.on_hover_text(tf("{} more enemies not shown", &[&count.to_string()]));
}

pub(crate) fn draw_monster_portrait(
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

pub(crate) fn contain_rect(bounds: egui::Rect, image_size: egui::Vec2) -> egui::Rect {
    if image_size.x <= 0.0 || image_size.y <= 0.0 {
        return bounds;
    }
    let scale = (bounds.width() / image_size.x).min(bounds.height() / image_size.y);
    egui::Rect::from_center_size(bounds.center(), image_size * scale)
}

pub(crate) fn draw_abyss_chip_frame(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    selected: bool,
    dark_mode: bool,
) {
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

pub(crate) fn draw_abyss_monster_detail(
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
                            RichText::new(monster_line_label(monster))
                                .size(11.0)
                                .color(ui.visuals().weak_text_color()),
                        )
                        .truncate(),
                    );
                });
            });
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(tf("Count ×{}", &[&monster.count.to_string()]))
                        .strong()
                        .color(shadcn_foreground(dark_mode)),
                );
                if let Some(level) = monster.level {
                    ui.label(
                        RichText::new(tf("Level {}", &[&level.to_string()]))
                            .color(ui.visuals().weak_text_color()),
                    );
                }
                ui.label(
                    RichText::new(tf(
                        "Total HP {}",
                        &[&format_stat_value(abyss_monster_total_hp(monster))],
                    ))
                    .color(theme_accent(dark_mode)),
                );
                if monster.is_boss {
                    ui.label(RichText::new("Boss").color(semantic_warning(dark_mode)));
                }
            });
            ui.add_space(10.0);
            ui.label(
                RichText::new(t("Per-enemy stat fields"))
                    .strong()
                    .color(shadcn_foreground(dark_mode)),
            );
            let grid_height = ui.available_height().max(120.0);
            egui::ScrollArea::vertical()
                .id_salt(("abyss_raw_props", monster.pack_id.as_str()))
                .auto_shrink([false, false])
                .max_height(grid_height)
                .show(ui, |ui| {
                    const SCROLLBAR_GUTTER: f32 = 28.0;
                    let mut viewport_clip = ui.clip_rect();
                    viewport_clip.max.x =
                        (viewport_clip.max.x - SCROLLBAR_GUTTER).max(viewport_clip.min.x);
                    let row_width = (ui.available_width() - SCROLLBAR_GUTTER).max(0.0);
                    let row_height = 21.0;
                    let pair_gap = 20.0;
                    // Keep this at 2-3 columns: four columns make the last value
                    // fight the vertical scrollbar, while three still uses the
                    // available width without leaving a wide empty gutter.
                    let columns = ((row_width / 330.0).floor() as usize).clamp(2, 3);
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

pub(crate) fn abyss_stat_pair_sized(
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

pub(crate) fn monster_texture<'a>(
    textures: &'a HashMap<String, egui::TextureHandle>,
    monster_id: &str,
) -> Option<&'a egui::TextureHandle> {
    monster_image_keys(monster_id)
        .into_iter()
        .find_map(|key| textures.get(&key))
}

pub(crate) fn monster_image_keys(value: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let base = canonical_monster_image_key(value);
    push_unique_key(&mut keys, base.clone());
    push_trimmed_monster_keys(&mut keys, &base);
    keys
}

pub(crate) fn monster_image_resource_keys(value: &str) -> Vec<String> {
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

pub(crate) fn monster_image_stem_candidates(monster_id: &str) -> Vec<String> {
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

pub(crate) fn raw_case_monster_image_stems(value: &str) -> Vec<String> {
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

pub(crate) fn raw_monster_image_keys(value: &str) -> Vec<String> {
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

pub(crate) fn push_trimmed_monster_stems(stems: &mut Vec<String>, stem: &str) {
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

pub(crate) fn titlecase_boss_key(key: &str) -> String {
    key.strip_prefix("boss_")
        .map(|suffix| format!("Boss_{suffix}"))
        .unwrap_or_else(|| key.to_owned())
}

pub(crate) fn push_trimmed_monster_keys(keys: &mut Vec<String>, key: &str) {
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

pub(crate) fn push_unique_key(keys: &mut Vec<String>, key: String) {
    if !keys.iter().any(|existing| existing == &key) {
        keys.push(key);
    }
}

pub(crate) fn canonical_monster_image_key(value: &str) -> String {
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

pub(crate) fn monster_icon_text(monster: &AbyssMonsterEntry) -> String {
    monster
        .name
        .chars()
        .find(|character| !character.is_whitespace())
        .unwrap_or('?')
        .to_string()
}

pub(crate) fn monster_color(monster_id: &str, dark_mode: bool) -> Color32 {
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

pub(crate) fn monster_line_label(monster: &AbyssMonsterEntry) -> String {
    let half = monster.half.map(|value| match value {
        0 => t("Ascending Line"),
        1 => t("Descending Line"),
        other => tf("Line {}", &[&other.to_string()]),
    });
    let wave = monster
        .wave
        .map(|value| tf("Wave {}", &[&value.to_string()]));
    match (half, wave) {
        (Some(half), Some(wave)) => format!("{half} · {wave}"),
        (Some(half), None) => half,
        (None, Some(wave)) => wave,
        (None, None) => t("Full floor config"),
    }
}

pub(crate) fn monster_wave_label(monster: &AbyssMonsterEntry) -> String {
    monster
        .wave
        .map(|wave| format!("W{wave}"))
        .unwrap_or_else(|| "-".to_owned())
}

pub(crate) fn format_stat_value(value: f64) -> String {
    if value.abs() >= 1000.0 || value.fract().abs() < f64::EPSILON {
        format_number(value)
    } else {
        format!("{value:.2}")
    }
}

pub(crate) fn write_abyss_half_json<W: std::fmt::Write + ?Sized>(
    out: &mut W,
    key: &str,
    party: &PartyCombatState,
    subtract_time_stop: bool,
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
    writeln!(
        out,
        "      \"dps\": {},",
        json_f64(party.dps_with_time_stop(subtract_time_stop))
    )
    .ok();
    writeln!(
        out,
        "      \"duration_seconds\": {},",
        json_f64(party.duration_with_time_stop(subtract_time_stop))
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
        let row_duration = party.character_duration_with_time_stop(row, subtract_time_stop);
        let row_dps = party.character_dps_with_time_stop(row, subtract_time_stop);
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
        writeln!(out, "          \"dps\": {},", json_f64(row_dps)).ok();
        writeln!(
            out,
            "          \"duration_seconds\": {},",
            json_f64(row_duration)
        )
        .ok();
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
