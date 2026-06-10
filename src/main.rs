mod app;
mod capture;
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
            .with_inner_size([1220.0, 760.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "NTE 实时 DPS",
        options,
        Box::new(|cc| Ok(Box::new(DpsApp::new(cc)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
