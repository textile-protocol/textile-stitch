// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Settings screen: switch to another corridor preset, or change the operator
//! wallet, buy/sell spreads, RPC URL and price-feed URL for a configured folder.
//! Reached from the panel's gear button or ⌘,. Saving rewrites stitch.toml
//! (comments preserved) and restarts a running bot so the change takes effect;
//! switching corridor replaces stitch.toml wholesale and stops a running bot.

use egui::{Align, CornerRadius, Layout, Margin, RichText};
use stitch_bot::setup::{self, SpreadEdit, SpreadKind, Status};

use crate::app::StitchApp;
use crate::theme::{self, Palette};

pub fn show(app: &mut StitchApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let p = Palette::current(&ctx);
    let running = app.status != Status::Stopped;

    egui::Panel::top("settings-header")
        .frame(panel_frame(&p, 14, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::header_tile(ui, &p, &app.icon);
                ui.add_space(3.0);
                ui.vertical(|ui| {
                    ui.heading("Settings");
                    ui.label(
                        RichText::new(
                            "Switch corridors, or change the signer, spreads, and endpoints.",
                        )
                        .color(p.text_muted)
                        .size(12.5),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("← Back").clicked() {
                        app.close_settings();
                    }
                });
            });
        });

    egui::CentralPanel::default()
        .frame(panel_frame(&p, 8, 16))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    corridor_card(ui, &p, app);
                    ui.add_space(12.0);
                    signer_card(ui, &p, app);
                    ui.add_space(12.0);
                    spreads_card(ui, &p, app);
                    ui.add_space(12.0);
                    endpoints_card(ui, &p, app);

                    ui.add_space(16.0);
                    let label = if running {
                        "Save & restart bot"
                    } else {
                        "Save"
                    };
                    if theme::primary_button(ui, &p, label).clicked() {
                        app.save_settings(&ctx);
                    }
                    if running {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("The bot is running. Saving will stop and restart it.")
                                .color(p.text_faint)
                                .size(11.0),
                        );
                    }

                    if let Some(err) = &app.settings.error {
                        ui.add_space(12.0);
                        notice_card(ui, p.danger, p.danger_bg, err);
                    }
                });
        });
}

/// Current corridor, with the whole-config swap to another preset tucked behind a
/// deliberate "Switch corridor…" click. Switching replaces stitch.toml with the
/// chosen preset and discards this corridor's spread/endpoint tweaks, so it's kept
/// out of the way of the per-field edits below.
fn corridor_card(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    let corridors = setup::catalog();
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        theme::field_label(ui, p, "Corridor");
        ui.add_space(2.0);
        let current = app
            .corridor
            .map(|c| format!("{} — {}", c.display_name, c.network_label))
            .unwrap_or_else(|| "Custom corridor".into());
        ui.label(RichText::new(current).color(p.text).size(12.5));

        // With a single corridor in the catalog there's nothing to switch to.
        if corridors.len() < 2 {
            return;
        }
        ui.add_space(8.0);

        if !app.settings.switch_corridor {
            if ui.button("Switch corridor…").clicked() {
                app.settings.switch_corridor = true;
                app.settings.corridor_choice = app.current_corridor_index();
            }
            return;
        }

        theme::field_label(ui, p, "Switch to");
        egui::ComboBox::from_id_salt("settings-corridor")
            .width(ui.available_width())
            .selected_text(format!(
                "{} — {}",
                corridors[app.settings.corridor_choice].display_name,
                corridors[app.settings.corridor_choice].network_label
            ))
            .show_ui(ui, |ui| {
                for (i, c) in corridors.iter().enumerate() {
                    ui.selectable_value(
                        &mut app.settings.corridor_choice,
                        i,
                        format!("{} — {}", c.display_name, c.network_label),
                    );
                }
            });

        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Switching replaces this folder's config with the chosen preset. The current \
                 spreads and endpoints are discarded; your wallet is kept. A running bot is \
                 stopped, and you'll need to approve tokens for the new corridor before starting.",
            )
            .color(p.text_faint)
            .size(11.0),
        );
        ui.add_space(8.0);

        let target = corridors[app.settings.corridor_choice];
        let is_change = app.corridor.is_none_or(|c| c.id != target.id);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(is_change, |ui| {
                if theme::tinted_button(
                    ui,
                    p.warning,
                    &format!("Switch to {}", target.display_name),
                )
                .clicked()
                {
                    app.switch_corridor();
                }
            });
            if ui.button("Cancel").clicked() {
                app.settings.switch_corridor = false;
            }
        });
        if !is_change {
            ui.add_space(4.0);
            ui.label(
                RichText::new("This is the current corridor. Pick another to switch.")
                    .color(p.text_faint)
                    .size(11.0),
            );
        }
    });
}

/// Signer: the current backend (hot wallet vs MPC) and operator address, with the
/// change tucked behind a deliberate "Change signer…" click. Applying rewrites the
/// `[signer]` section, the secret file, and stitch.env — its own action, separate
/// from the Save button — and bounces a running bot.
fn signer_card(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    let ctx = ui.ctx().clone();
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        theme::field_label(ui, p, "Signer");
        ui.add_space(2.0);
        ui.label(
            RichText::new(app.settings.signer.kind.display_label())
                .color(p.text)
                .size(12.5),
        );
        ui.add_space(6.0);
        theme::field_label(ui, p, "Operator");
        let addr = app.operator.clone().unwrap_or_else(|| "—".into());
        ui.label(
            RichText::new(addr)
                .monospace()
                .color(p.text_muted)
                .size(12.0),
        );
        ui.add_space(8.0);

        if !app.settings.change_signer {
            if ui.button("Change signer…").clicked() {
                app.settings.change_signer = true;
            }
            return;
        }

        crate::signerform::signer_fields(ui, p, &mut app.settings.signer);
        ui.add_space(2.0);
        ui.label(
            RichText::new(
                "Applying rewrites the signer and its secret, then restarts a running bot.",
            )
            .color(p.text_faint)
            .size(11.0),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::primary_button(ui, p, "Apply signer").clicked() {
                app.apply_signer_change(&ctx);
            }
            if ui.button("Cancel").clicked() {
                app.settings.signer.zeroize_secrets();
                app.settings.change_signer = false;
                if let Ok(toml) = std::fs::read_to_string(&app.paths.toml) {
                    app.settings.signer =
                        crate::signerform::SignerForm::from_view(&setup::read_signer(&toml));
                }
            }
        });
    });
}

/// Buy/sell spreads, side by side, each labelled with the unit its config uses.
fn spreads_card(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        theme::field_label(ui, p, "Spreads");
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            spread_field(&mut cols[0], p, "Buy spread", &mut app.settings.buy);
            spread_field(&mut cols[1], p, "Sell spread", &mut app.settings.sell);
        });
        if app.settings.pool_count > 1 {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "This config has {} pools. Only the first is edited here.",
                    app.settings.pool_count
                ))
                .color(p.text_faint)
                .size(11.0),
            );
        }
    });
}

/// A single spread input, its label carrying the unit (bps or abs) the config uses.
fn spread_field(ui: &mut egui::Ui, p: &Palette, label: &str, edit: &mut SpreadEdit) {
    let unit = match edit.kind {
        SpreadKind::Bps => "bps",
        SpreadKind::Abs => "abs",
    };
    theme::field_label(ui, p, &format!("{label} ({unit})"));
    let resp = ui.add(
        egui::TextEdit::singleline(&mut edit.value)
            .hint_text("0")
            .margin(theme::FIELD_MARGIN)
            .desired_width(f32::INFINITY),
    );
    // Reject non-numeric input as it's typed/pasted, so a spread can only ever be
    // a number (bps is parsed as an integer, an absolute offset as a decimal).
    if resp.changed() {
        keep_numeric(&mut edit.value, edit.kind);
    }
}

/// Strip anything that isn't part of a number: digits only for bps (an integer),
/// digits plus a single decimal point for an absolute offset (a decimal).
fn keep_numeric(value: &mut String, kind: SpreadKind) {
    let allow_dot = matches!(kind, SpreadKind::Abs);
    let mut seen_dot = false;
    value.retain(|c| {
        if c.is_ascii_digit() {
            true
        } else if allow_dot && c == '.' && !seen_dot {
            seen_dot = true;
            true
        } else {
            false
        }
    });
}

/// RPC and price-feed endpoints.
fn endpoints_card(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        theme::field_label(ui, p, "RPC URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.settings.rpc_url)
                .hint_text("https://…")
                .margin(theme::FIELD_MARGIN)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(10.0);
        theme::field_label(ui, p, "Price feed URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.settings.feed_url)
                .hint_text("https://…")
                .margin(theme::FIELD_MARGIN)
                .desired_width(f32::INFINITY),
        );
    });
}

/// A flat panel frame in the app background with generous horizontal padding.
/// Matches the control panel so the two screens read as one product.
fn panel_frame(p: &Palette, top: i8, bottom: i8) -> egui::Frame {
    egui::Frame::new().fill(p.bg).inner_margin(Margin {
        left: 18,
        right: 18,
        top,
        bottom,
    })
}

/// A tinted notice card (error) with colored text.
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

#[cfg(test)]
mod tests {
    use super::keep_numeric;
    use stitch_bot::setup::SpreadKind;

    #[test]
    fn bps_keeps_digits_only() {
        let mut v = "1a2.3 4x".to_string();
        keep_numeric(&mut v, SpreadKind::Bps);
        assert_eq!(v, "1234");
    }

    #[test]
    fn abs_keeps_digits_and_one_decimal_point() {
        let mut v = "0.00a1.5".to_string();
        keep_numeric(&mut v, SpreadKind::Abs);
        assert_eq!(v, "0.0015", "letters gone, only the first dot kept");
    }

    #[test]
    fn empty_and_pure_number_are_unchanged() {
        let mut v = String::new();
        keep_numeric(&mut v, SpreadKind::Bps);
        assert_eq!(v, "");
        let mut n = "42".to_string();
        keep_numeric(&mut n, SpreadKind::Bps);
        assert_eq!(n, "42");
    }
}
