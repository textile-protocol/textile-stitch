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

/// Public repo the release binaries (both `stitch` and `stitch-setup`) are cut
/// from. The two ship from one crate version, so a single check covers both.
const RELEASE_REPO: &str = "textile-protocol/textile-stitch";

/// The releases page to send operators to when a self-update isn't possible
/// (notably the macOS `Stitch.app`, which ships out-of-band with no updater
/// receipt, so it's re-downloaded rather than patched in place).
pub const RELEASES_PAGE: &str =
    "https://github.com/textile-protocol/textile-stitch/releases/latest";

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
}

/// Best-effort "is a newer release published?" check that does NOT need an
/// install receipt, so it works for the macOS app bundle too. Returns the newer
/// version string, or None when already current / offline / anything odd.
pub async fn newer_release() -> Option<String> {
    let url = format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent(concat!("stitch/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let release: GhRelease = client
        .get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    newer_than(env!("CARGO_PKG_VERSION"), &release.tag_name)
}

/// Blocking wrapper so the synchronous GUI can run the check on a worker thread
/// without threading a runtime through its own code.
pub fn newer_release_blocking() -> Option<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?
        .block_on(newer_release())
}

/// The pure comparison seam: return the normalized latest version when
/// `latest_tag` (e.g. "v0.2.0") parses to a semver strictly greater than
/// `current`. Any parse failure yields None, so a garbled tag can't nag.
fn newer_than(current: &str, latest_tag: &str) -> Option<String> {
    let latest = semver::Version::parse(latest_tag.strip_prefix('v').unwrap_or(latest_tag)).ok()?;
    let current = semver::Version::parse(current).ok()?;
    (latest > current).then(|| latest.to_string())
}

#[cfg(test)]
mod tests {
    use super::newer_than;

    #[test]
    fn flags_a_newer_tag() {
        assert_eq!(newer_than("0.1.0", "v0.2.0").as_deref(), Some("0.2.0"));
    }

    #[test]
    fn tolerates_a_missing_v_prefix() {
        assert_eq!(newer_than("0.1.0", "0.2.0").as_deref(), Some("0.2.0"));
    }

    #[test]
    fn ignores_the_same_version() {
        assert!(newer_than("0.1.0", "v0.1.0").is_none());
    }

    #[test]
    fn ignores_an_older_tag() {
        assert!(newer_than("0.2.0", "v0.1.0").is_none());
    }

    #[test]
    fn ignores_a_garbage_tag() {
        assert!(newer_than("0.1.0", "nightly").is_none());
    }
}
