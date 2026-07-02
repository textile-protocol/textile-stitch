// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The signer picker shared by the setup wizard and the Settings screen: a
//! dropdown to choose Hot wallet / Turnkey / MPCVault, plus the fields each
//! backend needs. Non-secret fields go into the config; secrets are collected
//! here and handed to the writer, which stores them in an owner-only file.

use egui::{CornerRadius, Margin, RichText, Stroke};
use stitch_bot::setup::{SignerKind, SignerSetup, SignerView};

use crate::theme::{self, Palette};

/// Editable signer state, mapped to [`SignerSetup`] on save. One struct holds
/// every provider's fields; only the selected provider's are read.
#[derive(Default)]
pub struct SignerForm {
    pub kind: SignerKind,
    /// Hot wallet private key.
    pub key: String,
    /// The operator/maker EVM address (shared by both MPC providers).
    pub operator_address: String,
    /// Optional API base URL override (blank = provider default).
    pub api_base_url: String,
    // Turnkey.
    pub organization_id: String,
    pub sign_with: String,
    pub api_public_key: String,
    pub api_private_key: String,
    // MPCVault.
    pub vault_uuid: String,
    pub client_signer_pubkey: String,
    pub callback_listen_addr: String,
    pub api_token: String,
}

impl SignerForm {
    /// Prefill the non-secret fields from an existing config (Settings screen).
    /// Secrets are never in the config, so their fields stay blank and are only
    /// rewritten if the operator types a new one.
    pub fn from_view(view: &SignerView) -> Self {
        match view {
            SignerView::Local => SignerForm {
                kind: SignerKind::Local,
                ..Default::default()
            },
            SignerView::Turnkey {
                organization_id,
                sign_with,
                operator_address,
                api_base_url,
            } => SignerForm {
                kind: SignerKind::Turnkey,
                operator_address: operator_address.clone(),
                api_base_url: api_base_url.clone(),
                organization_id: organization_id.clone(),
                sign_with: sign_with.clone(),
                ..Default::default()
            },
            SignerView::Mpcvault {
                vault_uuid,
                client_signer_pubkey,
                operator_address,
                api_base_url,
                callback_listen_addr,
            } => SignerForm {
                kind: SignerKind::Mpcvault,
                operator_address: operator_address.clone(),
                api_base_url: api_base_url.clone(),
                vault_uuid: vault_uuid.clone(),
                client_signer_pubkey: client_signer_pubkey.clone(),
                callback_listen_addr: callback_listen_addr.clone(),
                ..Default::default()
            },
        }
    }

    /// Build the writer input from the current fields (clones the strings).
    pub fn to_setup(&self) -> SignerSetup {
        let opt = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        match self.kind {
            SignerKind::Local => SignerSetup::Local {
                key: self.key.clone(),
            },
            SignerKind::Turnkey => SignerSetup::Turnkey {
                organization_id: self.organization_id.clone(),
                sign_with: self.sign_with.clone(),
                operator_address: self.operator_address.clone(),
                api_base_url: opt(&self.api_base_url),
                api_public_key: self.api_public_key.clone(),
                api_private_key: self.api_private_key.clone(),
            },
            SignerKind::Mpcvault => SignerSetup::Mpcvault {
                vault_uuid: self.vault_uuid.clone(),
                client_signer_pubkey: self.client_signer_pubkey.clone(),
                operator_address: self.operator_address.clone(),
                api_base_url: opt(&self.api_base_url),
                callback_listen_addr: opt(&self.callback_listen_addr),
                api_token: self.api_token.clone(),
            },
        }
    }

    /// Wipe every secret field. Called after a write and when a screen closes.
    pub fn zeroize_secrets(&mut self) {
        use zeroize::Zeroize;
        self.key.zeroize();
        self.api_private_key.zeroize();
        self.api_token.zeroize();
    }
}

/// Render the signer dropdown and the selected provider's fields.
pub fn signer_fields(ui: &mut egui::Ui, p: &Palette, form: &mut SignerForm) {
    theme::field_label(ui, p, "Signer");
    egui::ComboBox::from_id_salt("signer_kind")
        .width(ui.available_width())
        .selected_text(form.kind.display_label())
        .show_ui(ui, |ui| {
            for kind in SignerKind::ALL {
                ui.selectable_value(&mut form.kind, kind, kind.display_label());
            }
        });
    ui.add_space(10.0);

    // MPC backends are new; make the experimental status unmistakable once picked.
    if form.kind.experimental() {
        experimental_notice(ui, p);
    }

    match form.kind {
        SignerKind::Local => {
            secret_field(ui, p, "Private key", &mut form.key, "0x…");
        }
        SignerKind::Turnkey => {
            text_field(ui, p, "Organization ID", &mut form.organization_id, "");
            text_field(
                ui,
                p,
                "Sign with (wallet address or private-key id)",
                &mut form.sign_with,
                "0x… or key id",
            );
            text_field(ui, p, "Operator address", &mut form.operator_address, "0x…");
            text_field(ui, p, "API public key", &mut form.api_public_key, "");
            secret_field(ui, p, "API private key", &mut form.api_private_key, "");
        }
        SignerKind::Mpcvault => {
            sidecar_warning(ui, p);
            text_field(ui, p, "Vault UUID", &mut form.vault_uuid, "");
            text_field(
                ui,
                p,
                "Client-signer public key",
                &mut form.client_signer_pubkey,
                "ssh-ed25519 AAAA…",
            );
            text_field(ui, p, "Operator address", &mut form.operator_address, "0x…");
            secret_field(ui, p, "API token", &mut form.api_token, "");
        }
    }
}

fn text_field(ui: &mut egui::Ui, p: &Palette, label: &str, value: &mut String, hint: &str) {
    theme::field_label(ui, p, label);
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .margin(theme::FIELD_MARGIN)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(8.0);
}

fn secret_field(ui: &mut egui::Ui, p: &Palette, label: &str, value: &mut String, hint: &str) {
    theme::field_label(ui, p, label);
    ui.add(
        egui::TextEdit::singleline(value)
            .password(true)
            .hint_text(hint)
            .margin(theme::FIELD_MARGIN)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(8.0);
}

/// An "EXPERIMENTAL" pill plus a one-line caution for the new MPC backends, so the
/// operator knows they may be rough before committing funds to them.
fn experimental_notice(ui: &mut egui::Ui, p: &Palette) {
    ui.horizontal(|ui| {
        egui::Frame::new()
            .fill(p.warning_bg)
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(7, 3))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("EXPERIMENTAL")
                        .color(p.warning)
                        .strong()
                        .size(10.5),
                );
            });
        ui.label(
            RichText::new("New — test with a dry run before going live.")
                .color(p.text_muted)
                .size(11.0),
        );
    });
    ui.add_space(10.0);
}

/// A prominent warning callout for the MPCVault sidecar requirement: without the
/// client-signer running, the bot can't sign at all, so this must not read like an
/// optional aside.
fn sidecar_warning(ui: &mut egui::Ui, p: &Palette) {
    egui::Frame::new()
        .fill(p.warning_bg)
        .stroke(Stroke::new(1.0, p.warning))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(
                RichText::new("⚠  Requires the MPCVault client-signer sidecar")
                    .color(p.warning)
                    .strong()
                    .size(12.5),
            );
            ui.add_space(3.0);
            ui.label(
                RichText::new(
                    "The bot cannot sign without it. Run one client-signer container next to the \
                     bot (one per operator) before you start.",
                )
                .color(p.text)
                .size(11.5),
            );
            ui.add_space(5.0);
            ui.hyperlink_to(
                "MPCVault sidecar setup guide →",
                "https://github.com/textile-protocol/textile-stitch/blob/main/ADVANCED.md#mpcvault-sidecar",
            );
        });
    ui.add_space(10.0);
}
