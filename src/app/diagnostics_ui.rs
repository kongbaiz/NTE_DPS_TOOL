use super::*;

pub(crate) fn draw_resource_audit_row(
    ui: &mut egui::Ui,
    item: &ResourceAuditItem,
    dark_mode: bool,
) {
    let color = match item.severity {
        ResourceAuditSeverity::Error => semantic_danger(dark_mode),
        ResourceAuditSeverity::Warning => semantic_warning(dark_mode),
    };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
    let fill = if response.hovered() {
        shadcn_card_hover(dark_mode)
    } else {
        shadcn_card(dark_mode)
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    ui.painter().rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, shadcn_border(dark_mode)),
        egui::StrokeKind::Inside,
    );
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + 3.0, rect.bottom()),
        ),
        6.0,
        color,
    );
    let left = rect.left() + 10.0;
    let severity_rect = egui::Rect::from_min_max(
        egui::pos2(left, rect.top()),
        egui::pos2(left + 72.0, rect.bottom()),
    );
    ui.painter().text(
        severity_rect.center(),
        egui::Align2::CENTER_CENTER,
        item.severity.label(),
        egui::FontId::proportional(11.0),
        color,
    );
    let title_left = severity_rect.right() + 8.0;
    let right_width = 172.0;
    let title_clip = egui::Rect::from_min_max(
        egui::pos2(title_left, rect.top()),
        egui::pos2(rect.right() - right_width, rect.bottom()),
    );
    let title = format!(
        "{} · {} · {}",
        item.category.label(),
        item.resource_id,
        item.display_name
    );
    ui.painter().with_clip_rect(title_clip).text(
        egui::pos2(title_left, rect.center().y - 7.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(12.0),
        shadcn_foreground(dark_mode),
    );
    ui.painter().with_clip_rect(title_clip).text(
        egui::pos2(title_left, rect.center().y + 9.0),
        egui::Align2::LEFT_CENTER,
        &item.message,
        egui::FontId::proportional(10.0),
        ui.visuals().weak_text_color(),
    );
    ui.painter().text(
        egui::pos2(rect.right() - 10.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        &item.suggested_source,
        egui::FontId::monospace(10.0),
        ui.visuals().weak_text_color(),
    );
    response.on_hover_text(tf(
        "{}\n{}\nSuggested source: {}",
        &[&item.resource_id, &item.message, &item.suggested_source],
    ));
}

pub(crate) fn draw_diagnostic_report(
    ui: &mut egui::Ui,
    report: &DiagnosticReport,
    dark_mode: bool,
) {
    ui.columns(3, |columns| {
        compact_metric(
            &mut columns[0],
            &t("Failed"),
            report.failed_count().to_string(),
            semantic_danger(dark_mode),
            true,
        );
        compact_metric(
            &mut columns[1],
            &t("Warnings"),
            report.warning_count().to_string(),
            semantic_warning(dark_mode),
            true,
        );
        let check_color = columns[2].visuals().text_color();
        compact_metric(
            &mut columns[2],
            &t("Checks"),
            report.checks.len().to_string(),
            check_color,
            false,
        );
    });
    ui.add_space(6.0);
    for check in &report.checks {
        let color = match check.status {
            DiagnosticStatus::Passed => semantic_success(dark_mode),
            DiagnosticStatus::Warning => semantic_warning(dark_mode),
            DiagnosticStatus::Failed => semantic_danger(dark_mode),
        };
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
        let fill = if response.hovered() {
            shadcn_card_hover(dark_mode)
        } else {
            shadcn_card(dark_mode)
        };
        ui.painter().rect_filled(rect, 6.0, fill);
        ui.painter().rect_stroke(
            rect,
            6.0,
            Stroke::new(1.0, shadcn_border(dark_mode)),
            egui::StrokeKind::Inside,
        );
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                rect.left_top(),
                egui::pos2(rect.left() + 3.0, rect.bottom()),
            ),
            6.0,
            color,
        );
        let left = rect.left() + 10.0;
        ui.painter().text(
            egui::pos2(left, rect.center().y - 8.0),
            egui::Align2::LEFT_CENTER,
            format!("{} · {}", check.status.label(), check.title),
            egui::FontId::proportional(12.0),
            color,
        );
        ui.painter()
            .with_clip_rect(rect.shrink2(egui::vec2(10.0, 0.0)))
            .text(
                egui::pos2(left, rect.center().y + 10.0),
                egui::Align2::LEFT_CENTER,
                &check.suggestion,
                egui::FontId::proportional(10.5),
                ui.visuals().weak_text_color(),
            );
        response.on_hover_text(format!("{}\n{}", check.detail, check.suggestion));
    }
}

pub(crate) fn draw_capture_quality_summary(
    ui: &mut egui::Ui,
    summary: &CaptureQualitySummary,
    dark_mode: bool,
) {
    egui::CollapsingHeader::new(
        RichText::new(t("Parse Quality"))
            .strong()
            .color(shadcn_foreground(dark_mode)),
    )
    .default_open(true)
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(t(summary.source.label()))
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            if ui.button(t("Copy Redacted Report")).clicked() {
                ui.ctx().copy_text(summary.redacted_text());
            }
        });
        ui.add_space(4.0);
        egui::Grid::new("capture_quality_summary")
            .num_columns(4)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label(t("Packets"));
                ui.monospace(tf(
                    "{} / hits {}",
                    &[
                        &summary.packet_count.to_string(),
                        &summary.packets_with_hits.to_string(),
                    ],
                ));
                ui.label(t("Hits"));
                ui.monospace(summary.hit_count.to_string());
                ui.end_row();

                ui.label(t("Outgoing"));
                ui.monospace(tf(
                    "{} / {}",
                    &[
                        &summary.outgoing_hits.to_string(),
                        &format_number(summary.outgoing_damage),
                    ],
                ));
                ui.label(t("Candidate"));
                ui.monospace(tf(
                    "{} / {}",
                    &[
                        &summary.unknown_direction_hits.to_string(),
                        &format_number(summary.unknown_direction_damage),
                    ],
                ));
                ui.end_row();

                ui.label(t("Incoming"));
                ui.monospace(tf(
                    "{} / {}",
                    &[
                        &summary.incoming_hits.to_string(),
                        &format_number(summary.incoming_damage),
                    ],
                ));
                ui.label(t("Unknown Characters"));
                ui.monospace(tf(
                    "{} / {} hits",
                    &[
                        &summary.unknown_character_count.to_string(),
                        &summary.unknown_character_hits.to_string(),
                    ],
                ));
                ui.end_row();

                ui.label(t("Pending Skills"));
                ui.monospace(tf(
                    "{} kinds / {} hits",
                    &[
                        &summary.unmapped_skill_rows.to_string(),
                        &summary.unmapped_skill_hits.to_string(),
                    ],
                ));
                ui.label(t("Unmapped GE"));
                ui.monospace(summary.unmapped_gameplay_effect_count.to_string());
                ui.end_row();

                ui.label(t("Time Stop"));
                ui.monospace(tf(
                    "{} events / {} intervals",
                    &[
                        &summary.time_stop_event_count.to_string(),
                        &summary.time_stop_interval_count.to_string(),
                    ],
                ));
                ui.label(t("Abyss"));
                ui.monospace(tf("{} events", &[&summary.abyss_event_count.to_string()]));
                ui.end_row();

                ui.label(t("Damage Calibration"));
                ui.monospace(tf(
                    "{} rows",
                    &[&summary.server_damage_corrections.to_string()],
                ));
                ui.label("");
                ui.label("");
                ui.end_row();
            });
    });
}
