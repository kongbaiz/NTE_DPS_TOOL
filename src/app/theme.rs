use super::*;

/// Character display name for the active UI language (`name_en` for English and any
/// non-Chinese language, `name_zh` for Simplified Chinese), falling back to
/// `fallback` when the character table has no usable entry for `char_id`. A free
/// function so the timeline, skills and history views — which render from a
/// `&HashMap<u32, CharacterInfo>` without a `DpsApp` handle — resolve names the same
/// way. Reads the active language from the shared i18n store.
pub(crate) fn character_display_name(
    characters: &HashMap<u32, CharacterInfo>,
    char_id: u32,
    fallback: &str,
) -> String {
    if let Some(info) = characters.get(&char_id) {
        let candidate = if i18n::current_language() == Language::SimplifiedChinese {
            info.name_zh.trim()
        } else {
            info.name_en.trim()
        };
        if !candidate.is_empty() {
            return candidate.to_owned();
        }
    }
    fallback.to_owned()
}

pub(crate) fn compact_metric(
    ui: &mut egui::Ui,
    label: &str,
    value: String,
    color: Color32,
    prominent: bool,
) {
    let id = ui.make_persistent_id(("compact_metric", label));
    let hovered = ui
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|pointer| ui.max_rect().contains(pointer));
    let hover = ui.ctx().animate_bool_with_time(id, hovered, 0.14);
    let fill = mix_color(
        shadcn_card(ui.visuals().dark_mode),
        shadcn_card_hover(ui.visuals().dark_mode),
        hover,
    );
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .stroke(Stroke::new(
            1.0,
            mix_color(
                shadcn_border(ui.visuals().dark_mode),
                theme_accent(ui.visuals().dark_mode).gamma_multiply(0.55),
                hover,
            ),
        ))
        .inner_margin(egui::Margin::symmetric(4, 4))
        .show(ui, |ui| {
            ui.set_min_height(38.0);
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(
                    RichText::new(value)
                        .size(if prominent { 17.0 } else { 15.0 })
                        .strong()
                        .color(color),
                );
                ui.label(
                    RichText::new(label)
                        .size(9.5)
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });
}

/// Height of each party row so the list fills the available vertical space (the
/// window is freely resizable, so the rows grow with it rather than leaving a large
/// empty gap under the last member). Only a lower bound is enforced — 38px keeps a
/// full roster readable when the window is short; there is no upper cap, so a few
/// members in a tall window stretch to fill it.
pub(crate) fn party_row_height(available_height: f32, row_count: usize) -> f32 {
    if row_count == 0 {
        return 52.0;
    }

    let spacing = 5.0 * row_count.saturating_sub(1) as f32;
    ((available_height - spacing - 2.0) / row_count as f32).max(38.0)
}

pub(crate) fn primary_button(label: impl Into<String>, dark_mode: bool) -> egui::Button<'static> {
    let fill = theme_accent(dark_mode);
    egui::Button::new(
        RichText::new(label.into())
            .strong()
            .color(contrast_text(fill)),
    )
    .fill(fill)
    .stroke(Stroke::new(1.0, fill))
}

/// Severity color for the live status text. The status string can be in either
/// language (app text is localized; engine-sent status stays Chinese), so both
/// languages' severity keywords are matched. ASCII is lowercased for
/// case-insensitive English matching; Chinese needles are unaffected.
pub(crate) fn status_color(status: &str, paused: bool, dark_mode: bool) -> Color32 {
    if paused {
        return semantic_warning(dark_mode);
    }
    let lower = status.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if has(&[
        "失败",
        "不可用",
        "未检测到",
        "failed",
        "unavailable",
        "not detected",
        "no game connection",
        "error",
    ]) {
        semantic_danger(dark_mode)
    } else if has(&[
        "正在",
        "启动",
        "导入",
        "处理",
        "starting",
        "importing",
        "processing",
        "capturing",
        "detecting",
        "...",
    ]) {
        semantic_warning(dark_mode)
    } else {
        semantic_success(dark_mode)
    }
}

/// Human-readable label for a capture NIC: its description (or raw name) plus any IPv4 addresses,
/// so users can disambiguate adapters — especially a VPN interface vs. the physical one.
pub(crate) fn capture_device_label(device: &CaptureDevice) -> String {
    let base = if device.description.is_empty() {
        device.name.as_str()
    } else {
        device.description.as_str()
    };
    if device.ipv4.is_empty() {
        base.to_owned()
    } else {
        let addresses = device
            .ipv4
            .iter()
            .map(|address| address.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{base} · {addresses}")
    }
}

pub(crate) fn semantic_success(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(74, 222, 128)
    } else {
        Color32::from_rgb(22, 128, 76)
    }
}

pub(crate) fn semantic_warning(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(250, 204, 21)
    } else {
        Color32::from_rgb(161, 98, 7)
    }
}

pub(crate) fn semantic_danger(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(248, 113, 113)
    } else {
        Color32::from_rgb(190, 55, 65)
    }
}

pub(crate) fn theme_accent(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(250, 250, 250)
    } else {
        Color32::from_rgb(24, 24, 27)
    }
}

pub(crate) fn hit_output_badge_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(63, 63, 70)
    } else {
        Color32::from_rgb(24, 24, 27)
    }
}

pub(crate) fn hit_output_text_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(244, 244, 245)
    } else {
        Color32::from_rgb(24, 24, 27)
    }
}

pub(crate) fn readable_accent(color: Color32, dark_mode: bool) -> Color32 {
    let luminance = 0.2126 * f32::from(color.r())
        + 0.7152 * f32::from(color.g())
        + 0.0722 * f32::from(color.b());
    if !dark_mode && luminance > 210.0 {
        Color32::from_rgb(82, 82, 91)
    } else if dark_mode && luminance < 52.0 {
        Color32::from_rgb(161, 161, 170)
    } else {
        color
    }
}

pub(crate) fn contrast_text(background: Color32) -> Color32 {
    let luminance = 0.2126 * f32::from(background.r())
        + 0.7152 * f32::from(background.g())
        + 0.0722 * f32::from(background.b());
    if luminance > 150.0 {
        Color32::from_rgb(9, 9, 11)
    } else {
        Color32::from_rgb(250, 250, 250)
    }
}

pub(crate) fn shadcn_background(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(9, 9, 11)
    } else {
        Color32::from_rgb(250, 250, 250)
    }
}

pub(crate) fn shadcn_foreground(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(250, 250, 250)
    } else {
        Color32::from_rgb(9, 9, 11)
    }
}

pub(crate) fn shadcn_card(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(24, 24, 27)
    } else {
        Color32::from_rgb(255, 255, 255)
    }
}

pub(crate) fn shadcn_card_hover(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(31, 31, 35)
    } else {
        Color32::from_rgb(248, 248, 249)
    }
}

pub(crate) fn shadcn_muted(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(39, 39, 42)
    } else {
        Color32::from_rgb(228, 228, 231)
    }
}

pub(crate) fn shadcn_border(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(39, 39, 42)
    } else {
        Color32::from_rgb(228, 228, 231)
    }
}

pub(crate) fn mix_color(from: Color32, to: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |from: u8, to: u8| {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * amount).round() as u8
    };
    Color32::from_rgba_unmultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

pub(crate) fn ease_out_cubic(value: f32) -> f32 {
    1.0 - (1.0 - value.clamp(0.0, 1.0)).powi(3)
}

pub(crate) fn format_number(value: f64) -> String {
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

pub(crate) fn format_time(timestamp: f64) -> String {
    DateTime::<Local>::from(std::time::UNIX_EPOCH + Duration::from_secs_f64(timestamp.max(0.0)))
        .format("%H:%M:%S%.3f")
        .to_string()
}

pub(crate) fn format_short_time(timestamp: f64) -> String {
    DateTime::<Local>::from(std::time::UNIX_EPOCH + Duration::from_secs_f64(timestamp.max(0.0)))
        .format("%H:%M:%S")
        .to_string()
}

pub(crate) fn show_detail_limit_notice(ui: &mut egui::Ui, filtered_count: usize) {
    if filtered_count > MAX_DETAIL_HITS {
        ui.label(
            RichText::new(tf(
                "Showing the latest {} of {} matching rows; stats within the full retained range are already counted in the summary above.",
                &[
                    &format_number(MAX_DETAIL_HITS as f64),
                    &format_number(filtered_count as f64),
                ],
            ))
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(4.0);
    }
}

pub(crate) fn character_color(
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
    deterministic_character_fallback_color(format!("{char_id}:{fallback_index}").as_bytes())
}

pub(crate) fn parse_hex_color(value: &str) -> Option<Color32> {
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

pub(crate) fn data_root() -> PathBuf {
    if PathBuf::from(CHARACTER_DATA_PATH).is_file() {
        return PathBuf::from(".");
    }
    std::env::current_exe()
        .ok()
        .into_iter()
        .flat_map(|path| path.ancestors().map(PathBuf::from).collect::<Vec<_>>())
        .find(|path| path.join(CHARACTER_DATA_PATH).is_file())
        .unwrap_or_else(|| PathBuf::from("."))
}
