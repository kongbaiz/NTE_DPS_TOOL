use super::*;

pub(crate) fn is_qte_follow_up_damage_type(attack_type: &str) -> bool {
    matches!(
        attack_type,
        "创生花" | "覆纹" | "延滞" | "黯星" | "浊燃" | "浸染" | "盈蓄" | "失谐"
    )
}

pub(crate) fn is_qte_follow_up_damage_hit(hit: &crate::engine::model::Hit) -> bool {
    hit.follow_up_attack_type
        .as_deref()
        .is_some_and(is_qte_follow_up_damage_type)
        || (!hit.char_known
            && hit
                .attack_type
                .as_deref()
                .is_some_and(is_qte_follow_up_damage_type))
}

pub(crate) fn reaction_text_key_for_hit(hit: &crate::engine::model::Hit) -> Option<u8> {
    hit.attack_type
        .as_deref()
        .and_then(reaction_text_key_from_trigger_attack_type)
}

pub(crate) fn reaction_text_key_from_trigger_attack_type(attack_type: &str) -> Option<u8> {
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

pub(crate) fn hit_detail_hover_text(
    hit: &crate::engine::model::Hit,
    include_character: bool,
) -> String {
    let mut lines = Vec::new();
    if include_character {
        lines.push(format!("{} · {}", hit.char_name, t(hit_type_label(hit))));
    } else {
        lines.push(t(hit_type_label(hit)));
    }
    if hit.follow_up_damage > 0.0 {
        lines.push(tf(
            "Damage: {} + {}",
            &[
                &format_number(hit.damage),
                &format_number(hit.follow_up_damage),
            ],
        ));
    } else {
        lines.push(tf("Damage: {}", &[&format_number(hit.damage)]));
    }
    if hit.target_max_hp > 0.0 {
        lines.push(tf(
            "Target HP: {} / {}  {}%",
            &[
                &format_number(hit.target_hp_after),
                &format_number(hit.target_max_hp),
                &format!("{:.1}", hit.target_hp_percent),
            ],
        ));
    }
    if hit.direction == "unknown" {
        lines.push(t("Direction not yet confirmed"));
    } else if let Some(ability_name) = hit.ability_name.as_deref() {
        lines.push(format!("GA：{ability_name}"));
    }
    lines.join("\n")
}

pub(crate) fn aggregate_character_skill_damage(
    hits: &std::collections::VecDeque<crate::engine::model::Hit>,
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

pub(crate) fn summarize_qte_type_filters(
    hits: &VecDeque<crate::engine::model::Hit>,
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

pub(crate) fn hit_detail_filter_available(
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
pub(crate) fn qte_type_filter_label(summary: &QteTypeFilterSummary, total_damage: f64) -> String {
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

pub(crate) fn draw_qte_damage_summary(
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
        ui.add(
            egui::Label::new(
                RichText::new(t("Reaction Damage"))
                    .strong()
                    .color(ui.visuals().weak_text_color()),
            )
            .selectable(false),
        );
        for summary in qte_summaries {
            qte_damage_summary_chip(ui, summary, total_damage, selected);
        }
    });
}

pub(crate) fn qte_damage_summary_chip(
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
        .on_hover_text(tf(
            "{} hits · total damage {} · {}% of total",
            &[
                &summary.hits.to_string(),
                &format_number(summary.damage),
                &format!("{share:.1}"),
            ],
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

pub(crate) fn detail_hits_for_source(
    state: &CombatState,
    source: HitDetailSource,
) -> &VecDeque<crate::engine::model::Hit> {
    match source {
        HitDetailSource::Global => &state.hits,
        HitDetailSource::AbyssFirst => &state.abyss.first_half.hits,
        HitDetailSource::AbyssSecond => &state.abyss.second_half.hits,
    }
}

pub(crate) fn build_hit_detail_cache(
    hits: &VecDeque<crate::engine::model::Hit>,
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

pub(crate) fn cached_hit_row(index: usize, hit: &crate::engine::model::Hit) -> CachedHitRow {
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

pub(crate) fn resolve_cached_hit<'a>(
    hits: &'a VecDeque<crate::engine::model::Hit>,
    row: &CachedHitRow,
    source_len: usize,
    appended: u64,
) -> Option<&'a crate::engine::model::Hit> {
    let appended = usize::try_from(appended).unwrap_or(usize::MAX);
    adjusted_cached_index(row.index, source_len, hits.len(), appended)
        .and_then(|index| hits.get(index))
        .filter(|hit| cached_hit_matches(row, hit))
        .or_else(|| {
            hits.get(row.index)
                .filter(|hit| cached_hit_matches(row, hit))
        })
}

pub(crate) fn cached_hit_matches(row: &CachedHitRow, hit: &crate::engine::model::Hit) -> bool {
    row.char_id == hit.char_id
        && (row.timestamp - hit.timestamp).abs() <= 0.001
        && row.byte_offset == hit.byte_offset
        && row.bit_shift == hit.bit_shift
        && (row.target_max_hp - hit.target_max_hp).abs() <= 0.5
}

pub(crate) fn adjusted_cached_index(
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

pub(crate) fn compare_cached_character_hits(
    left: &CachedHitRow,
    right: &CachedHitRow,
) -> std::cmp::Ordering {
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

pub(crate) fn compare_cached_team_hits(
    left: &CachedHitRow,
    right: &CachedHitRow,
) -> std::cmp::Ordering {
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

pub(crate) fn cached_health_pool_key(row: &CachedHitRow) -> i64 {
    if row.target_max_hp.is_finite() && row.target_max_hp > 0.0 {
        row.target_max_hp.round() as i64
    } else {
        i64::MIN
    }
}

pub(crate) fn draw_skill_damage_summary(
    ui: &mut egui::Ui,
    summaries: &[SkillDamageSummary],
    total_damage: f64,
    selected_skill: &mut String,
    dark_mode: bool,
) {
    egui::CollapsingHeader::new(
        RichText::new(t("Move Output Composition"))
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
            t("Specific Move"),
            header_font.clone(),
            header_color,
        );
        ui.painter().text(
            header_rect.right_center() - egui::vec2(10.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            t("Share / Total / Count"),
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
                            "{share:.1}%  ·  {}  ·  {}",
                            format_number(summary.damage),
                            tf("{} hits", &[&summary.hits.to_string()])
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
pub(crate) struct CharacterHitLayout {
    row_width: f32,
    time_x: f32,
    type_x: f32,
    type_width: f32,
    damage_x: f32,
    hp_x: f32,
    separators: [f32; 3],
}

#[derive(Clone, Copy)]
pub(crate) struct TeamHitLayout {
    row_width: f32,
    time_x: f32,
    character_x: f32,
    type_x: f32,
    type_width: f32,
    damage_x: f32,
    hp_x: f32,
    separators: [f32; 4],
}

pub(crate) struct TeamHitRowAssets<'a> {
    pub(crate) avatar_texture: Option<&'a egui::TextureHandle>,
    pub(crate) damage_digits: Option<&'a [egui::TextureHandle]>,
    pub(crate) follow_up_damage_digits: Option<&'a [egui::TextureHandle]>,
    pub(crate) reaction_textures: &'a HashMap<u8, Vec<egui::TextureHandle>>,
}

impl TeamHitLayout {
    pub(crate) fn new(available_width: f32) -> Self {
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
    pub(crate) fn new(available_width: f32) -> Self {
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

pub(crate) fn draw_character_hit_header(ui: &mut egui::Ui, layout: CharacterHitLayout) {
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
        t("Time"),
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.type_x, y),
        egui::Align2::LEFT_CENTER,
        t("Type"),
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.damage_x, y),
        egui::Align2::LEFT_CENTER,
        t("Damage"),
        font.clone(),
        color,
    );
    painter.text(
        egui::pos2(x + layout.hp_x, y),
        egui::Align2::LEFT_CENTER,
        t("Target / HP"),
        font,
        color,
    );
}

pub(crate) fn damage_digit_textures_for_hit<'a>(
    hit: &crate::engine::model::Hit,
    characters: &HashMap<u32, CharacterInfo>,
    damage_digit_textures: &'a HashMap<String, Vec<egui::TextureHandle>>,
) -> Option<&'a [egui::TextureHandle]> {
    damage_digit_key_for_hit(hit, characters)
        .and_then(|key| damage_digit_textures.get(key))
        .map(Vec::as_slice)
}

pub(crate) fn follow_up_damage_digit_textures_for_hit<'a>(
    hit: &crate::engine::model::Hit,
    damage_digit_textures: &'a HashMap<String, Vec<egui::TextureHandle>>,
) -> Option<&'a [egui::TextureHandle]> {
    follow_up_damage_digit_key_for_hit(hit)
        .and_then(|key| damage_digit_textures.get(key))
        .map(Vec::as_slice)
}

pub(crate) fn damage_digit_key_for_hit<'a>(
    hit: &'a crate::engine::model::Hit,
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

pub(crate) fn follow_up_damage_digit_key_for_hit(hit: &crate::engine::model::Hit) -> Option<&str> {
    let source_attribute = hit.follow_up_damage_attribute.as_deref()?;
    hit.follow_up_attack_type
        .as_deref()
        .and_then(|attack_type| mixed_damage_digit_key(attack_type, Some(source_attribute)))
        .or(Some(source_attribute))
}

pub(crate) fn mixed_damage_digit_key(
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

pub(crate) fn draw_damage_number(
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

pub(crate) fn damage_number_image_width(
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

pub(crate) fn damage_number_digits_text(value: f64) -> String {
    let rounded = value.round() as i64;
    if rounded < 0 {
        format!("-{}", rounded.unsigned_abs())
    } else {
        rounded.to_string()
    }
}

pub(crate) fn draw_damage_number_fallback(
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
    let width = ui.fonts_mut(|fonts| {
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

pub(crate) fn draw_follow_up_damage_badge(
    ui: &egui::Ui,
    damage_cell_rect: egui::Rect,
    base_damage_rect: egui::Rect,
    hit: &crate::engine::model::Hit,
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

pub(crate) fn draw_character_hit_row(
    ui: &mut egui::Ui,
    layout: CharacterHitLayout,
    hit: &crate::engine::model::Hit,
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

pub(crate) fn draw_team_hit_header(ui: &mut egui::Ui, layout: TeamHitLayout) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(layout.row_width, 24.0), egui::Sense::hover());
    let y = rect.center().y;
    let x = rect.left();
    let painter = ui.painter();
    let font = egui::FontId::proportional(12.0);
    let color = ui.visuals().weak_text_color();
    draw_team_hit_column_separators(painter, rect, layout);

    for (offset, label) in [
        (layout.time_x, t("Time")),
        (layout.character_x, t("Character")),
        (layout.type_x, t("Type")),
        (layout.damage_x, t("Damage")),
        (layout.hp_x, t("Target / HP")),
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

pub(crate) fn draw_hit_type_badge_content(
    ui: &mut egui::Ui,
    badge_rect: egui::Rect,
    hit: &crate::engine::model::Hit,
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

pub(crate) fn draw_reaction_text_images(
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

pub(crate) fn draw_team_hit_row(
    ui: &mut egui::Ui,
    layout: TeamHitLayout,
    hit: &crate::engine::model::Hit,
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

pub(crate) fn draw_hit_metric_row(ui: &mut egui::Ui, metrics: [(&str, String, Color32); 5]) {
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

pub(crate) fn hit_metric_card_sized(
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

pub(crate) fn draw_clipped_label(
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

pub(crate) fn truncate_text_to_width(
    ui: &egui::Ui,
    text: &str,
    font: &egui::FontId,
    color: Color32,
    max_width: f32,
) -> String {
    let text_width = |value: &str| {
        ui.fonts_mut(|fonts| {
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

pub(crate) fn draw_target_hp_text(
    ui: &mut egui::Ui,
    cell_rect: egui::Rect,
    hit: &crate::engine::model::Hit,
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

pub(crate) fn draw_direction_summary(ui: &mut egui::Ui, summary: HitDirectionSummary) {
    ui.add_space(5.0);
    let text = tf(
        "Confirmed output {} ({} hits) · candidate output {} ({} hits, {}% of total output)",
        &[
            &format_number(summary.outgoing_damage),
            &summary.outgoing_hits.to_string(),
            &format_number(summary.unknown_damage),
            &summary.unknown_hits.to_string(),
            &format!("{:.1}", summary.unknown_share()),
        ],
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

pub(crate) fn draw_hit_column_separators(
    painter: &egui::Painter,
    rect: egui::Rect,
    layout: CharacterHitLayout,
) {
    let color = if painter.ctx().global_style().visuals.dark_mode {
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

pub(crate) fn draw_team_hit_column_separators(
    painter: &egui::Painter,
    rect: egui::Rect,
    layout: TeamHitLayout,
) {
    let color = if painter.ctx().global_style().visuals.dark_mode {
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
