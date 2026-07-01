// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Control panel for a configured folder: run/stop, dry-run, Permit2 approval,
//! update, and a live log pane.

use egui::{Align, Layout, Margin, RichText};
use stitch_bot::setup::{self, Status};

use crate::app::StitchApp;
use crate::theme::{self, Palette};

pub fn show(app: &mut StitchApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let p = Palette::current(&ctx);
    let running = app.status != Status::Stopped;

    app.check_for_update(&ctx);

    egui::Panel::top("header")
        .frame(panel_frame(&p, 14, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::header_tile(ui, &p, &app.icon);
                ui.add_space(3.0);
                ui.vertical(|ui| {
                    ui.heading("Stitch");
                    let sub = app
                        .corridor
                        .map(|c| format!("{} · {}", c.display_name, c.network_label))
                        .unwrap_or_else(|| "Custom corridor".into());
                    ui.label(RichText::new(sub).color(p.text_muted).size(12.5));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (bg, fg, label) = status_style(&p, app.status);
                    theme::status_pill(ui, bg, fg, label);
                });
            });

            ui.add_space(13.0);
            let operator = app.operator.clone().unwrap_or_else(|| "—".into());
            let folder = app.paths.dir.display().to_string();
            ui.columns(2, |cols| {
                meta_card(&mut cols[0], &p, "Operator", &operator);
                meta_card(&mut cols[1], &p, "Folder", &folder);
            });
        });

    if let Some(latest) = app.available_update.lock().unwrap().clone() {
        egui::Panel::top("update-nudge")
            .frame(panel_frame(&p, 0, 12))
            .show(ui, |ui| {
                update_banner(ui, &p, &latest);
            });
    }

    egui::Panel::top("controls")
        .frame(panel_frame(&p, 12, 12))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                // One hero button that flips between the two live actions.
                if running {
                    if theme::tinted_button(ui, p.danger, "■  Stop").clicked() {
                        app.stop_bot();
                    }
                } else if theme::primary_button(ui, &p, "▶  Start").clicked() {
                    app.start_bot(&ctx);
                }

                ui.add_space(2.0);
                ui.add_enabled_ui(!running, |ui| {
                    theme::toggle(ui, &p, &mut app.dry_run);
                    ui.label(RichText::new("Dry run").color(p.text_muted));
                });

                ui.separator();

                if ui
                    .add_enabled(!running, egui::Button::new("Approve tokens"))
                    .clicked()
                {
                    if let Some(bin) = app.stitch_bin.clone() {
                        let cmd = setup::approve_command(&bin, &app.paths, false);
                        app.run_oneshot(cmd, &ctx);
                    }
                }
                if ui.button("Preview").clicked() {
                    if let Some(bin) = app.stitch_bin.clone() {
                        let cmd = setup::approve_command(&bin, &app.paths, true);
                        app.run_oneshot(cmd, &ctx);
                    }
                }
                if ui
                    .add_enabled(!running, egui::Button::new("Update"))
                    .clicked()
                {
                    if let Some(bin) = app.stitch_bin.clone() {
                        let cmd = setup::update_command(&bin);
                        app.run_oneshot(cmd, &ctx);
                    }
                }
                if ui.button("Open folder").clicked() {
                    open_folder(&app.paths.dir);
                }
            });

            if app.stitch_bin.is_none() {
                ui.add_space(6.0);
                ui.colored_label(
                    p.danger,
                    "stitch binary not found next to this app — Start, Approve and Update are disabled.",
                );
            }
            if let Some(note) = &app.action_note {
                ui.add_space(6.0);
                ui.label(RichText::new(note).color(p.text_muted));
            }
            if cfg!(windows) && running {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "On Windows, Stop ends the bot immediately rather than after the current tick.",
                    )
                    .color(p.text_faint)
                    .size(11.0),
                );
            }
        });

    egui::CentralPanel::default()
        .frame(panel_frame(&p, 12, 16))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::field_label(ui, &p, "Logs");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("auto-scroll").color(p.text_faint).size(11.0));
                });
            });
            ui.add_space(6.0);
            theme::card(&p).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let logs = app.logs.lock().unwrap();
                        if logs.is_empty() {
                            ui.label(
                                RichText::new("Waiting for output…")
                                    .color(p.text_faint)
                                    .monospace(),
                            );
                        }
                        for line in logs.iter() {
                            log_line(ui, &p, line);
                        }
                    });
            });
        });
}

/// A flat panel frame in the app background with generous horizontal padding.
fn panel_frame(p: &Palette, top: i8, bottom: i8) -> egui::Frame {
    egui::Frame::new().fill(p.bg).inner_margin(Margin {
        left: 18,
        right: 18,
        top,
        bottom,
    })
}

/// A labeled value card that fills its column.
fn meta_card(ui: &mut egui::Ui, p: &Palette, label: &str, value: &str) {
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.vertical(|ui| {
            theme::field_label(ui, p, label);
            ui.add_space(2.0);
            ui.label(RichText::new(value).monospace().color(p.text).size(12.5));
        });
    });
}

/// Color a log line by level so warnings and errors stand out at a glance.
fn log_line(ui: &mut egui::Ui, p: &Palette, line: &str) {
    let color = if line.contains("ERROR") {
        p.danger
    } else if line.contains("WARN") {
        p.warning
    } else {
        p.text
    };
    ui.label(RichText::new(line).monospace().color(color));
}

fn status_style(p: &Palette, status: Status) -> (egui::Color32, egui::Color32, &'static str) {
    match status {
        Status::Stopped => (p.surface_hover, p.text_muted, "Stopped"),
        Status::Running => (p.success_bg, p.success, "Running"),
        Status::DryRun => (p.warning_bg, p.warning, "Dry run"),
    }
}

/// A warning-tinted strip that tells the operator a newer release is out and
/// sends them to the download page. The macOS app ships out-of-band, so a
/// download (not an in-place self-update) is the honest action for everyone.
fn update_banner(ui: &mut egui::Ui, p: &Palette, latest: &str) {
    egui::Frame::new()
        .fill(p.warning_bg)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(Margin::symmetric(13, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Stitch v{latest} is available."))
                        .color(p.warning)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::tinted_button(ui, p.accent, "Download").clicked() {
                        open_url(stitch_bot::update::RELEASES_PAGE);
                    }
                });
            });
        });
}

fn open_folder(dir: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
