// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The "Move Stitch to Applications" nudge, shown across every view when the app
//! is running translocated or from outside an Applications folder. See
//! [`stitch_bot::setup::macos`] for why that state is worth fixing.

use egui::{Align, Layout, Margin, RichText};

use crate::app::StitchApp;
use crate::theme::{self, Palette};

/// Draw the install nudge as a top strip when needed. A no-op on non-macOS, once
/// the app is already in Applications, or after the operator dismisses it.
pub fn show_nudge(app: &mut StitchApp, ui: &mut egui::Ui) {
    let needs = app.mac_install.as_ref().is_some_and(|m| m.needs_move());
    if !needs || app.install_nudge_dismissed {
        return;
    }

    let ctx = ui.ctx().clone();
    let p = Palette::current(&ctx);
    egui::Panel::top("install-nudge")
        .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin {
            left: 18,
            right: 18,
            top: 12,
            bottom: 0,
        }))
        .show(ui, |ui| {
            egui::Frame::new()
                .fill(p.warning_bg)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(Margin::symmetric(13, 10))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Move Stitch to your Applications folder")
                                .color(p.warning)
                                .strong(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            // Primary added first so it lands on the right.
                            if theme::tinted_button(ui, p.accent, "Move to Applications").clicked()
                            {
                                app.install_to_applications(&ctx);
                            }
                            if ui.button("Not now").clicked() {
                                app.install_nudge_dismissed = true;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "You're running it from the download. Moving it keeps updates and the \
                             bot binary working.",
                        )
                        .color(p.text_muted)
                        .size(12.0),
                    );
                    if let Some(err) = &app.install_error {
                        ui.add_space(4.0);
                        ui.label(RichText::new(err).color(p.danger).size(12.0));
                    }
                });
        });
}
