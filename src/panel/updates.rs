// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Detect whether a bot or the panel itself is behind a newer published image.
//!
//! Compares a running container's local image digests against the registry
//! manifest for a target tag. Offline / private-registry failures degrade to
//! "no update available" rather than blocking the fleet UI.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

use super::docker::{ContainerInfo, DockerApi};
use super::inventory::Fleet;
use super::PanelConfig;

/// How long a successful (or soft-failed) registry check is reused. Pulling the
/// fleet every few seconds must not hammer GHCR.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Parsed image reference: `registry/repo:tag` or bare `name:tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: Option<String>,
    pub repository: String,
    pub tag: String,
}

/// Split `ghcr.io/org/name:tag` (or `name:tag`, or digest refs) into parts.
pub fn parse_image_ref(image: &str) -> ImageRef {
    let (without_digest, _digest) = match image.split_once('@') {
        Some((name, digest)) => (name, Some(digest)),
        None => (image, None),
    };
    let (name, tag) = match without_digest.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, tag.to_string()),
        _ => (without_digest, "latest".to_string()),
    };
    // A first path component with a dot or colon is a registry host.
    let mut parts = name.splitn(2, '/');
    let first = parts.next().unwrap_or(name);
    if (first.contains('.') || first.contains(':') || first == "localhost")
        && parts.next().is_some()
    {
        let repo = name[first.len() + 1..].to_string();
        ImageRef {
            registry: Some(first.to_string()),
            repository: repo,
            tag,
        }
    } else {
        ImageRef {
            registry: None,
            repository: name.to_string(),
            tag,
        }
    }
}

/// The image tag an update should pull for a running container.
///
/// Mutable channels (`latest`, branch names without `sha-`) keep their tag.
/// Pinned `sha-*` tags and digests resolve to the same repository's `:latest`
/// so a pin can still see a newer publish. Local-only names (no registry) yield
/// `None` — the panel can't pull what it built by hand.
pub fn update_target_image(running_image: &str) -> Option<String> {
    let parsed = parse_image_ref(running_image);
    let registry = parsed.registry.as_deref()?;
    let tag = if parsed.tag.starts_with("sha-") || running_image.contains('@') {
        "latest"
    } else {
        parsed.tag.as_str()
    };
    Some(format!("{registry}/{repo}:{tag}", repo = parsed.repository))
}

/// Whether two image refs share registry + repository (ignoring tag/digest).
///
/// Update detection compares digests against `STITCH_PANEL_BOT_IMAGE`'s remote
/// manifest. A custom/forked bot on another repository would almost always look
/// "behind" that digest — and Update would recreate it onto the default image.
/// Those bots are out of scope for the panel Update button.
pub fn same_image_repository(a: &str, b: &str) -> bool {
    let a = parse_image_ref(a);
    let b = parse_image_ref(b);
    a.registry == b.registry && a.repository == b.repository
}

/// Whether a running bot image is on `STITCH_PANEL_BOT_IMAGE`'s update channel.
///
/// Same repository is not enough: a bot on `:canary` must not be Updated onto
/// `:latest` just because they share a repo. Mutable tags must match.
///
/// An explicit `sha-*` / digest pin on the *bot* may always Update onto the
/// configured channel's target — operators pin for reproducibility, then use
/// Update to leave the pin. When the *configured* image is itself a pin, bots
/// already on the resolved `:latest` target stay eligible for later releases
/// (otherwise a successful pin→latest Update would leave them "off channel"
/// forever while the env still names the pin).
pub fn bot_eligible_for_configured_update(current: &str, configured: &str) -> bool {
    if !same_image_repository(current, configured) {
        return false;
    }
    let cur = parse_image_ref(current);
    // Any same-repo sha-* / digest pin can leave the pin via Update, whether
    // STITCH_PANEL_BOT_IMAGE is `:latest`, another pin, or a mutable channel.
    if cur.tag.starts_with("sha-") || current.contains('@') {
        return true;
    }
    let cfg = parse_image_ref(configured);
    let cfg_is_pin = cfg.tag.starts_with("sha-") || configured.contains('@');
    if cfg_is_pin {
        // Already moved to the resolved update target (:latest). Exact ref only —
        // digest refs are handled above as pins (a synthetic `latest` tag would
        // otherwise treat any same-repo `@sha256:…` as on-channel).
        return update_target_image(configured).is_some_and(|target| current == target);
    }
    // Mutable channel: already on that tag.
    cur.tag == cfg.tag
}

/// True when `remote` (a `sha256:…` digest) is not among the local RepoDigests.
///
/// Local digests look like `ghcr.io/org/name@sha256:…`; we compare the digest
/// suffix. Empty local digests mean we can't tell — treat as not behind so a
/// missing local image doesn't nag.
pub fn is_behind(local_digests: &[String], remote_digest: &str) -> bool {
    if local_digests.is_empty() || remote_digest.is_empty() {
        return false;
    }
    let remote = remote_digest.trim();
    let remote_norm = remote.strip_prefix("sha256:").unwrap_or(remote);
    !local_digests.iter().any(|d| {
        let suffix = d.rsplit_once('@').map(|(_, dig)| dig).unwrap_or(d.as_str());
        let suffix = suffix.strip_prefix("sha256:").unwrap_or(suffix);
        suffix == remote_norm
    })
}

/// Find the panel's own container among the host list.
///
/// Prefers `$HOSTNAME` / container-id match first — Docker sets HOSTNAME to the
/// short id of *this* container. An exact `stitch-panel` name match is only the
/// fallback, and prefers a running one, so a leftover stopped `stitch-panel`
/// cannot win over the live process when the panel was renamed.
pub fn find_self_container<'a>(
    containers: &'a [ContainerInfo],
    hostname: &str,
) -> Option<&'a ContainerInfo> {
    let host = hostname.trim();
    if !host.is_empty() {
        if let Some(c) = containers
            .iter()
            .find(|c| c.id.starts_with(host) || c.id.starts_with(&format!("sha256:{host}")))
        {
            return Some(c);
        }
    }
    containers
        .iter()
        .find(|c| c.name == "stitch-panel" && c.state.is_running())
        .or_else(|| containers.iter().find(|c| c.name == "stitch-panel"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageUpdateInfo {
    pub target_image: String,
    pub current_image: Option<String>,
    pub update_available: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotUpdateInfo {
    pub name: String,
    pub current_image: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatesStatus {
    pub bot: ImageUpdateInfo,
    pub panel: ImageUpdateInfo,
    pub bots: Vec<BotUpdateInfo>,
}

struct CacheEntry {
    at: Instant,
    status: UpdatesStatus,
}

/// Process-wide cache so fleet polls don't re-hit the registry every few seconds.
static CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

pub fn clear_cache() {
    *CACHE.lock().unwrap() = None;
}

/// Build the updates status for the fleet + the panel container.
pub async fn check_updates(
    cfg: &PanelConfig,
    docker: &dyn DockerApi,
    fleet: &Fleet,
    containers: &[ContainerInfo],
    force_refresh: bool,
) -> UpdatesStatus {
    if !force_refresh {
        if let Ok(guard) = CACHE.lock() {
            if let Some(entry) = guard.as_ref() {
                if entry.at.elapsed() < CACHE_TTL {
                    return entry.status.clone();
                }
            }
        }
    }

    // Same target resolution as POST /api/bots/{name}/update: pinned sha-* /
    // digest refs become `:latest` so detection and the Update action agree.
    let bot_target = update_target_image(&cfg.bot_image).unwrap_or_else(|| cfg.bot_image.clone());
    let bot_remote = fetch_remote_digest(&bot_target).await;

    let mut bots = Vec::new();
    for bot in fleet.bots() {
        let current = bot.image.clone();
        // Prefer the content-addressed id the container is *actually* running.
        // Looking up digests by a mutable tag (`:latest`) would resolve to
        // whatever was last pulled — so updating bot A would make bot B on the
        // same tag look current even though B still runs the old digest.
        let lookup = bot
            .image_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .or(current.as_deref());
        let on_channel = current
            .as_deref()
            .is_some_and(|img| bot_eligible_for_configured_update(img, &cfg.bot_image));
        let update_available = if !on_channel {
            // Wrong repo or wrong tag channel (e.g. :canary vs :latest) — Update
            // would recreate onto the configured image/channel.
            false
        } else {
            match (lookup, &bot_remote) {
                (Some(img), Ok(remote)) => match docker.local_image_digests(img).await {
                    Ok(local) => is_behind(&local, remote),
                    // No local digests → can't confirm. Don't fall back to string
                    // drift: a sha-* pin always differs from the resolved `:latest`
                    // target, which would nag forever without a digest compare.
                    Err(_) => false,
                },
                // Registry unreachable / auth failure: unknown, not "behind".
                (Some(_), Err(_)) => false,
                (None, _) => false,
            }
        };
        bots.push(BotUpdateInfo {
            name: bot.name.clone(),
            current_image: current,
            update_available,
        });
    }

    let bot_info = ImageUpdateInfo {
        target_image: bot_target.clone(),
        current_image: Some(bot_target.clone()),
        update_available: bots.iter().any(|b| b.update_available),
        reason: bot_remote.as_ref().err().map(|e| format!("{e:#}")),
    };

    let hostname = std::env::var("HOSTNAME").unwrap_or_default();
    let panel_info = match find_self_container(containers, &hostname) {
        Some(self_ctr) => {
            match update_target_image(&self_ctr.image) {
                Some(target) => {
                    let remote = fetch_remote_digest(&target).await;
                    // Same rule as bots: key off the running image id, not the
                    // mutable tag, so a pull for a bot doesn't mask a panel update.
                    let lookup = if self_ctr.image_id.is_empty() {
                        self_ctr.image.as_str()
                    } else {
                        self_ctr.image_id.as_str()
                    };
                    let local = docker.local_image_digests(lookup).await.unwrap_or_default();
                    let available = match &remote {
                        Ok(d) => is_behind(&local, d),
                        Err(_) => false,
                    };
                    ImageUpdateInfo {
                        target_image: target,
                        current_image: Some(self_ctr.image.clone()),
                        update_available: available,
                        reason: remote.err().map(|e| format!("{e:#}")),
                    }
                }
                None => ImageUpdateInfo {
                    target_image: self_ctr.image.clone(),
                    current_image: Some(self_ctr.image.clone()),
                    update_available: false,
                    reason: Some(
                        "this panel image has no registry path, so it can't be pulled \
                         for an update — rebuild locally or point PANEL_IMAGE at \
                         ghcr.io/textile-protocol/textile-stitch-panel"
                            .into(),
                    ),
                },
            }
        }
        None => ImageUpdateInfo {
            target_image: String::new(),
            current_image: None,
            update_available: false,
            reason: Some(
                "couldn't find this panel's container (expected name stitch-panel, \
                 or HOSTNAME matching a container id) — self-update is unavailable"
                    .into(),
            ),
        },
    };

    let status = UpdatesStatus {
        bot: bot_info,
        panel: panel_info,
        bots,
    };
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(CacheEntry {
            at: Instant::now(),
            status: status.clone(),
        });
    }
    status
}

/// Fetch the content digest for an image tag from its registry.
///
/// Talks to the Docker Distribution API (token + manifests). Public GHCR packages
/// work anonymously; anything else soft-fails.
pub async fn fetch_remote_digest(image: &str) -> Result<String> {
    let parsed = parse_image_ref(image);
    let registry = parsed
        .registry
        .as_deref()
        .context("image has no registry host")?;
    let repo = &parsed.repository;
    let tag = &parsed.tag;

    let client = reqwest::Client::builder()
        .user_agent(concat!("stitch-panel/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .context("building the registry HTTP client")?;

    let token = registry_token(&client, registry, repo).await?;

    // Docker Hub's Distribution API lives on registry-1.docker.io; docker.io
    // is only the Hub website / image-ref host. Token auth already special-cases
    // both names — the manifest URL must too, or Hub images always soft-fail.
    let api_host = distribution_api_host(registry);
    let url = format!("https://{api_host}/v2/{repo}/manifests/{tag}");
    let mut req = client.get(&url).header(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
             application/vnd.oci.image.manifest.v1+json, \
             application/vnd.oci.image.index.v1+json, \
             application/vnd.docker.distribution.manifest.list.v2+json",
    );
    if let Some(t) = &token {
        req = req.bearer_auth(t);
    }
    let res = req
        .send()
        .await
        .with_context(|| format!("requesting manifest for {image}"))?
        .error_for_status()
        .with_context(|| format!("registry rejected the manifest for {image}"))?;

    // Docker-Content-Digest is the canonical comparison key.
    if let Some(d) = res.headers().get("docker-content-digest") {
        if let Ok(s) = d.to_str() {
            return Ok(s.to_string());
        }
    }
    anyhow::bail!("registry response for {image} had no Docker-Content-Digest header")
}

/// Host for the Docker Distribution HTTP API for a parsed image registry.
fn distribution_api_host(registry: &str) -> &str {
    if registry == "docker.io" {
        "registry-1.docker.io"
    } else {
        registry
    }
}

async fn registry_token(
    client: &reqwest::Client,
    registry: &str,
    repo: &str,
) -> Result<Option<String>> {
    // GHCR (and most registries) want a bearer token before serving manifests.
    let token_url = if registry == "ghcr.io" {
        format!("https://ghcr.io/token?service=ghcr.io&scope=repository:{repo}:pull")
    } else if registry == "docker.io" || registry == "registry-1.docker.io" {
        format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"
        )
    } else {
        // Try the anonymous path first; many private registries need creds we don't have.
        return Ok(None);
    };
    let res = client.get(&token_url).send().await?;
    if !res.status().is_success() {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    struct TokenBody {
        token: Option<String>,
        access_token: Option<String>,
    }
    let body: TokenBody = res.json().await?;
    Ok(body.token.or(body.access_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ghcr_refs() {
        let r = parse_image_ref("ghcr.io/textile-protocol/textile-stitch:sha-abc");
        assert_eq!(r.registry.as_deref(), Some("ghcr.io"));
        assert_eq!(r.repository, "textile-protocol/textile-stitch");
        assert_eq!(r.tag, "sha-abc");
    }

    #[test]
    fn parses_local_names_without_a_registry() {
        let r = parse_image_ref("stitch-panel:latest");
        assert!(r.registry.is_none());
        assert_eq!(r.repository, "stitch-panel");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn pinned_sha_updates_resolve_to_latest() {
        assert_eq!(
            update_target_image("ghcr.io/textile-protocol/textile-stitch-panel:sha-deadbeef")
                .as_deref(),
            Some("ghcr.io/textile-protocol/textile-stitch-panel:latest")
        );
    }

    #[test]
    fn docker_hub_manifests_use_registry_1_host() {
        assert_eq!(distribution_api_host("docker.io"), "registry-1.docker.io");
        assert_eq!(distribution_api_host("ghcr.io"), "ghcr.io");
        assert_eq!(
            distribution_api_host("registry-1.docker.io"),
            "registry-1.docker.io"
        );
    }

    #[test]
    fn same_repository_ignores_tag_and_rejects_forks() {
        assert!(same_image_repository(
            "ghcr.io/textile-protocol/textile-stitch:sha-old",
            "ghcr.io/textile-protocol/textile-stitch:latest"
        ));
        assert!(!same_image_repository(
            "ghcr.io/acme/stitch-fork:v9",
            "ghcr.io/textile-protocol/textile-stitch:latest"
        ));
    }

    #[test]
    fn update_eligibility_requires_the_configured_channel() {
        let latest = "ghcr.io/textile-protocol/textile-stitch:latest";
        let canary = "ghcr.io/textile-protocol/textile-stitch:canary";
        let pin = "ghcr.io/textile-protocol/textile-stitch:sha-deadbeef";
        let other_pin = "ghcr.io/textile-protocol/textile-stitch:sha-cafebabe";
        let digest_pin = "ghcr.io/textile-protocol/textile-stitch@sha256:aaaa";
        assert!(bot_eligible_for_configured_update(latest, latest));
        assert!(!bot_eligible_for_configured_update(canary, latest));
        assert!(bot_eligible_for_configured_update(pin, pin));
        // After a pin→latest Update, stay on the channel for later releases.
        assert!(bot_eligible_for_configured_update(latest, pin));
        assert!(!bot_eligible_for_configured_update(canary, pin));
        assert!(!bot_eligible_for_configured_update(
            "ghcr.io/acme/stitch-fork:v9",
            latest
        ));
        // Bots still on a sha-* / digest pin must be able to Update onto the
        // panel's mutable channel (the common "panel is :latest, bot is sha-…"
        // case) — and onto :latest when the env itself still names a pin.
        assert!(bot_eligible_for_configured_update(pin, latest));
        assert!(bot_eligible_for_configured_update(digest_pin, latest));
        assert!(bot_eligible_for_configured_update(other_pin, pin));
        assert!(bot_eligible_for_configured_update(
            "ghcr.io/textile-protocol/textile-stitch@sha256:other",
            pin
        ));
        assert!(bot_eligible_for_configured_update(digest_pin, digest_pin));
        assert!(bot_eligible_for_configured_update(latest, digest_pin));
        assert!(bot_eligible_for_configured_update(
            "ghcr.io/textile-protocol/textile-stitch@sha256:bbbb",
            digest_pin
        ));
    }

    #[test]
    fn latest_keeps_its_tag() {
        assert_eq!(
            update_target_image("ghcr.io/textile-protocol/textile-stitch:latest").as_deref(),
            Some("ghcr.io/textile-protocol/textile-stitch:latest")
        );
    }

    #[test]
    fn local_builds_cannot_self_update() {
        assert!(update_target_image("stitch-panel").is_none());
        assert!(update_target_image("stitch-panel:latest").is_none());
    }

    #[test]
    fn behind_when_remote_digest_is_missing_locally() {
        let local = vec!["ghcr.io/textile-protocol/textile-stitch@sha256:aaaa".into()];
        assert!(is_behind(&local, "sha256:bbbb"));
        assert!(!is_behind(&local, "sha256:aaaa"));
        assert!(!is_behind(&[], "sha256:bbbb"));
    }

    #[test]
    fn finds_panel_by_name_or_hostname() {
        let containers = vec![
            crate::panel::docker::fake::container(
                "stitch-bot-a",
                crate::panel::docker::ContainerState::Running,
            ),
            {
                let mut c = crate::panel::docker::fake::container(
                    "stitch-panel",
                    crate::panel::docker::ContainerState::Running,
                );
                c.id = "abcdef0123456789".into();
                c
            },
        ];
        assert_eq!(
            find_self_container(&containers, "other").map(|c| c.name.as_str()),
            Some("stitch-panel")
        );
        let by_host = vec![{
            let mut c = crate::panel::docker::fake::container(
                "something-else",
                crate::panel::docker::ContainerState::Running,
            );
            c.id = "deadbeefcafebabe".into();
            c.image = "ghcr.io/textile-protocol/textile-stitch-panel:latest".into();
            c
        }];
        assert_eq!(
            find_self_container(&by_host, "deadbeef").map(|c| c.id.as_str()),
            Some("deadbeefcafebabe")
        );
    }

    #[test]
    fn prefers_hostname_over_a_stale_stitch_panel_name() {
        let mut stale = crate::panel::docker::fake::container(
            "stitch-panel",
            crate::panel::docker::ContainerState::Exited,
        );
        stale.id = "stale0000000000".into();
        let mut live = crate::panel::docker::fake::container(
            "custom-panel",
            crate::panel::docker::ContainerState::Running,
        );
        live.id = "livebeefcafebabe".into();
        live.image = "ghcr.io/textile-protocol/textile-stitch-panel:latest".into();
        let containers = vec![stale, live];
        assert_eq!(
            find_self_container(&containers, "livebeef").map(|c| c.name.as_str()),
            Some("custom-panel")
        );
    }
}
