// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The hosted panel's request-origin check, on top of the panel's usual
//! same-origin check ([`crate::panel::http::origin`]).
//!
//! A hosted panel is reached two ways and no others: the tailnet HTTPS origin
//! that `tailscale serve` answers on, and `http://localhost:8420` through the
//! SSM break-glass port forward. So:
//!
//! - `Host` must be one of those two authorities on every request, which also
//!   stops DNS rebinding onto the loopback listener. `X-Forwarded-Host` is not
//!   believed here, because the loopback listener has no proxy in front of it.
//! - a state-changing request must carry `Origin`, and it must be exactly one of
//!   the two origins. Browsers always send it on a POST/PUT/PATCH/DELETE, so the
//!   only clients this turns away are scripts that didn't say where they're from.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use super::HostedConfig;
use crate::panel::http::{ApiError, AppState};

pub fn check(cfg: &HostedConfig, method: &Method, headers: &HeaderMap) -> Result<(), String> {
    let allowed = cfg.allowed_origins();
    let host = header(headers, "host").unwrap_or_default();
    let host_ok = allowed
        .iter()
        .any(|origin| origin.split_once("://").map(|(_, a)| a) == Some(host.as_str()));
    if !host_ok {
        return Err(format!(
            "this hosted panel answers only at {} or {}, not at host {host:?}",
            allowed[0], allowed[1]
        ));
    }
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    match header(headers, "origin") {
        Some(origin) if allowed.contains(&origin.as_str()) => Ok(()),
        Some(origin) => Err(format!(
            "this request came from {origin}; a hosted panel accepts changes only from {} or {}",
            allowed[0], allowed[1]
        )),
        None => Err("state-changing requests to a hosted panel must send an Origin header".into()),
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

/// Outer layer on a hosted panel's whole router, assets included.
pub async fn enforce(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(cfg) = state.cfg.profile.hosted() {
        check(cfg, req.method(), req.headers())
            .map_err(|reason| ApiError::new(StatusCode::FORBIDDEN, reason))?;
    }
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::hosted::Envelope;

    fn cfg() -> HostedConfig {
        HostedConfig {
            public_origin: "https://stitch-panel.example.ts.net".into(),
            image_repository: "r".into(),
            envelope: Envelope::default(),
            broker_socket: "/x".into(),
            publish_bind: None,
            attestation_registry: crate::panel::attestation::registry::GHCR.into(),
        }
    }

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("host", host.parse().unwrap());
        if let Some(o) = origin {
            h.insert("origin", o.parse().unwrap());
        }
        h
    }

    #[test]
    fn tailnet_and_break_glass_origins_are_accepted() {
        for (host, origin) in [
            (
                "stitch-panel.example.ts.net",
                "https://stitch-panel.example.ts.net",
            ),
            ("localhost:8420", "http://localhost:8420"),
        ] {
            assert!(
                check(&cfg(), &Method::POST, &headers(host, Some(origin))).is_ok(),
                "{origin}"
            );
            assert!(
                check(&cfg(), &Method::GET, &headers(host, None)).is_ok(),
                "{host}"
            );
        }
    }

    #[test]
    fn anything_else_is_refused() {
        let cases = [
            ("localhost:8420", Some("http://localhost:9999")),
            ("localhost:8420", Some("http://127.0.0.1:8420")),
            ("localhost:8420", Some("https://localhost:8420")),
            ("localhost:8420", Some("null")),
            ("localhost:8420", Some("https://attacker.example")),
            ("localhost:8420", None),
            (
                "stitch-panel.example.ts.net",
                Some("http://stitch-panel.example.ts.net"),
            ),
            (
                "stitch-panel.example.ts.net",
                Some("https://other.example.ts.net"),
            ),
        ];
        for (host, origin) in cases {
            assert!(
                check(&cfg(), &Method::POST, &headers(host, origin)).is_err(),
                "{host} {origin:?}"
            );
        }
        for host in [
            "127.0.0.1:8420",
            "localhost",
            "attacker.example",
            "localhost:9999",
            "0.0.0.0:8420",
        ] {
            assert!(
                check(&cfg(), &Method::GET, &headers(host, None)).is_err(),
                "{host}"
            );
        }
        assert!(check(&cfg(), &Method::GET, &HeaderMap::new()).is_err());
    }
}
