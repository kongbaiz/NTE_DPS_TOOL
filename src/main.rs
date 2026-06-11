#![cfg_attr(windows, windows_subsystem = "windows")]
#![cfg_attr(feature = "no_debug", allow(dead_code))]

mod app;
mod capture;
mod hotkey;
mod model;
mod network;
mod parser;

use anyhow::Result;
use app::DpsApp;
use eframe::egui;

fn main() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NTE 实时 DPS")
            .with_inner_size([520.0, 390.0])
            .with_min_inner_size([460.0, 320.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_window_level(egui::WindowLevel::AlwaysOnTop),
        ..Default::default()
    };

    eframe::run_native(
        "NTE 实时 DPS",
        options,
        Box::new(|cc| Ok(Box::new(DpsApp::new(cc)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
