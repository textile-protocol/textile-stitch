// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! "Stitch" — the desktop setup and control app. Windowed (no console on
//! Windows). Sets the bot up on first run, then supervises it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod panel;
mod wizard;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([560.0, 420.0])
            .with_title("Stitch"),
        ..Default::default()
    };
    eframe::run_native(
        "Stitch",
        options,
        Box::new(|_cc| Ok(Box::new(app::StitchApp::new()))),
    )
}
