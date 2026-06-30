// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Operator onboarding: the corridor catalog, config-file writer, path helpers,
//! and child-process command builders shared by `stitch init` and the GUI.

pub mod catalog;
pub mod paths;
pub mod process;
pub mod writer;

pub use catalog::{catalog, find_corridor, identify_corridor, Corridor};
pub use paths::{
    config_paths, default_dir, has_operator_files, is_configured, operator_address, ConfigPaths,
};
pub use process::{
    approve_command, find_stitch_binary, run_command, terminate, update_command, Status,
};
pub use writer::{render_env, write_config};
