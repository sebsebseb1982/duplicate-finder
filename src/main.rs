// Pas de console supplémentaire sous Windows en version release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod deletion;
mod model;
mod preview;
mod rules;
mod scanner;
#[cfg(test)]
mod tests;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Duplicate Finder")
            .with_inner_size([1400.0, 850.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "duplicate-finder",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cc)))
        }),
    )
}
