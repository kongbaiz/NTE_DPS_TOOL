use super::*;

pub(crate) enum UiConfigSavePlan {
    NoChange,
    SetPending((UiConfig, Instant)),
    KeepPending((UiConfig, Instant)),
    Save(UiConfig),
}

pub(crate) fn passthrough_egui_key(hotkey: PassthroughHotkey) -> egui::Key {
    match hotkey {
        PassthroughHotkey::Home => egui::Key::Home,
        PassthroughHotkey::Insert => egui::Key::Insert,
        PassthroughHotkey::F8 => egui::Key::F8,
        PassthroughHotkey::F9 => egui::Key::F9,
    }
}

pub(crate) fn stable_selectable_value<'a, Value: PartialEq>(
    ui: &mut egui::Ui,
    current_value: &mut Value,
    selected_value: Value,
    text: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let mut response = ui.add(
        egui::Button::selectable(*current_value == selected_value, text).frame_when_inactive(true),
    );
    if response.clicked() && *current_value != selected_value {
        *current_value = selected_value;
        response.mark_changed();
    }
    response
}

pub(crate) fn stable_popup_selectable_value<'a, Value: PartialEq>(
    ui: &mut egui::Ui,
    current_value: &mut Value,
    selected_value: Value,
    text: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let mut style = (**ui.style()).clone();
    style.visuals.widgets.inactive.bg_stroke =
        Stroke::new(0.0, style.visuals.widgets.inactive.bg_stroke.color);
    ui.scope(|ui| {
        ui.set_style(style);
        ui.selectable_value(current_value, selected_value, text)
    })
    .inner
}

pub(crate) fn inline_controls<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope(|ui| {
        let mut style = (**ui.style()).clone();
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::proportional(INLINE_CONTROL_TEXT_SIZE),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::proportional(INLINE_CONTROL_TEXT_SIZE),
        );
        ui.set_style(style);
        ui.spacing_mut().interact_size.y = INLINE_CONTROL_HEIGHT;
        ui.spacing_mut().button_padding = egui::vec2(10.0, 3.0);
        // Wrapped, not a plain horizontal: localized labels/buttons vary in width per
        // language, so a row that fits in Chinese can overflow in English. Wrapping
        // pushes the overflow to a second line instead of letting it spill past the
        // card/window edge and get clipped.
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(INLINE_CONTROL_HEIGHT);
            add_contents(ui)
        })
        .inner
    })
    .inner
}

pub(crate) fn inline_text(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text)
        .size(INLINE_CONTROL_TEXT_SIZE)
        .color(color)
}

pub(crate) fn file_display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_owned())
        .unwrap_or_else(|| t("the selected file"))
}

/// Returns `(title_key, localized_message, confirm_key)`. Title and confirm are
/// English keys (wrap with [`t`] at the display site); the message is already
/// localized here because it interpolates a runtime path/id.
pub(crate) fn confirmation_content(
    action: &ConfirmationAction,
) -> (&'static str, String, &'static str) {
    match action {
        ConfirmationAction::StartLive => (
            "Confirm Start",
            t("Starting live capture clears the current stats and re-detects the game connection."),
            "Start",
        ),
        ConfirmationAction::ResetSession => (
            "Confirm Reset",
            t(
                "This stops the current task and clears this session's stats, abyss state and detail caches.",
            ),
            "Reset",
        ),
        ConfirmationAction::ImportPcapng(path) => (
            "Confirm Import",
            tf(
                "Importing {} stops the current task and clears existing stats.",
                &[&file_display_name(path)],
            ),
            "Import",
        ),
        ConfirmationAction::ImportCaptureJson(path) => (
            "Confirm Import",
            tf(
                "Importing {} stops the current task and clears existing stats.",
                &[&file_display_name(path)],
            ),
            "Import",
        ),
        ConfirmationAction::ClearEncryptedIni => (
            "Confirm Clear",
            t("The current INI has unsaved changes; clearing loses them."),
            "Clear",
        ),
        ConfirmationAction::ReloadEncryptedIni(path) => (
            "Confirm Reload",
            tf(
                "The current INI has unsaved changes; reloading {} loses them.",
                &[&file_display_name(path)],
            ),
            "Reload",
        ),
        ConfirmationAction::DeleteHistory(record_id) => (
            "Confirm Delete",
            tf(
                "Deleting history summary {} cannot be undone.",
                &[record_id],
            ),
            "Delete",
        ),
        ConfirmationAction::ClearCaptureLogs => (
            "Confirm Clear",
            t(
                "This deletes every raw capture file (nte_raw_*.pcapng) under logs; a file in use during capture is kept. This cannot be undone.",
            ),
            "Clear",
        ),
    }
}

/// English key; wrap with [`t`] at the display site.
pub(crate) fn error_action_label(action: ErrorAction) -> &'static str {
    match action {
        ErrorAction::RefreshNetwork => "Re-detect",
        ErrorAction::OpenPcapng => "Reselect PCAPNG",
        ErrorAction::OpenCaptureJson => "Reselect JSON",
        ErrorAction::OpenEncryptedIni => "Reselect INI",
        ErrorAction::OpenTeamDpsImport => "Reselect DPS Data",
        ErrorAction::OpenConsole => "Open Console",
    }
}

pub(crate) fn import_error_action(error: &str) -> Option<ErrorAction> {
    let lower = error.to_ascii_lowercase();
    if lower.contains("pcapng") {
        Some(ErrorAction::OpenPcapng)
    } else if lower.contains("json") {
        Some(ErrorAction::OpenCaptureJson)
    } else {
        None
    }
}

pub(crate) fn humanize_engine_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    const PCAPNG_PREFIX: &str = "pcapng import failed:";
    const JSON_PREFIX: &str = "json import failed:";
    if lower.starts_with(PCAPNG_PREFIX) {
        let reason = error[PCAPNG_PREFIX.len()..].trim();
        return tf("PCAPNG import failed: {}", &[reason.trim()]);
    }
    if lower.starts_with(JSON_PREFIX) {
        let reason = error[JSON_PREFIX.len()..].trim();
        return tf("Capture JSON import failed: {}", &[reason.trim()]);
    }
    error.to_owned()
}

/// System CJK fonts tried in order. The whole UI is Chinese, so without a CJK face every label
/// renders as tofu. Microsoft YaHei is the preferred match; the rest are fallbacks that ship on
/// common Windows editions (including ones where YaHei was removed or replaced).
pub(crate) const CJK_FONT_CANDIDATES: &[&str] = &[
    "msyh.ttc",   // Microsoft YaHei (Win 8+)
    "msyh.ttf",   // Microsoft YaHei (older layout)
    "msyhl.ttc",  // Microsoft YaHei Light
    "simsun.ttc", // SimSun / NSimSun
    "simhei.ttf", // SimHei
    "Deng.ttf",   // DengXian
];

pub(crate) fn install_fonts(ctx: &egui::Context) {
    let windows_dir = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let fonts_dir = windows_dir.join("Fonts");
    let Some(bytes) = CJK_FONT_CANDIDATES
        .iter()
        .find_map(|name| std::fs::read(fonts_dir.join(name)).ok())
    else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "system-cjk".to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "system-cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Invisible edge/corner drag handles that give a borderless window native OS resize.
///
/// The window has no OS chrome (`with_decorations(false)`), so there is no native resize border.
/// This paints eight thin, frameless [`egui::Area`]s (four edges + four corners) along the
/// viewport's edges; dragging one hands off to the OS resize loop via
/// [`egui::ViewportCommand::BeginResize`] (winit `drag_resize_window`). Unlike the retired
/// `−／＋` stepper this never streams `InnerSize`, but the handoff to the OS resize loop is
/// exactly what egui #4061 / #5460 crash on: a fast corner drag on a transparent borderless
/// window. Under the glow backend that killed the process via an NVIDIA OpenGL context loss
/// with no Rust panic, so the app renders with wgpu (see `main`) to dodge that driver path.
///
/// Each grip is its own `Area`, so its interaction rect is just the strip: the window center
/// still routes pointer events to the panels underneath. Works for the root viewport and any
/// child viewport because [`egui::Context::content_rect`] returns that viewport's inner rect.
pub(crate) fn window_resize_grips(ctx: &egui::Context) {
    use egui::CursorIcon;
    use egui::viewport::ResizeDirection as Dir;

    const EDGE: f32 = 6.0;
    const CORNER: f32 = 12.0;
    let r = ctx.content_rect();
    if r.width() <= 2.0 * CORNER || r.height() <= 2.0 * CORNER {
        return;
    }
    let rect = egui::Rect::from_min_size;
    let grips: [(&str, egui::Rect, Dir, CursorIcon); 8] = [
        // Corners first so they win the shared pixels with the edges.
        (
            "nw",
            rect(r.left_top(), egui::vec2(CORNER, CORNER)),
            Dir::NorthWest,
            CursorIcon::ResizeNwSe,
        ),
        (
            "ne",
            rect(
                egui::pos2(r.right() - CORNER, r.top()),
                egui::vec2(CORNER, CORNER),
            ),
            Dir::NorthEast,
            CursorIcon::ResizeNeSw,
        ),
        (
            "sw",
            rect(
                egui::pos2(r.left(), r.bottom() - CORNER),
                egui::vec2(CORNER, CORNER),
            ),
            Dir::SouthWest,
            CursorIcon::ResizeNeSw,
        ),
        (
            "se",
            rect(
                egui::pos2(r.right() - CORNER, r.bottom() - CORNER),
                egui::vec2(CORNER, CORNER),
            ),
            Dir::SouthEast,
            CursorIcon::ResizeNwSe,
        ),
        (
            "n",
            rect(
                egui::pos2(r.left() + CORNER, r.top()),
                egui::vec2(r.width() - 2.0 * CORNER, EDGE),
            ),
            Dir::North,
            CursorIcon::ResizeVertical,
        ),
        (
            "s",
            rect(
                egui::pos2(r.left() + CORNER, r.bottom() - EDGE),
                egui::vec2(r.width() - 2.0 * CORNER, EDGE),
            ),
            Dir::South,
            CursorIcon::ResizeVertical,
        ),
        (
            "w",
            rect(
                egui::pos2(r.left(), r.top() + CORNER),
                egui::vec2(EDGE, r.height() - 2.0 * CORNER),
            ),
            Dir::West,
            CursorIcon::ResizeHorizontal,
        ),
        (
            "e",
            rect(
                egui::pos2(r.right() - EDGE, r.top() + CORNER),
                egui::vec2(EDGE, r.height() - 2.0 * CORNER),
            ),
            Dir::East,
            CursorIcon::ResizeHorizontal,
        ),
    ];
    for (id, grip_rect, direction, cursor) in grips {
        let response = egui::Area::new(egui::Id::new(("window_resize_grip", id)))
            .order(egui::Order::Foreground)
            .fixed_pos(grip_rect.min)
            .movable(false)
            .constrain(false)
            .sense(egui::Sense::drag())
            .show(ctx, |ui| {
                ui.allocate_space(grip_rect.size());
            })
            .response
            .on_hover_and_drag_cursor(cursor);
        // Mirrors the title-bar `StartDrag`: fire on drag-start while the button is still down so
        // winit's native resize loop takes over immediately.
        if response.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

/// Records a viewport's current inner size (logical points) into `target`, ignoring degenerate or
/// sub-pixel changes. Callers persist `target` through the normal debounced config path, so a
/// window reopens at the size it was last dragged to.
pub(crate) fn track_window_size(ctx: &egui::Context, target: &mut egui::Vec2) {
    let size = ctx.content_rect().size();
    if size.x.is_finite()
        && size.y.is_finite()
        && size.x >= 1.0
        && size.y >= 1.0
        && (size - *target).length() > 0.5
    {
        *target = size;
    }
}

/// Which native window control a [`window_control_button`] paints.
#[derive(Clone, Copy)]
pub(crate) enum WindowControlIcon {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// A frameless, fixed-size window-control button with a hand-painted glyph.
///
/// The glyphs are painted (not font characters) so they render identically
/// regardless of which CJK fallback font [`install_fonts`] picks — the box /
/// restore shapes are not reliably present in every Windows system font. Close
/// tints red on hover to match the native title bar; the others brighten.
pub(crate) fn window_control_button(
    ui: &mut egui::Ui,
    icon: WindowControlIcon,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(TITLE_BAR_BUTTON_SIZE, egui::Sense::click());
    let color = match (icon, response.hovered()) {
        (WindowControlIcon::Close, true) => Color32::from_rgb(232, 76, 76),
        (_, true) => ui.visuals().strong_text_color(),
        (_, false) => ui.visuals().text_color(),
    };
    paint_window_control_icon(ui.painter(), rect, icon, color, ui.visuals().panel_fill);
    response.on_hover_text(tooltip.to_owned())
}

fn paint_window_control_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: WindowControlIcon,
    color: Color32,
    bg: Color32,
) {
    let center = rect.center().round();
    let stroke = Stroke::new(1.2, color);
    match icon {
        WindowControlIcon::Minimize => {
            painter.hline(center.x - 5.0..=center.x + 5.0, center.y, stroke);
        }
        WindowControlIcon::Maximize => {
            let square = egui::Rect::from_center_size(center, egui::vec2(9.0, 9.0));
            painter.rect_stroke(square, 0.0, stroke, egui::StrokeKind::Inside);
        }
        WindowControlIcon::Restore => {
            const SIDE: f32 = 8.0;
            const OFFSET: f32 = 2.0;
            // Back square peeks out to the top-right; the front square is filled
            // with the bar's own color first so it cleanly occludes the overlap,
            // giving the classic two-square "restore" look.
            let back = egui::Rect::from_min_size(
                egui::pos2(
                    center.x - SIDE * 0.5 + OFFSET,
                    center.y - SIDE * 0.5 - OFFSET,
                ),
                egui::vec2(SIDE, SIDE),
            );
            painter.rect_stroke(back, 0.0, stroke, egui::StrokeKind::Inside);
            let front = egui::Rect::from_min_size(
                egui::pos2(
                    center.x - SIDE * 0.5 - OFFSET,
                    center.y - SIDE * 0.5 + OFFSET,
                ),
                egui::vec2(SIDE, SIDE),
            );
            painter.rect_filled(front, 0.0, bg);
            painter.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Inside);
        }
        WindowControlIcon::Close => {
            let half = 5.0;
            painter.line_segment(
                [
                    egui::pos2(center.x - half, center.y - half),
                    egui::pos2(center.x + half, center.y + half),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - half, center.y + half),
                    egui::pos2(center.x + half, center.y - half),
                ],
                stroke,
            );
        }
    }
}

/// Shared borderless, always-on-top, resizable [`egui::ViewportBuilder`] for a
/// secondary window (console and the detail panels).
///
/// `already_open` must be the window's "first-frame setup done" flag (its
/// `*_corner_applied`): the persisted `size` is written into the builder only on the
/// opening frame. Re-passing `inner_size` every frame would fight the user's live
/// resize — [`egui::ViewportBuilder::patch`] re-sends `InnerSize` whenever the builder
/// value changes, and [`track_window_size`] feeds the DPI-rounded OS size straight
/// back into that field, so requesting it again rounds again: a resize→round→resize
/// loop seen as window-edge jitter. After the first frame the builder omits
/// `inner_size` (patch sends nothing) and the OS owns the size; `track_window_size`
/// still records it for persistence.
pub(crate) fn secondary_viewport_builder(
    title: impl Into<String>,
    size: egui::Vec2,
    min_size: [f32; 2],
    already_open: bool,
) -> egui::ViewportBuilder {
    let mut builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_min_inner_size(egui::Vec2::from(min_size))
        .with_window_level(egui::WindowLevel::AlwaysOnTop)
        .with_decorations(false)
        // Borderless but freely resizable via the edge grips (window_resize_grips).
        .with_resizable(true);
    if !already_open {
        builder = builder.with_inner_size(size);
    }
    builder
}

/// Renders the top title-bar strip shared by every secondary window, wrapping
/// [`secondary_title_bar`] in the same frame the main window uses: a subtle
/// panel-fill tone with no border stroke, so there is no hard divider line under the
/// title (the strokes here used to read as an abrupt full-width rule). Returns whether
/// the close button was clicked. `ui` is the viewport's root ui (the immediate-viewport
/// callback's first argument). Safe to reuse the same panel id across windows — each
/// secondary window is its own viewport with an independent ui tree.
pub(crate) fn secondary_title_panel(ui: &mut egui::Ui, title: &str) -> bool {
    let mut close_clicked = false;
    egui::Panel::top("secondary_title_bar")
        .exact_size(34.0)
        .frame(
            egui::Frame::new()
                .fill(ui.style().visuals.panel_fill)
                .inner_margin(egui::Margin::symmetric(10, 3)),
        )
        .show_inside(ui, |ui| {
            close_clicked = secondary_title_bar(ui, title);
        });
    close_clicked
}

pub(crate) fn secondary_title_bar(ui: &mut egui::Ui, title: &str) -> bool {
    let title_height = 28.0;
    let mut close_clicked = false;
    // Whole bar drags the window; buttons drawn on top win their own clicks. See
    // the matching note in `title_bar` — keeps the bar draggable on any empty
    // area regardless of how the controls pack in.
    let (full_rect, title_drag) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), title_height),
        egui::Sense::click_and_drag(),
    );
    if title_drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    // Controls first (their natural, localized width may vary), title truncates
    // into whatever's left — see the matching note in `title_bar`.
    let mut controls = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(full_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    controls.set_clip_rect(full_rect);
    {
        let ui = &mut controls;
        ui.spacing_mut().item_spacing.x = 2.0;
        // Native window controls, matching the main title bar: minimize ·
        // maximize/restore · close (painted glyphs so they render regardless of font).
        if window_control_button(ui, WindowControlIcon::Close, &t("Close")).clicked() {
            close_clicked = true;
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

    let title_rect = egui::Rect::from_min_max(
        full_rect.min,
        egui::pos2((controls_left - 6.0).max(full_rect.left()), full_rect.max.y),
    );
    let mut left = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(title_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    left.set_clip_rect(title_rect);
    left.add_sized(
        title_rect.size(),
        egui::Label::new(
            RichText::new(title)
                .size(13.0)
                .strong()
                .color(left.visuals().text_color()),
        )
        .truncate(),
    );
    close_clicked
}

pub(crate) fn console_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_debug_viewport")
}

pub(crate) fn hit_detail_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_hit_detail_viewport")
}

pub(crate) fn team_hit_detail_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_team_hit_detail_viewport")
}

pub(crate) fn abyss_overview_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nte_abyss_overview_viewport")
}
