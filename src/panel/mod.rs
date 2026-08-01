// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The Stitch web UI backend: add, remove, start, stop and configure
//! Stitch bots on one Docker host from a browser.
//!
//! The panel is a control plane, not a second implementation of the bot. Every
//! config it writes goes through [`crate::setup`] — the same corridor catalog,
//! the same atomic writer, the same `toml_edit` settings patcher, the same
//! [`crate::config::Config`] validation the bot itself uses. That keeps one
//! definition of "a valid bot config" in the crate instead of one per surface.
//!
//! Container lifecycle goes through the Docker Engine API rather than shelling
//! out to `docker compose`, so the panel can adopt an existing hand-written
//! compose fleet without restarting it, and can't mangle the operator's compose
//! file. A generated (never round-tripped) compose export keeps that file a
//! valid recovery artifact.

pub mod auth;
pub mod compose;
pub mod config;
pub mod docker;
pub mod http;
pub mod inventory;
pub mod migrate;
pub mod naming;
pub mod provision;
pub mod updates;

pub use config::{AuthMode, PanelConfig};
pub use docker::{DockerApi, STOP_GRACE_SECS};
pub use inventory::{discover, Bot, Fleet, Layout, Origin, Warning};
pub use naming::{container_name, validate_bot_id};
