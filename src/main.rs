#![cfg_attr(windows, windows_subsystem = "windows")]
#![cfg_attr(feature = "no_debug", allow(dead_code))]

mod abyss_data;
mod app;
mod capture;
mod character_editor;
mod config;
mod encrypted_ini;
mod file_drop;
mod hotkey;
mod io_util;
mod model;
mod network;
mod parser;
mod protocol;
mod window_attributes;

use anyhow::Result;
use app::DpsApp;
use eframe::egui;
use std::sync::Arc;

fn main() -> Result<()> {
    install_panic_log();
    let (ui_config, config_warning) = config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NTE DPS TOOL")
            // 100% size; the title-bar −／＋ stepper scales it. Free drag-resize is
            // disabled (with_resizable(false)) to avoid the Windows resize crash.
            .with_inner_size(app::MAIN_WINDOW_BASE_SIZE)
            .with_decorations(false)
            .with_resizable(false)
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
    let image = image::load_from_memory(include_bytes!("../res/icons/app-icon.png"))
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
