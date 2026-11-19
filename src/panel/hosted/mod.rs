// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The hosted panel profile: Fleet for a host Textile runs for a client, built by
//! Terraform (`packages/infra/terraform/stitch-bots`), with keys the host loads
//! into tmpfs and the panel never touches.
//!
//! Opt-in only. `STITCH_PANEL_PROFILE=hosted` is the one thing that selects it.
//! An unset or empty value, or `full`, is the full Fleet every existing install
//! runs; any other value stops startup. Nothing else (`PANEL_MODE=server`,
//! `TS_AUTHKEY`, the bind address, the runtime) is read as a hint, so a legacy
//! host can't drift into it.
//!
//! What hosted changes:
//!
//! - no Docker socket in the panel: [`runtime::BrokerDocker`] talks to the
//!   fixed-policy [`crate::panel::broker`] over a Unix socket
//! - a route policy ([`policy`]) refuses everything outside the agreed list
//! - quoting edits must stay inside an envelope file ([`envelope`])
//! - Update and Rollback take only an `@sha256` digest ([`image`])
//! - password login only, and only from the tailnet HTTPS origin or the
//!   break-glass `http://localhost:8420` ([`origin`])

pub mod envelope;
#[cfg(test)]
mod http_tests;
pub mod image;
pub mod origin;
pub mod policy;
#[cfg(unix)]
pub mod runtime;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::panel::broker::{DEFAULT_IMAGE_REPOSITORY, SOCKET_PATH};
use crate::panel::config::{AuthMode, PanelRuntime};

pub use envelope::Envelope;

/// The origin the SSM `<prefix>-panel` port forward lands on. The forward has a
/// fixed remote port and the docs tell the operator to use local port 8420 too.
pub const BREAK_GLASS_ORIGIN: &str = "http://localhost:8420";

/// Which panel this process is.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Profile {
    /// Today's Fleet. Every install that doesn't say otherwise.
    #[default]
    Full,
    Hosted(HostedConfig),
}

impl Profile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::Full => "full",
            Profile::Hosted(_) => "hosted",
        }
    }

    pub fn hosted(&self) -> Option<&HostedConfig> {
        match self {
            Profile::Hosted(h) => Some(h),
            Profile::Full => None,
        }
    }
}

/// Settings only a hosted panel has. All come from the host's compose file.
#[derive(Debug, Clone, PartialEq)]
pub struct HostedConfig {
    /// `https://<machine>.<tailnet>.ts.net`, the daily access origin.
    pub public_origin: String,
    /// The one repository bots may be moved onto, by digest.
    pub image_repository: String,
    /// Bounds for quoting edits. Empty means no edit is allowed.
    pub envelope: Envelope,
    /// The runtime broker's socket.
    pub broker_socket: PathBuf,
    /// A second listener for the break-glass path, on the sidecar's fixed
    /// bridge address that the host's `127.0.0.1:8420` publish forwards to.
    /// The main listener stays on loopback for `tailscale serve`. Never a
    /// wildcard: `0.0.0.0` would also listen on the tailnet interface.
    pub publish_bind: Option<std::net::SocketAddr>,
}

impl HostedConfig {
    /// The origins a browser may send state-changing requests from.
    pub fn allowed_origins(&self) -> [&str; 2] {
        [self.public_origin.as_str(), BREAK_GLASS_ORIGIN]
    }
}

/// Read the profile through `lookup` (the process environment in production).
pub fn profile_from(lookup: impl Fn(&str) -> Option<String>) -> Result<Profile> {
    let raw = lookup("STITCH_PANEL_PROFILE").unwrap_or_default();
    match raw.trim() {
        "" | "full" => Ok(Profile::Full),
        "hosted" => hosted_from(&lookup).map(Profile::Hosted),
        other => bail!(
            "STITCH_PANEL_PROFILE must be unset, \"full\" or \"hosted\", not {other:?}. \
             Leave it unset unless this is a Terraform-built hosted host."
        ),
    }
}

fn hosted_from(lookup: &impl Fn(&str) -> Option<String>) -> Result<HostedConfig> {
    let public_origin = lookup("STITCH_PANEL_PUBLIC_ORIGIN").unwrap_or_default();
    let public_origin = check_public_origin(public_origin.trim())?;
    let image_repository = lookup("STITCH_PANEL_IMAGE_REPOSITORY")
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE_REPOSITORY.to_string());
    let envelope = match lookup("STITCH_PANEL_ENVELOPE").filter(|p| !p.trim().is_empty()) {
        Some(path) => {
            let text = std::fs::read_to_string(path.trim())
                .with_context(|| format!("reading the quoting envelope at {path}"))?;
            Envelope::parse(&text)
                .with_context(|| format!("parsing the quoting envelope at {path}"))?
        }
        None => Envelope::default(),
    };
    let broker_socket = lookup("STITCH_PANEL_BROKER_SOCKET")
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SOCKET_PATH));
    let publish_bind = match lookup("STITCH_PANEL_PUBLISH_BIND").filter(|v| !v.trim().is_empty()) {
        Some(raw) => Some(check_publish_bind(raw.trim())?),
        None => None,
    };
    Ok(HostedConfig {
        public_origin,
        image_repository,
        envelope,
        broker_socket,
        publish_bind,
    })
}

/// A private IPv4 address and port: the sidecar's address on its own Docker
/// bridge network. Loopback is the main listener's job; wildcards, public and
/// tailnet addresses are refused.
fn check_publish_bind(raw: &str) -> Result<std::net::SocketAddr> {
    let addr: std::net::SocketAddr = raw.parse().with_context(|| {
        format!(
            "STITCH_PANEL_PUBLISH_BIND must be an address like 192.168.254.250:8420, not {raw:?}"
        )
    })?;
    let private = matches!(addr.ip(), std::net::IpAddr::V4(v4) if v4.is_private());
    if !private {
        bail!(
            "STITCH_PANEL_PUBLISH_BIND must be a private IPv4 address on the sidecar's bridge \
             network (10/8, 172.16/12, 192.168/16), not {addr}. Never 0.0.0.0: that would also \
             listen on the tailnet interface."
        );
    }
    Ok(addr)
}

/// `https://<host>.ts.net` with no port, path or trailing slash, lowercased.
fn check_public_origin(raw: &str) -> Result<String> {
    let lower = raw.to_ascii_lowercase();
    let host = lower.strip_prefix("https://").unwrap_or("");
    let ok = host.ends_with(".ts.net")
        && host.len() > ".ts.net".len()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'));
    if !ok {
        bail!(
            "STITCH_PANEL_PUBLIC_ORIGIN must be the panel's tailnet HTTPS origin, like \
             https://stitch-panel.example.ts.net (no port, path or trailing slash), not {raw:?}"
        );
    }
    Ok(lower)
}

/// Refuse a hosted panel that could be driven without its password.
///
/// The hosted compose file publishes the panel on host loopback for the SSM
/// break-glass forward, so any process on the host can reach the listener and
/// could forge `Tailscale-User-Login`. The identity header is therefore never
/// trusted on hosted, and a password is required.
pub fn check_startup(
    profile: &Profile,
    runtime: PanelRuntime,
    auth: &AuthMode,
    trust_identity_header: bool,
) -> Result<()> {
    if profile.hosted().is_none() {
        return Ok(());
    }
    if cfg!(not(unix)) {
        bail!("the hosted profile talks to its runtime broker over a Unix socket and needs Linux");
    }
    if runtime != PanelRuntime::Docker {
        bail!(
            "the hosted profile runs bots through the runtime broker; unset STITCH_PANEL_RUNTIME"
        );
    }
    if trust_identity_header {
        bail!(
            "the hosted profile never trusts Tailscale-User-Login: the panel is also published on \
             host loopback for break-glass, where any local process could forge it. Unset \
             STITCH_PANEL_TRUST_IDENTITY_HEADER and STITCH_PANEL_IDENTITY_PROXY_ONLY."
        );
    }
    if auth.password_hash().is_none() {
        bail!("the hosted profile requires STITCH_PANEL_PASSWORD_HASH");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn unset_empty_and_full_are_the_full_profile() {
        for pairs in [
            vec![],
            vec![("STITCH_PANEL_PROFILE", "")],
            vec![("STITCH_PANEL_PROFILE", " full ")],
        ] {
            assert_eq!(profile_from(env(&pairs)).unwrap(), Profile::Full);
        }
    }

    /// The legacy server install sets these. None of them may select hosted.
    #[test]
    fn server_mode_settings_never_select_hosted() {
        let legacy = env(&[
            ("PANEL_MODE", "server"),
            ("TS_AUTHKEY", "tskey-auth-xyz"),
            ("STITCH_PANEL_BIND", "127.0.0.1:8420"),
            ("STITCH_PANEL_TRUST_IDENTITY_HEADER", "1"),
            ("STITCH_PANEL_IDENTITY_PROXY_ONLY", "1"),
            (
                "STITCH_PANEL_PUBLIC_ORIGIN",
                "https://stitch-panel.example.ts.net",
            ),
            ("STITCH_PANEL_ENVELOPE", "/nonexistent"),
        ]);
        assert_eq!(profile_from(legacy).unwrap(), Profile::Full);
    }

    #[test]
    fn unknown_values_stop_startup_instead_of_guessing() {
        for value in ["Hosted", "HOSTED", "server", "1", "true", "hosted-lite"] {
            assert!(
                profile_from(env(&[("STITCH_PANEL_PROFILE", value)])).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn hosted_needs_a_tailnet_https_origin() {
        let ok = profile_from(env(&[
            ("STITCH_PANEL_PROFILE", "hosted"),
            (
                "STITCH_PANEL_PUBLIC_ORIGIN",
                "https://Stitch-Panel.example.ts.net",
            ),
        ]))
        .unwrap();
        let hosted = ok.hosted().unwrap();
        assert_eq!(hosted.public_origin, "https://stitch-panel.example.ts.net");
        assert_eq!(hosted.envelope, Envelope::default());
        assert_eq!(hosted.image_repository, DEFAULT_IMAGE_REPOSITORY);
        assert_eq!(hosted.publish_bind, None);
        for origin in [
            "",
            "http://stitch-panel.example.ts.net",
            "https://stitch-panel.example.ts.net/",
            "https://stitch-panel.example.ts.net:443",
            "https://evil.example.com",
            "https://ts.net",
            "https://x.ts.net.evil.com",
        ] {
            assert!(
                profile_from(env(&[
                    ("STITCH_PANEL_PROFILE", "hosted"),
                    ("STITCH_PANEL_PUBLIC_ORIGIN", origin),
                ]))
                .is_err(),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn the_publish_listener_is_a_private_bridge_address_only() {
        let with = |bind: &str| {
            profile_from(env(&[
                ("STITCH_PANEL_PROFILE", "hosted"),
                ("STITCH_PANEL_PUBLIC_ORIGIN", "https://p.example.ts.net"),
                ("STITCH_PANEL_PUBLISH_BIND", bind),
            ]))
        };
        let ok = with("192.168.254.250:8420").unwrap();
        assert_eq!(
            ok.hosted().unwrap().publish_bind,
            Some("192.168.254.250:8420".parse().unwrap())
        );
        for bind in [
            "0.0.0.0:8420",
            "127.0.0.1:8420",
            "100.64.0.1:8420",
            "8.8.8.8:8420",
            "[::]:8420",
            "nonsense",
        ] {
            assert!(with(bind).is_err(), "{bind}");
        }
    }

    #[test]
    fn hosted_refuses_header_trust_and_missing_password() {
        let hosted = profile_from(env(&[
            ("STITCH_PANEL_PROFILE", "hosted"),
            ("STITCH_PANEL_PUBLIC_ORIGIN", "https://p.example.ts.net"),
        ]))
        .unwrap();
        let password = AuthMode::Password {
            hash: "$argon2id$x".into(),
        };
        let tailnet = AuthMode::Tailnet {
            allowed_users: vec!["a@example.com".into()],
        };
        assert!(check_startup(&hosted, PanelRuntime::Docker, &password, false).is_ok());
        assert!(check_startup(&hosted, PanelRuntime::Docker, &password, true).is_err());
        assert!(check_startup(&hosted, PanelRuntime::Docker, &tailnet, false).is_err());
        assert!(check_startup(&hosted, PanelRuntime::Process, &password, false).is_err());
        // The full profile is untouched by these checks.
        assert!(check_startup(&Profile::Full, PanelRuntime::Docker, &tailnet, true).is_ok());
    }
}
