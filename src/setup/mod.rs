// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Operator onboarding: the corridor catalog, config-file writer, path helpers,
//! and child-process helpers shared by `stitch init`, the panel, and the
//! desktop tray app.

pub mod catalog;
pub mod custom;
pub mod explorer;
pub mod macos;
pub mod paths;
pub mod process;
pub mod settings;
pub mod writer;

pub use catalog::{catalog, deployable_catalog, find_corridor, identify_corridor, Corridor};
pub use custom::CustomCorridor;
pub use explorer::{address_explorer_url, explorer_base_url};
pub use paths::{
    app_state_dir, config_paths, default_dir, has_operator_files, home_dir, is_configured,
    legacy_gui_dirs, operator_address, operator_address_from_key, remember_config_dir,
    remembered_config_dir, ConfigPaths,
};
pub use process::{find_stitch_binary, terminate};
pub use settings::{
    apply_rfq_default_preset, apply_settings, read_settings, read_settings_at, read_signer,
    rfq_connect_patch, try_read_signer, PoolPair, SettingsPatch, SettingsView, SideSizing,
    SignerView, SpreadEdit, SpreadKind,
};
pub use writer::{
    apply_signer, render_env, rfq_api_key_is_set, signer_files, stamp_rfq_default_preset,
    switch_corridor_file, switch_corridor_preserving_signer, validate_signer_change, write_config,
    write_config_signer, write_config_signer_from_toml, write_key, write_rfq_api_key,
    write_toml_atomic, LocalKeyMaterial, SignerKind, SignerSetup, RFQ_API_KEY_FILE,
    RFQ_API_KEY_FILE_ENV,
};
