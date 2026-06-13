#![cfg_attr(windows, windows_subsystem = "windows")]
#![cfg_attr(feature = "no_debug", allow(dead_code))]

mod app;
mod capture;
mod hotkey;
mod model;
mod network;
mod parser;
mod protocol;

use anyhow::Result;
use app::DpsApp;
use eframe::egui;
use std::sync::Arc;

fn main() -> Result<()> {
    install_panic_log();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NTE DPS TOOL")
            .with_inner_size([520.0, 420.0])
            .with_min_inner_size([460.0, 390.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_icon(Arc::new(app_icon()))
            .with_window_level(egui::WindowLevel::AlwaysOnTop),
        ..Default::default()
    };

    eframe::run_native(
        "NTE DPS TOOL",
        options,
        Box::new(|cc| Ok(Box::new(DpsApp::new(cc)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn app_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../assets/app-icon.png"))
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
