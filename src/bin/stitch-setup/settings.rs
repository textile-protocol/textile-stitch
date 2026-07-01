// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Settings screen: change the operator wallet, buy/sell spreads, RPC URL and
//! price-feed URL for a configured folder. Reached from the panel's gear button
//! or ⌘,. Saving rewrites stitch.toml (comments preserved) and restarts a running
//! bot so the change takes effect.

use egui::{Align, CornerRadius, Layout, Margin, RichText};
use stitch_bot::setup::{SpreadEdit, SpreadKind, Status};

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
                        RichText::new("Change the operator wallet, spreads, and endpoints.")
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
                    wallet_card(ui, &p, app);
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

/// Operator wallet: current address, with the destructive key swap tucked behind
/// a deliberate "Change wallet…" click.
fn wallet_card(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        theme::field_label(ui, p, "Operator wallet");
        ui.add_space(2.0);
        let addr = app.operator.clone().unwrap_or_else(|| "—".into());
        ui.label(RichText::new(addr).monospace().color(p.text).size(12.5));
        ui.add_space(8.0);

        if !app.settings.change_wallet {
            if ui.button("Change wallet…").clicked() {
                app.settings.change_wallet = true;
            }
            return;
        }

        theme::field_label(ui, p, "New private key");
        ui.add(
            egui::TextEdit::singleline(&mut app.settings.key_input)
                .password(true)
                .hint_text("0x…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Replaces stitch.key with the new key. Cancel to keep your current wallet.",
            )
            .color(p.text_faint)
            .size(11.0),
        );
        ui.add_space(6.0);
        if ui.button("Cancel wallet change").clicked() {
            use zeroize::Zeroize;
            app.settings.key_input.zeroize();
            app.settings.change_wallet = false;
        }
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
    ui.add(
        egui::TextEdit::singleline(&mut edit.value)
            .hint_text("0")
            .desired_width(f32::INFINITY),
    );
}

/// RPC and price-feed endpoints.
fn endpoints_card(ui: &mut egui::Ui, p: &Palette, app: &mut StitchApp) {
    theme::card(p).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        theme::field_label(ui, p, "RPC URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.settings.rpc_url)
                .hint_text("https://…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(10.0);
        theme::field_label(ui, p, "Price feed URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.settings.feed_url)
                .hint_text("https://…")
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
