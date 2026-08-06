// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Self-update glue around `axoupdater`. It reads the install receipt that the
//! cargo-dist installer writes, so it only does anything for binaries installed
//! that way; running via `cargo run` (no receipt) is a silent no-op.
//!
//! Desktop also polls GitHub Releases (no receipt required) so `Stitch.app` /
//! archive installs can show an update banner when a newer tag is published.

use std::path::PathBuf;

use anyhow::{anyhow, Context};
use axoupdater::AxoUpdater;
use rand::RngCore;
use sha2::{Digest, Sha256};
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

/// Public repo the release binaries (`stitch`, `stitch-panel`, `stitch-desktop`) are cut
/// from. The two ship from one crate version, so a single check covers both.
const RELEASE_REPO: &str = "textile-protocol/textile-stitch";

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub digest: Option<String>,
}

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

/// Outcome of comparing this build to GitHub's latest release.
///
/// Callers must not treat a failed network check as "up to date" — that was
/// wiping the desktop update banner whenever the first poll lost a race with
/// DNS / wifi coming up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseCheck {
    /// `latest` is strictly newer than this binary's `CARGO_PKG_VERSION`.
    Available { latest: String, asset: ReleaseAsset },
    /// Latest published release is this version or older.
    Current,
    /// Could not reach GitHub or parse the response.
    Failed { reason: String },
}

/// Best-effort "is a newer release published?" check that does NOT need an
/// install receipt, so it works for the macOS app bundle too.
pub async fn check_latest_release() -> ReleaseCheck {
    check_latest_release_against(env!("CARGO_PKG_VERSION")).await
}

async fn check_latest_release_against(current: &str) -> ReleaseCheck {
    let url = format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest");
    let client = match reqwest::Client::builder()
        .user_agent(concat!("stitch/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ReleaseCheck::Failed {
                reason: format!("http client: {e}"),
            }
        }
    };
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return ReleaseCheck::Failed {
                reason: format!("request: {e}"),
            }
        }
    };
    if let Err(e) = response.error_for_status_ref() {
        return ReleaseCheck::Failed {
            reason: format!("status: {e}"),
        };
    }
    let release: GhRelease = match response.json().await {
        Ok(r) => r,
        Err(e) => {
            return ReleaseCheck::Failed {
                reason: format!("decode: {e}"),
            }
        }
    };
    match newer_than(current, &release.tag_name) {
        Ok(Some(latest)) => match select_desktop_asset(&release.assets) {
            Some(asset) => ReleaseCheck::Available { latest, asset },
            None => ReleaseCheck::Failed {
                reason: format!(
                    "release {} has no desktop download for {}",
                    release.tag_name,
                    desktop_target()
                ),
            },
        },
        Ok(None) => ReleaseCheck::Current,
        Err(reason) => ReleaseCheck::Failed { reason },
    }
}

/// Blocking wrapper so the synchronous GUI can run the check on a worker thread
/// without threading a runtime through its own code.
pub fn check_latest_release_blocking() -> ReleaseCheck {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(check_latest_release()),
        Err(e) => ReleaseCheck::Failed {
            reason: format!("runtime: {e}"),
        },
    }
}

/// Convenience for callers that only care about "is there something newer?"
/// Returns `None` for both Current and Failed — prefer [`check_latest_release`]
/// when you need to keep a previous Available result across transient errors.
pub async fn newer_release() -> Option<String> {
    match check_latest_release().await {
        ReleaseCheck::Available { latest, .. } => Some(latest),
        ReleaseCheck::Current | ReleaseCheck::Failed { .. } => None,
    }
}

/// Blocking wrapper matching [`newer_release`].
pub fn newer_release_blocking() -> Option<String> {
    match check_latest_release_blocking() {
        ReleaseCheck::Available { latest, .. } => Some(latest),
        ReleaseCheck::Current | ReleaseCheck::Failed { .. } => None,
    }
}

/// The pure comparison seam: return the normalized latest version when
/// `latest_tag` (e.g. "v0.2.0") parses to a semver strictly greater than
/// `current`. Parse failures stay distinct from "current" so callers never
/// clear a known update or tell the operator they are current on bad metadata.
fn newer_than(current: &str, latest_tag: &str) -> Result<Option<String>, String> {
    let latest = semver::Version::parse(latest_tag.strip_prefix('v').unwrap_or(latest_tag))
        .map_err(|e| format!("invalid release tag {latest_tag:?}: {e}"))?;
    let current = semver::Version::parse(current)
        .map_err(|e| format!("invalid current version {current:?}: {e}"))?;
    Ok((latest > current).then(|| latest.to_string()))
}

fn desktop_target() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        match std::env::consts::ARCH {
            "aarch64" => "Linux aarch64",
            _ => "Linux x86_64",
        }
    } else {
        "this platform"
    }
}

fn desktop_asset_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("Stitch.dmg")
    } else if cfg!(target_os = "windows") && std::env::consts::ARCH == "x86_64" {
        Some("stitch-bot-x86_64-pc-windows-msvc.zip")
    } else if cfg!(target_os = "linux") {
        match std::env::consts::ARCH {
            "aarch64" => Some("stitch-bot-aarch64-unknown-linux-gnu.tar.xz"),
            "x86_64" => Some("stitch-bot-x86_64-unknown-linux-gnu.tar.xz"),
            _ => None,
        }
    } else {
        None
    }
}

fn select_desktop_asset(assets: &[ReleaseAsset]) -> Option<ReleaseAsset> {
    let expected = desktop_asset_name()?;
    assets.iter().find(|asset| asset.name == expected).cloned()
}

/// Download the platform asset into a versioned temporary directory and verify
/// GitHub's immutable SHA-256 digest before returning it.
pub fn download_desktop_update_blocking(
    version: &str,
    asset: &ReleaseAsset,
) -> anyhow::Result<PathBuf> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating update download runtime")?;
    rt.block_on(download_desktop_update(version, asset))
}

async fn download_desktop_update(version: &str, asset: &ReleaseAsset) -> anyhow::Result<PathBuf> {
    if desktop_asset_name() != Some(asset.name.as_str()) {
        anyhow::bail!("refusing unexpected update asset {}", asset.name);
    }
    validate_asset_url(&asset.browser_download_url)?;
    let expected_digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .filter(|value| value.len() == 64)
        .context("release asset has no valid SHA-256 digest")?;

    let response = reqwest::Client::builder()
        .user_agent(concat!("stitch/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("creating update download client")?
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("downloading update")?
        .error_for_status()
        .context("downloading update")?;
    let bytes = response.bytes().await.context("reading update download")?;
    verify_sha256(&bytes, expected_digest)?;

    let directory = create_update_temp_dir(version)?;
    let partial = directory.join(format!("{}.part", asset.name));
    let destination = directory.join(&asset.name);
    if destination.exists() {
        std::fs::remove_file(&destination)
            .with_context(|| format!("removing stale {}", destination.display()))?;
    }
    std::fs::write(&partial, &bytes).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, &destination)
        .with_context(|| format!("finalizing {}", destination.display()))?;
    Ok(destination)
}

fn validate_asset_url(raw: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(raw).context("invalid release asset URL")?;
    let expected_path = format!("/{RELEASE_REPO}/releases/download/");
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.path().starts_with(&expected_path)
    {
        anyhow::bail!("refusing release asset outside the official GitHub repository");
    }
    Ok(())
}

fn create_update_temp_dir(version: &str) -> anyhow::Result<PathBuf> {
    for _ in 0..8 {
        let suffix = rand::rngs::OsRng.next_u64();
        let directory = std::env::temp_dir().join(format!("stitch-update-{version}-{suffix:016x}"));
        match create_private_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", directory.display()))
            }
        }
    }
    anyhow::bail!("couldn't allocate a private update directory")
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        return std::fs::DirBuilder::new().mode(0o700).create(path);
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

fn verify_sha256(bytes: &[u8], expected: &str) -> anyhow::Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        anyhow::bail!("update checksum mismatch (expected {expected}, got {actual})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        newer_than, select_desktop_asset, validate_asset_url, verify_sha256, ReleaseAsset,
        ReleaseCheck,
    };
    use sha2::Digest;

    #[test]
    fn flags_a_newer_tag() {
        assert_eq!(
            newer_than("0.1.0", "v0.2.0").unwrap().as_deref(),
            Some("0.2.0")
        );
    }

    #[test]
    fn tolerates_a_missing_v_prefix() {
        assert_eq!(
            newer_than("0.1.0", "0.2.0").unwrap().as_deref(),
            Some("0.2.0")
        );
    }

    #[test]
    fn ignores_the_same_version() {
        assert!(newer_than("0.1.0", "v0.1.0").unwrap().is_none());
    }

    #[test]
    fn ignores_an_older_tag() {
        assert!(newer_than("0.2.0", "v0.1.0").unwrap().is_none());
    }

    #[test]
    fn ignores_a_garbage_tag() {
        assert!(newer_than("0.1.0", "nightly").is_err());
    }

    #[test]
    fn release_check_variants_are_distinct() {
        // Pin the contract desktop relies on: Failed must not look like Current.
        assert_ne!(
            ReleaseCheck::Current,
            ReleaseCheck::Failed {
                reason: "offline".into()
            }
        );
        assert_ne!(
            ReleaseCheck::Available {
                latest: "0.2.0".into(),
                asset: ReleaseAsset {
                    name: "test".into(),
                    browser_download_url: "https://example.com/test".into(),
                    digest: None,
                },
            },
            ReleaseCheck::Current
        );
    }

    #[test]
    fn selects_only_the_current_platform_asset() {
        let expected = super::desktop_asset_name().expect("supported test platform");
        let assets = vec![
            ReleaseAsset {
                name: "unrelated.zip".into(),
                browser_download_url: "https://example.com/unrelated".into(),
                digest: None,
            },
            ReleaseAsset {
                name: expected.into(),
                browser_download_url: "https://example.com/update".into(),
                digest: Some(format!("sha256:{}", "a".repeat(64))),
            },
        ];
        assert_eq!(select_desktop_asset(&assets).unwrap().name, expected);
    }

    #[test]
    fn verifies_download_digest_before_installing() {
        let expected = format!("{:x}", sha2::Sha256::digest(b"verified update"));
        assert!(verify_sha256(b"verified update", &expected).is_ok());
        assert!(verify_sha256(b"tampered update", &expected).is_err());
    }

    #[test]
    fn accepts_only_assets_from_the_official_release_path() {
        assert!(validate_asset_url(
            "https://github.com/textile-protocol/textile-stitch/releases/download/v0.2.0/Stitch.dmg"
        )
        .is_ok());
        assert!(validate_asset_url(
            "http://github.com/textile-protocol/textile-stitch/releases/download/v0.2.0/Stitch.dmg"
        )
        .is_err());
        assert!(validate_asset_url("https://example.com/Stitch.dmg").is_err());
        assert!(validate_asset_url(
            "https://github.com/attacker/repo/releases/download/v0.2.0/Stitch.dmg"
        )
        .is_err());
    }
}
