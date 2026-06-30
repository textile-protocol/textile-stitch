// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! First-run setup: pick a corridor, choose a folder, paste the key, write config.

use stitch_bot::setup;

use crate::app::StitchApp;

pub fn show(app: &mut StitchApp, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            crate::app::show_header_icon(ui, &app.icon);
            ui.heading("Set up Stitch");
        });
        ui.label("Pick a corridor and add your operator wallet to get started.");
        ui.add_space(16.0);

        let corridors = setup::catalog();
        egui::Grid::new("setup_form")
            .num_columns(2)
            .spacing([16.0, 12.0])
            .show(ui, |ui| {
                ui.label("Corridor");
                egui::ComboBox::from_id_source("corridor")
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
                ui.end_row();

                ui.label("Folder");
                ui.horizontal(|ui| {
                    ui.monospace(app.dir.display().to_string());
                    if ui.button("Browse…").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            app.dir = folder;
                            app.paths = setup::config_paths(&app.dir);
                            // A pending overwrite confirmation was for the old
                            // folder; drop it so it can't apply to the new one.
                            app.pending_overwrite = false;
                        }
                    }
                });
                ui.end_row();

                ui.label("Private key");
                ui.add(
                    egui::TextEdit::singleline(&mut app.key_input)
                        .password(true)
                        .hint_text("0x…")
                        .desired_width(360.0),
                );
                ui.end_row();
            });

        ui.add_space(16.0);
        if ui.button("Create configuration").clicked() {
            // Don't clobber an existing setup (including its private key) just
            // because the operator browsed to the wrong folder: ask first.
            if setup::has_operator_files(&app.dir) && !app.pending_overwrite {
                app.pending_overwrite = true;
            } else {
                write_and_advance(app);
            }
        }

        if app.pending_overwrite {
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::from_rgb(200, 150, 40),
                format!(
                    "{} already has a Stitch config. Overwriting replaces stitch.toml \
                     and the stitch.key private key.",
                    app.dir.display()
                ),
            );
            ui.horizontal(|ui| {
                if ui.button("Overwrite").clicked() {
                    write_and_advance(app);
                }
                if ui.button("Cancel").clicked() {
                    app.pending_overwrite = false;
                }
            });
        }

        if let Some(err) = &app.setup_error {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
        }
    });
}

/// Write the chosen corridor's config, wipe the pasted key, and move to the
/// control panel. On failure the error is shown and the form stays put.
fn write_and_advance(app: &mut StitchApp) {
    let corridor = &setup::catalog()[app.selected_corridor];
    match setup::write_config(&app.dir, corridor, &app.key_input) {
        Ok(_) => {
            use zeroize::Zeroize;
            app.key_input.zeroize();
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
