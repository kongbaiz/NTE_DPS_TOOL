use super::*;

pub(crate) struct IoFmtWriter<'a, W: IoWrite> {
    inner: &'a mut W,
    error: Option<String>,
}

impl<'a, W: IoWrite> IoFmtWriter<'a, W> {
    pub(crate) fn new(inner: &'a mut W) -> Self {
        Self { inner, error: None }
    }

    pub(crate) fn finish(self) -> Result<(), String> {
        self.error.map_or(Ok(()), Err)
    }
}

impl<W: IoWrite> std::fmt::Write for IoFmtWriter<'_, W> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.error.is_some() {
            return Err(std::fmt::Error);
        }
        if let Err(error) = self.inner.write_all(value.as_bytes()) {
            self.error = Some(error.to_string());
            return Err(std::fmt::Error);
        }
        Ok(())
    }
}

pub(crate) fn default_export_filename() -> String {
    format!("nte_capture_{}.json", Local::now().format("%Y%m%d_%H%M%S"))
}

pub(crate) fn json_option_time(value: Option<f64>) -> String {
    value
        .map(|timestamp| json_string(&format_time(timestamp)))
        .unwrap_or_else(|| "null".to_owned())
}

pub(crate) fn json_option_f64(value: Option<f64>) -> String {
    value.map(json_f64).unwrap_or_else(|| "null".to_owned())
}

pub(crate) fn json_f64(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.3}")
    } else {
        "null".to_owned()
    }
}

pub(crate) fn json_string(value: &str) -> String {
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

pub(crate) fn character_text_field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    dirty: &mut bool,
) {
    ui.label(label);
    if ui
        .add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY))
        .changed()
    {
        *dirty = true;
    }
    ui.end_row();
}

pub(crate) struct CharacterEditorCard<'a> {
    pub(crate) id: &'a str,
    pub(crate) name_zh: &'a str,
    pub(crate) name_en: &'a str,
    pub(crate) attribute: &'a str,
    pub(crate) avatar_texture: Option<&'a egui::TextureHandle>,
    pub(crate) selected: bool,
    pub(crate) fallback_color: Color32,
    pub(crate) dark_mode: bool,
}

pub(crate) fn draw_character_editor_card(
    ui: &mut egui::Ui,
    card: CharacterEditorCard<'_>,
) -> egui::Response {
    let size = egui::vec2(ui.available_width().max(1.0), CHARACTER_EDITOR_CARD_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let corner_radius = egui::CornerRadius::same(7);
    let fill = if card.selected {
        shadcn_muted(card.dark_mode)
    } else if response.hovered() {
        shadcn_card_hover(card.dark_mode)
    } else {
        shadcn_card(card.dark_mode)
    };
    let border_color = if card.selected {
        theme_accent(card.dark_mode)
    } else {
        shadcn_border(card.dark_mode)
    };
    ui.painter().rect(
        rect,
        corner_radius,
        fill,
        Stroke::new(if card.selected { 1.5 } else { 1.0 }, border_color),
        egui::StrokeKind::Inside,
    );

    let avatar_rect = egui::Rect::from_center_size(
        egui::pos2(
            rect.left() + 12.0 + CHARACTER_EDITOR_AVATAR_SIZE * 0.5,
            rect.center().y,
        ),
        egui::vec2(CHARACTER_EDITOR_AVATAR_SIZE, CHARACTER_EDITOR_AVATAR_SIZE),
    );
    let primary_name = character_editor_primary_name(card.name_zh, card.name_en, card.id);
    draw_character_editor_avatar(
        ui,
        avatar_rect,
        card.avatar_texture,
        card.fallback_color,
        &primary_name,
    );

    let text_rect = egui::Rect::from_min_max(
        egui::pos2(avatar_rect.right() + 12.0, rect.top() + 9.0),
        egui::pos2(rect.right() - 12.0, rect.bottom() - 9.0),
    );
    let secondary_line = character_editor_secondary_line(card.name_zh, card.name_en, card.id);
    let painter = ui.painter().with_clip_rect(text_rect);
    painter.text(
        egui::pos2(text_rect.left(), text_rect.top() + 15.0),
        egui::Align2::LEFT_CENTER,
        &primary_name,
        egui::FontId::proportional(16.0),
        shadcn_foreground(card.dark_mode),
    );
    painter.text(
        egui::pos2(text_rect.left(), text_rect.top() + 38.0),
        egui::Align2::LEFT_CENTER,
        secondary_line,
        egui::FontId::monospace(11.5),
        ui.visuals().weak_text_color(),
    );

    response.on_hover_text(character_editor_card_hover_text(
        card.id,
        card.name_zh,
        card.name_en,
        card.attribute,
    ))
}

pub(crate) fn draw_character_editor_avatar(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    texture: Option<&egui::TextureHandle>,
    fallback_color: Color32,
    fallback_text: &str,
) {
    if let Some(texture) = texture {
        ui.painter().rect_filled(rect, 8.0, ui.visuals().panel_fill);
        egui::Image::new((texture.id(), rect.size()))
            .corner_radius(8)
            .paint_at(ui, rect);
    } else {
        ui.painter()
            .rect_filled(rect, 8.0, fallback_color.gamma_multiply(0.85));
        if let Some(initial) = fallback_text.chars().next() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initial,
                egui::FontId::proportional(22.0),
                contrast_text(fallback_color),
            );
        }
    }
    ui.painter().rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, shadcn_border(ui.visuals().dark_mode)),
        egui::StrokeKind::Inside,
    );
}

pub(crate) fn character_editor_primary_name(name_zh: &str, name_en: &str, id: &str) -> String {
    let name_zh = name_zh.trim();
    let name_en = name_en.trim();
    if !name_zh.is_empty() {
        name_zh.to_owned()
    } else if !name_en.is_empty() {
        name_en.to_owned()
    } else {
        format!("ID {id}")
    }
}

pub(crate) fn character_editor_secondary_line(name_zh: &str, name_en: &str, id: &str) -> String {
    let name_zh = name_zh.trim();
    let name_en = name_en.trim();
    if !name_zh.is_empty() && !name_en.is_empty() && name_zh != name_en {
        format!("ID {id} · {name_en}")
    } else {
        format!("ID {id}")
    }
}

pub(crate) fn character_editor_card_hover_text(
    id: &str,
    name_zh: &str,
    name_en: &str,
    attribute: &str,
) -> String {
    let primary_name = character_editor_primary_name(name_zh, name_en, id);
    let mut text = format!("{primary_name}\nID {id}");
    let name_en = name_en.trim();
    if !name_en.is_empty() && name_en != primary_name {
        write!(&mut text, "\n{}", tf("English Name {}", &[name_en])).ok();
    }
    let attribute = attribute.trim();
    if !attribute.is_empty() {
        write!(&mut text, "\n{}", tf("Attribute {}", &[attribute])).ok();
    }
    text
}

pub(crate) fn next_search_match(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(current.map_or(0, |index| (index + 1) % len))
    }
}

pub(crate) fn previous_search_match(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(current.map_or(
            len - 1,
            |index| {
                if index == 0 { len - 1 } else { index - 1 }
            },
        ))
    }
}

pub(crate) fn encrypted_ini_match_cursor_range(
    text: &str,
    query: &str,
    match_index: usize,
) -> Option<egui::text::CCursorRange> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let start = byte_to_char_index(text, match_index);
    let end = byte_to_char_index(
        text,
        byte_index_after_chars(text, match_index, query.chars().count())
            .unwrap_or(match_index + query.len()),
    );
    Some(egui::text::CCursorRange::two(
        egui::text::CCursor::new(start),
        egui::text::CCursor::new(end),
    ))
}

pub(crate) fn encrypted_ini_match_byte_range(
    text: &str,
    query: &str,
    match_index: usize,
) -> Option<std::ops::Range<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let end = byte_index_after_chars(text, match_index, query.chars().count())
        .unwrap_or(match_index + query.len())
        .min(text.len());
    (match_index < end && text.is_char_boundary(match_index) && text.is_char_boundary(end))
        .then_some(match_index..end)
}

pub(crate) fn encrypted_ini_layout_galley(
    ui: &egui::Ui,
    request: EncryptedIniLayoutRequest<'_>,
    cache: &mut EncryptedIniLayoutCache,
) -> Arc<egui::Galley> {
    let text_color = ui.visuals().widgets.inactive.text_color();
    let query = request.query.trim();
    let highlight_query = if query.is_empty() || request.matches.is_empty() {
        ""
    } else {
        query
    };
    let current_match_byte = request
        .current_match_byte
        .filter(|_| !highlight_query.is_empty());
    let key = EncryptedIniLayoutCacheKey {
        text_len: request.text.len(),
        text_hash: encrypted_ini_text_fingerprint(request.text),
        query: highlight_query.to_owned(),
        current_match_byte,
        dark_mode: request.dark_mode,
        text_color,
    };
    if cache.key.as_ref() == Some(&key)
        && let Some(galley) = &cache.galley
    {
        return Arc::clone(galley);
    }

    let layout_job = encrypted_ini_layout_job(
        ui,
        request.text,
        highlight_query,
        request.matches,
        current_match_byte,
        request.wrap_width,
        request.dark_mode,
    );
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(layout_job));
    cache.key = Some(key);
    cache.galley = Some(Arc::clone(&galley));
    galley
}

pub(crate) fn encrypted_ini_layout_job(
    ui: &egui::Ui,
    text: &str,
    query: &str,
    matches: &[usize],
    current_match_byte: Option<usize>,
    _wrap_width: f32,
    dark_mode: bool,
) -> egui::text::LayoutJob {
    let text_color = ui.visuals().widgets.inactive.text_color();
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let base_format = egui::text::TextFormat {
        font_id,
        color: text_color,
        ..Default::default()
    };
    let mut match_format = base_format.clone();
    match_format.background = if dark_mode {
        Color32::from_rgb(82, 62, 12)
    } else {
        Color32::from_rgb(254, 240, 138)
    };
    match_format.color = if dark_mode {
        Color32::from_rgb(254, 249, 195)
    } else {
        Color32::from_rgb(63, 63, 70)
    };
    let mut current_format = match_format.clone();
    current_format.background = Color32::from_rgb(37, 99, 235);
    current_format.color = Color32::WHITE;

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut cursor = 0;
    for &start in matches {
        let Some(range) = encrypted_ini_match_byte_range(text, query, start) else {
            continue;
        };
        if range.start < cursor {
            continue;
        }
        if cursor < range.start {
            job.append(&text[cursor..range.start], 0.0, base_format.clone());
        }
        let format = if Some(range.start) == current_match_byte {
            current_format.clone()
        } else {
            match_format.clone()
        };
        job.append(&text[range.clone()], 0.0, format);
        cursor = range.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base_format);
    } else if text.is_empty() {
        job.append("", 0.0, base_format);
    }
    job
}

pub(crate) fn byte_to_char_index(text: &str, byte_index: usize) -> usize {
    text[..byte_index.min(text.len())].chars().count()
}

pub(crate) fn byte_index_after_chars(
    text: &str,
    byte_index: usize,
    char_count: usize,
) -> Option<usize> {
    let mut remaining = char_count;
    for (offset, ch) in text.get(byte_index..)?.char_indices() {
        if remaining == 0 {
            return Some(byte_index + offset);
        }
        remaining -= 1;
        if remaining == 0 {
            return Some(byte_index + offset + ch.len_utf8());
        }
    }
    (remaining == 0).then_some(text.len())
}

pub(crate) fn line_column_for_byte(text: &str, byte_index: usize) -> (usize, usize) {
    let prefix = &text[..byte_index.min(text.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, tail)| tail)
        .chars()
        .count()
        + 1;
    (line, column)
}
