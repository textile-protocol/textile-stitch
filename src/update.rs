// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Self-update glue around `axoupdater`. It reads the install receipt that the
//! cargo-dist installer writes, so it only does anything for binaries installed
//! that way; running via `cargo run` (no receipt) is a silent no-op.

use anyhow::{anyhow, Context};
use axoupdater::AxoUpdater;
use tracing::{info, warn};

/// Cargo package name cargo-dist uses for the installer receipt directory
/// (`~/.config/stitch-bot/stitch-bot-receipt.json`). This is updater state, not
/// the operator-facing config directory (`~/Stitch`), and not the binary name
/// `stitch`.
const APP_NAME: &str = "stitch-bot";

/// Self-update to the latest release. Returns Ok even when already current.
/// Errors only on a real failure (network, bad receipt) so `--update` can
/// surface them to the operator.
pub async fn run_update() -> anyhow::Result<()> {
    let mut updater = AxoUpdater::new_for(APP_NAME);
    updater
        .load_receipt()
        .map_err(|e| anyhow!(e.to_string()))
        .context(
            "no install receipt found — `--update` only works for a release \
             installed via the stitch installer",
        )?;
    match updater.run().await.map_err(|e| anyhow!(e.to_string()))? {
        Some(result) => info!(version = %result.new_version, "updated stitch"),
        None => info!("already on the latest version"),
    }
    Ok(())
}

/// Best-effort "you're behind" nudge at startup. Never fails the bot: any error
/// (no receipt when run from source, network down) is swallowed silently so a
/// version check can't keep the operator from starting.
pub async fn warn_if_outdated() {
    let mut updater = AxoUpdater::new_for(APP_NAME);
    if updater.load_receipt().is_err() {
        return; // not installed via the updater; nothing to compare against
    }
    if let Ok(Some(latest)) = updater.query_new_version().await {
        warn!(
            current = env!("CARGO_PKG_VERSION"),
            latest = %latest,
            "a newer stitch is available — run `stitch --update`"
        );
    }
}
