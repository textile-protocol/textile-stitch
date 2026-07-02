// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! First-run setup: pick a corridor, choose a folder, paste the key, write config.

use egui::{CornerRadius, Margin, RichText, Stroke};
use stitch_bot::setup;

use crate::app::StitchApp;
use crate::theme::{self, Palette};

pub fn show(app: &mut StitchApp, ui: &mut egui::Ui) {
    let p = Palette::current(ui.ctx());
    let frame = egui::Frame::new().fill(p.bg).inner_margin(Margin {
        left: 22,
        right: 22,
        top: 20,
        bottom: 20,
    });

    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        ui.horizontal(|ui| {
            theme::header_tile(ui, &p, &app.icon);
            ui.add_space(3.0);
            ui.vertical(|ui| {
                ui.heading("Set up Stitch");
                ui.label(
                    RichText::new("Pick a corridor and add your operator wallet to get started.")
                        .color(p.text_muted)
                        .size(12.5),
                );
            });
        });

        ui.add_space(18.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
        let corridors = setup::catalog();
        theme::card(&p).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;

            theme::field_label(ui, &p, "Corridor");
            egui::ComboBox::from_id_salt("corridor")
                .width(ui.available_width())
                .selected_text(format!(
                    "{} — {}",
                    corridors[app.selected_corridor].display_name,
                    corridors[app.selected_corridor].network_label
                ))
                .show_ui(ui, |ui| {
                    for (i, c) in corridors.iter().enumerate() {
                        ui.selectable_value(
                            &mut app.selected_corridor,
                            i,
                            format!("{} — {}", c.display_name, c.network_label),
                        );
                    }
                });

            ui.add_space(12.0);
            theme::field_label(ui, &p, "Folder");
            folder_field(ui, &p, app);

            ui.add_space(12.0);
            crate::signerform::signer_fields(ui, &p, &mut app.signer_form);
        });

        ui.add_space(16.0);
        if theme::primary_button(ui, &p, "Create configuration").clicked() {
            // Don't clobber an existing setup (including its private key) just
            // because the operator browsed to the wrong folder: ask first.
            if setup::has_operator_files(&app.dir) && !app.pending_overwrite {
                app.pending_overwrite = true;
            } else {
                write_and_advance(app);
            }
        }

        if app.pending_overwrite {
            ui.add_space(12.0);
            notice_card(ui, p.warning, p.warning_bg, &format!(
                "{} already has a Stitch config. Overwriting replaces stitch.toml and the stored operator secret.",
                app.dir.display()
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::tinted_button(ui, p.warning, "Overwrite").clicked() {
                    write_and_advance(app);
                }
                if ui.button("Cancel").clicked() {
                    app.pending_overwrite = false;
                }
            });
        }

        if let Some(err) = &app.setup_error {
            ui.add_space(12.0);
            notice_card(ui, p.danger, p.danger_bg, err);
        }
            });
    });
}

/// A read-only path field with a Browse button, laid out to fill the row.
fn folder_field(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    let browse_w = 96.0;
    let gap = 8.0;
    let field_w = (ui.available_width() - browse_w - gap).max(140.0);
    ui.horizontal(|ui| {
        egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(11, 7))
            .show(ui, |ui| {
                ui.set_width(field_w - 22.0);
                ui.label(
                    RichText::new(app.dir.display().to_string())
                        .monospace()
                        .color(p.text_muted)
                        .size(12.5),
                );
            });
        if ui.button("Browse…").clicked() {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                app.dir = folder;
                app.paths = setup::config_paths(&app.dir);
                // A pending overwrite confirmation was for the old folder; drop
                // it so it can't apply to the new one.
                app.pending_overwrite = false;
            }
        }
    });
}

/// A tinted notice card (warning or error) with colored text.
fn notice_card(ui: &mut egui::Ui, fg: egui::Color32, bg: egui::Color32, msg: &str) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(msg).color(fg).size(12.5));
        });
}

/// Write the chosen corridor's config for the selected signer, wipe the pasted
/// secrets, and move to the control panel. On failure the error is shown and the
/// form stays put.
fn write_and_advance(app: &mut StitchApp) {
    let corridor = &setup::catalog()[app.selected_corridor];
    match setup::write_config_signer(&app.dir, corridor, &app.signer_form.to_setup()) {
        Ok(_) => {
            app.signer_form.zeroize_secrets();
            app.setup_error = None;
            app.pending_overwrite = false;
            app.refresh_after_setup();
        }
        Err(e) => {
            app.setup_error = Some(format!("{e:#}"));
            app.pending_overwrite = false;
        }
    }
}
