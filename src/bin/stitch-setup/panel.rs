// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Control panel for a configured folder: run/stop, dry-run, Permit2 approval,
//! update, and a live log pane.

use stitch_bot::setup::{self, Status};

use crate::app::StitchApp;

pub fn show(app: &mut StitchApp, ctx: &egui::Context) {
    egui::TopBottomPanel::top("header").show(ctx, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Stitch");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                status_pill(ui, app.status);
            });
        });
        let pair = app
            .corridor
            .map(|c| format!("{} — {}", c.display_name, c.network_label))
            .unwrap_or_else(|| "Custom corridor".into());
        ui.label(pair);
        if let Some(addr) = &app.operator {
            ui.monospace(format!("Operator {addr}"));
        }
        ui.monospace(app.paths.dir.display().to_string());
        ui.add_space(6.0);
    });

    egui::TopBottomPanel::top("controls").show(ctx, |ui| {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let running = app.status != Status::Stopped;
            if ui
                .add_enabled(!running, egui::Button::new("▶ Start"))
                .clicked()
            {
                app.start_bot(ctx);
            }
            if ui
                .add_enabled(running, egui::Button::new("■ Stop"))
                .clicked()
            {
                app.stop_bot();
            }
            ui.add_enabled(!running, egui::Checkbox::new(&mut app.dry_run, "Dry run"));

            ui.separator();

            if ui
                .add_enabled(!running, egui::Button::new("Approve tokens"))
                .clicked()
            {
                if let Some(bin) = app.stitch_bin.clone() {
                    let cmd = setup::approve_command(&bin, &app.paths, false);
                    app.run_oneshot(cmd, ctx);
                }
            }
            if ui.button("Preview approval").clicked() {
                if let Some(bin) = app.stitch_bin.clone() {
                    let cmd = setup::approve_command(&bin, &app.paths, true);
                    app.run_oneshot(cmd, ctx);
                }
            }
            if ui
                .add_enabled(!running, egui::Button::new("Update bot"))
                .clicked()
            {
                if let Some(bin) = app.stitch_bin.clone() {
                    let cmd = setup::update_command(&bin);
                    app.run_oneshot(cmd, ctx);
                }
            }
            if ui.button("Open folder").clicked() {
                open_folder(&app.paths.dir);
            }
        });
        if app.stitch_bin.is_none() {
            ui.colored_label(
                egui::Color32::from_rgb(200, 60, 60),
                "stitch binary not found next to this app — Start/Approve/Update are disabled.",
            );
        }
        if let Some(note) = &app.action_note {
            ui.label(note);
        }
        if cfg!(windows) && app.status != Status::Stopped {
            ui.small(
                "On Windows, Stop ends the bot immediately rather than after the current tick.",
            );
        }
        ui.add_space(6.0);
    });

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(4.0);
        ui.label("Logs");
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let logs = app.logs.lock().unwrap();
                for line in logs.iter() {
                    ui.monospace(line);
                }
            });
    });
}

fn status_pill(ui: &mut egui::Ui, status: Status) {
    let (text, color) = match status {
        Status::Stopped => ("Stopped", egui::Color32::GRAY),
        Status::Running => ("Running", egui::Color32::from_rgb(40, 160, 80)),
        Status::DryRun => ("Dry run", egui::Color32::from_rgb(200, 150, 40)),
    };
    ui.colored_label(color, format!("● {text}"));
}

fn open_folder(dir: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}
