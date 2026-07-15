// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Operator onboarding: the corridor catalog, config-file writer, path helpers,
//! and child-process command builders shared by `stitch init` and the GUI.

pub mod catalog;
pub mod macos;
pub mod paths;
pub mod process;
pub mod settings;
pub mod writer;

pub use catalog::{catalog, find_corridor, identify_corridor, Corridor};
pub use paths::{
    config_paths, default_dir, has_operator_files, home_dir, is_configured, legacy_gui_dirs,
    operator_address, remember_config_dir, remembered_config_dir, ConfigPaths,
};
pub use process::{
    approve_command, find_stitch_binary, run_command, terminate, update_command, Status,
};
pub use settings::{
    apply_settings, read_settings, read_signer, SettingsPatch, SettingsView, SignerView,
    SpreadEdit, SpreadKind,
};
pub use writer::{
    apply_signer, render_env, switch_corridor_preserving_signer, write_config, write_config_signer,
    write_key, write_toml_atomic, LocalKeyMaterial, SignerKind, SignerSetup,
};
