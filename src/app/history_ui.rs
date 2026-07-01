use super::*;

pub(crate) fn history_record_combo(
    ui: &mut egui::Ui,
    id: &str,
    selected_id: &mut Option<String>,
    choices: &[(String, String)],
    width: f32,
) {
    let selected_text = selected_id
        .as_deref()
        .and_then(|id| choices.iter().find(|(choice_id, _)| choice_id == id))
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| t("Not selected"));
    // `truncate` keeps the button pinned to `width` and ellipsizes long record labels instead of
    // letting the button grow and overflow the panel.
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .truncate()
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            ui.set_min_width(width.max(260.0));
            for (choice_id, label) in choices {
                stable_popup_selectable_value(ui, selected_id, Some(choice_id.clone()), label);
            }
        });
}

pub(crate) fn delta_metric(ui: &mut egui::Ui, label: &str, value: f64, dark_mode: bool) {
    let color = if value > 0.0 {
        semantic_success(dark_mode)
    } else if value < 0.0 {
        semantic_danger(dark_mode)
    } else {
        ui.visuals().text_color()
    };
    compact_metric(ui, label, format_signed_number(value), color, false);
}

pub(crate) fn format_signed_number(value: f64) -> String {
    if value > 0.0 {
        format!("+{}", format_number(value))
    } else if value < 0.0 {
        format!("-{}", format_number(value.abs()))
    } else {
        format_number(0.0)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryVisualContext<'a> {
    pub(crate) dark_mode: bool,
    pub(crate) characters: &'a HashMap<u32, CharacterInfo>,
    pub(crate) avatar_textures: &'a HashMap<String, egui::TextureHandle>,
}

pub(crate) fn draw_history_abyss_half(
    ui: &mut egui::Ui,
    half: &CombatSessionAbyssHalfSummary,
    visual: HistoryVisualContext<'_>,
) {
    let dark_mode = visual.dark_mode;
    let accent = history_half_accent(&half.half, dark_mode);
    egui::Frame::new()
        .fill(shadcn_card(dark_mode))
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.55)))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot_rect, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 5.0, accent);
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&half.half)
                        .size(18.0)
                        .strong()
                        .color(shadcn_foreground(dark_mode)),
                );
                ui.add_space(8.0);
                history_metric_chip(ui, "DPS", format_number(half.total_dps), accent, dark_mode);
                history_metric_chip(
                    ui,
                    &t("Damage"),
                    format_number(half.total_damage),
                    ui.visuals().text_color(),
                    dark_mode,
                );
            });
            ui.add_space(10.0);
            draw_history_summary_rows(
                ui,
                &t("Character Contribution"),
                &half.characters,
                &t("Skill Composition"),
                &half.skills,
                visual,
            );
        });
    ui.add_space(2.0);
}

pub(crate) fn history_half_accent(half: &str, dark_mode: bool) -> Color32 {
    if half.contains('上') {
        if dark_mode {
            Color32::from_rgb(96, 165, 250)
        } else {
            Color32::from_rgb(37, 99, 235)
        }
    } else if dark_mode {
        Color32::from_rgb(52, 211, 153)
    } else {
        Color32::from_rgb(5, 150, 105)
    }
}

pub(crate) fn history_metric_chip(
    ui: &mut egui::Ui,
    label: &str,
    value: String,
    color: Color32,
    dark_mode: bool,
) {
    egui::Frame::new()
        .fill(shadcn_card_hover(dark_mode))
        .stroke(Stroke::new(1.0, shadcn_border(dark_mode)))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(label)
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.monospace(RichText::new(value).size(14.0).strong().color(color));
            });
        });
}

pub(crate) fn draw_history_avatar(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    row: &CombatSessionCharacterSummary,
    color: Color32,
    characters: &HashMap<u32, CharacterInfo>,
    avatar_textures: &HashMap<String, egui::TextureHandle>,
    dark_mode: bool,
) {
    let texture = characters
        .get(&row.char_id)
        .and_then(|info| info.avatar.as_deref())
        .and_then(|avatar| avatar_textures.get(avatar));
    if let Some(texture) = texture {
        egui::Image::new((texture.id(), rect.size()))
            .corner_radius(8.0)
            .paint_at(ui, rect);
    } else {
        ui.painter()
            .rect_filled(rect, 8.0, color.gamma_multiply(0.85));
        let initial = row.name.chars().next().unwrap_or('?');
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial,
            egui::FontId::proportional(16.0),
            contrast_text(color),
        );
    }
    ui.painter().rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, shadcn_border(dark_mode)),
        egui::StrokeKind::Inside,
    );
}

pub(crate) fn draw_history_progress_row(
    ui: &mut egui::Ui,
    color: Color32,
    progress: f32,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui, egui::Rect),
) {
    let size = egui::vec2(ui.available_width().max(1.0), height);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let dark_mode = ui.visuals().dark_mode;
    ui.painter()
        .rect_filled(rect, 7.0, shadcn_card_hover(dark_mode));
    let progress_rect = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(
            rect.left() + rect.width() * progress.clamp(0.0, 1.0),
            rect.bottom(),
        ),
    );
    ui.painter()
        .rect_filled(progress_rect, 7.0, color.gamma_multiply(0.18));
    ui.painter().rect_stroke(
        rect,
        7.0,
        Stroke::new(1.0, shadcn_border(dark_mode)),
        egui::StrokeKind::Inside,
    );
    let content_rect = rect.shrink2(egui::vec2(8.0, 5.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| add_contents(ui, content_rect),
    );
}

pub(crate) fn history_section_heading(ui: &mut egui::Ui, title: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(dot_rect.center(), 4.0, color);
        ui.label(
            RichText::new(title)
                .strong()
                .color(ui.visuals().text_color()),
        );
    });
}

pub(crate) fn draw_history_summary_rows(
    ui: &mut egui::Ui,
    character_title: &str,
    character_rows: &[CombatSessionCharacterSummary],
    skill_title: &str,
    skills: &[CombatSessionSkillSummary],
    visual: HistoryVisualContext<'_>,
) {
    ui.columns(2, |columns| {
        draw_history_character_rows(&mut columns[0], character_title, character_rows, visual);
        draw_history_skill_rows(&mut columns[1], skill_title, skills, visual.dark_mode);
    });
}

pub(crate) fn draw_history_character_rows(
    ui: &mut egui::Ui,
    title: &str,
    rows: &[CombatSessionCharacterSummary],
    visual: HistoryVisualContext<'_>,
) {
    let dark_mode = visual.dark_mode;
    history_section_heading(ui, title, theme_accent(dark_mode));
    ui.add_space(4.0);
    for (index, row) in rows.iter().take(6).enumerate() {
        let color = readable_accent(
            character_color(row.char_id, visual.characters, index),
            dark_mode,
        );
        draw_history_progress_row(
            ui,
            color,
            (row.damage_share_percent / 100.0) as f32,
            48.0,
            |ui, content_rect| {
                let avatar_rect = egui::Rect::from_center_size(
                    egui::pos2(content_rect.left() + 16.0, content_rect.center().y),
                    egui::vec2(30.0, 30.0),
                );
                draw_history_avatar(
                    ui,
                    avatar_rect,
                    row,
                    color,
                    visual.characters,
                    visual.avatar_textures,
                    dark_mode,
                );
                ui.add_space(36.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&row.name)
                            .strong()
                            .color(shadcn_foreground(dark_mode)),
                    );
                    ui.label(
                        RichText::new(format!("{} DPS", format_number(row.dps)))
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.1}%", row.damage_share_percent))
                            .strong()
                            .color(color),
                    );
                });
            },
        );
        ui.add_space(4.0);
    }
}

pub(crate) fn skill_row_color(
    row: &CombatSessionSkillSummary,
    index: usize,
    dark_mode: bool,
) -> Color32 {
    if row.is_follow_up {
        semantic_warning(dark_mode)
    } else if row.category.contains("深渊") {
        semantic_success(dark_mode)
    } else {
        readable_accent(
            deterministic_character_fallback_color(
                format!("skill:{index}:{}", row.name).as_bytes(),
            ),
            dark_mode,
        )
    }
}

pub(crate) fn skill_display_name(row: &CombatSessionSkillSummary) -> String {
    if contains_cjk(&row.name) {
        return row.name.clone();
    }

    let skill_name = fallback_skill_display_name(row);
    if !row.char_name.trim().is_empty() {
        format!("{} · {skill_name}", row.char_name.trim())
    } else {
        skill_name
    }
}

pub(crate) fn contains_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        )
    })
}

pub(crate) fn fallback_skill_display_name(row: &CombatSessionSkillSummary) -> String {
    let normalized = row.name.to_ascii_lowercase();
    if normalized.contains("ultraskill") || normalized.contains("ultimate") {
        "大招".to_owned()
    } else if normalized.contains("melee") || normalized.contains("normal") {
        "普攻".to_owned()
    } else if normalized.contains("qte") {
        "环合".to_owned()
    } else if normalized.contains("skill") {
        "技能".to_owned()
    } else if contains_cjk(&row.category) && row.category != "未归类" {
        row.category.clone()
    } else {
        "技能".to_owned()
    }
}

pub(crate) fn draw_skill_glyph(ui: &mut egui::Ui, rect: egui::Rect, color: Color32, index: usize) {
    ui.painter()
        .rect_filled(rect, 8.0, color.gamma_multiply(0.82));
    let label = (index + 1).min(99).to_string();
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(14.0),
        contrast_text(color),
    );
}

pub(crate) fn draw_history_skill_rows(
    ui: &mut egui::Ui,
    title: &str,
    rows: &[CombatSessionSkillSummary],
    dark_mode: bool,
) {
    history_section_heading(ui, title, semantic_success(dark_mode));
    ui.add_space(4.0);
    for (index, row) in rows.iter().take(6).enumerate() {
        let color = skill_row_color(row, index, dark_mode);
        draw_history_progress_row(
            ui,
            color,
            (row.damage_share_percent / 100.0) as f32,
            48.0,
            |ui, content_rect| {
                let glyph_rect = egui::Rect::from_center_size(
                    egui::pos2(content_rect.left() + 16.0, content_rect.center().y),
                    egui::vec2(30.0, 30.0),
                );
                draw_skill_glyph(ui, glyph_rect, color, index);
                ui.add_space(36.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(skill_display_name(row))
                            .strong()
                            .color(shadcn_foreground(dark_mode)),
                    );
                    ui.label(
                        RichText::new(tf("{} hits", &[&row.hits.to_string()]))
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.1}%", row.damage_share_percent))
                            .strong()
                            .color(color),
                    );
                    ui.monospace(format_number(row.damage));
                });
            },
        );
        ui.add_space(4.0);
    }
}
