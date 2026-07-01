#![cfg_attr(windows, windows_subsystem = "windows")]
#![cfg_attr(feature = "no_debug", allow(dead_code))]

mod app;
mod engine;
mod platform;
mod storage;
mod support;

use anyhow::Result;
use app::DpsApp;
use eframe::egui;
use std::path::Path;
use std::sync::Arc;

fn main() -> Result<()> {
    install_panic_log();
    let (ui_config, config_warning) = storage::config::load();
    // Load the active locale before the first frame so the UI never flashes English keys.
    storage::i18n::set_language(ui_config.language);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NTE DPS TOOL")
            // Reopen at the last dragged size (native edge-resize via BeginResize grips), falling
            // back to the base size on first run. The window stays borderless; resize is driven by
            // the custom grips in `window_resize_grips`.
            .with_inner_size(
                ui_config
                    .main_window_size
                    .map(egui::Vec2::from)
                    .unwrap_or(app::MAIN_WINDOW_BASE_SIZE),
            )
            .with_min_inner_size(egui::Vec2::from(storage::config::MAIN_WINDOW_MIN_SIZE))
            .with_decorations(false)
            .with_resizable(true)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_icon(Arc::new(app_icon()))
            .with_window_level(if ui_config.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }),
        ..Default::default()
    };

    eframe::run_native(
        "NTE DPS TOOL",
        options,
        Box::new(move |cc| Ok(Box::new(DpsApp::new(cc, ui_config, config_warning)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn app_icon() -> egui::IconData {
    let bytes = storage::resource::read_resource_bytes(Path::new("res/icons/app-icon.png"))
        .expect("application icon resource must be available");
    let image = image::load_from_memory(bytes.as_ref())
        .expect("embedded application icon must be valid")
        .into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        let _ = std::fs::create_dir_all("logs");
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let path = format!("logs/nte_panic_{timestamp}.log");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::write(path, format!("{info}\n\n{backtrace}\n"));
    }));
}
