// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! "Stitch" — the desktop setup and control app. Windowed (no console on
//! Windows). Sets the bot up on first run, then supervises it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod icon;
mod panel;
mod wizard;

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([720.0, 560.0])
        .with_min_inner_size([560.0, 420.0])
        .with_title("Stitch")
        // App id must match the stitch.desktop basename so Linux (Wayland app_id
        // / X11 WM class) links the window to the launcher for its name + icon.
        .with_app_id("stitch");
    if let Some(window_icon) = icon::window_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(window_icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Stitch",
        options,
        Box::new(|cc| {
            let icon = icon::texture(&cc.egui_ctx);
            Ok(Box::new(app::StitchApp::new(icon)))
        }),
    )
}
