// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Refuse state-changing requests that a different site made on the operator's
//! behalf.
//!
//! The session cookie is `SameSite=Strict`, which would be enough on its own. But
//! the panel's other credential is the `Tailscale-User-Login` header, and the
//! proxy attaches that to every request from an authorized device regardless of
//! cookies. So a page on any other site, open in a browser on that device, could
//! POST a plain HTML form at `/api/bots/<name>/stop` and have it arrive fully
//! authenticated. No JavaScript, no CORS preflight, nothing to opt into.
//!
//! The same credential also authenticates GETs. A cross-site page can't *read*
//! a followed log stream (CORS), but it can open one — and each open holds a
//! Docker connection and an HTTP response indefinitely. So the fetch-site check
//! covers those streaming GETs too.
//!
//! The defence is the two headers a page cannot forge or suppress: browsers set
//! `Sec-Fetch-Site` on every request, and `Origin` on every cross-origin form
//! post. Requests carrying neither are not browser requests — `curl`, a script,
//! a healthcheck — and are left alone.

use axum::http::{HeaderMap, Method};

const SEC_FETCH_SITE: &str = "sec-fetch-site";
const X_FORWARDED_HOST: &str = "x-forwarded-host";

/// Whether this request may proceed, given where it came from.
///
/// Ordinary reads are never blocked: they're readable cross-origin anyway, and
/// the SPA's own navigation depends on them. Mutations and followed log streams
/// are.
pub fn check(method: &Method, path: &str, headers: &HeaderMap) -> Result<(), String> {
    if !needs_same_origin(method, path) {
        return Ok(());
    }

    // Set by every current browser and unforgeable by page script, so it decides
    // on its own when present. `none` means the user themselves initiated it (a
    // typed URL, a bookmark), which no attacking page can produce.
    if let Some(site) = header(headers, SEC_FETCH_SITE) {
        return match site.as_str() {
            "same-origin" | "none" => Ok(()),
            other => Err(format!(
                "this looks like a {other} request from another site. State-changing calls have \
                 to come from the panel's own pages."
            )),
        };
    }

    // Older browser: fall back to comparing the origin it declares against the
    // host it asked for. A cross-origin form post always carries `Origin`.
    match (header(headers, "origin"), request_host(headers)) {
        (Some(origin), Some(host)) if authority(&origin).as_deref() != Some(host.as_str()) => {
            Err(format!(
                "this request came from {origin}, not from the panel at {host}. State-changing \
                 calls have to come from the panel's own pages."
            ))
        }
        // Neither header, or nothing to compare against: not a browser doing this
        // behind the operator's back.
        _ => Ok(()),
    }
}

fn needs_same_origin(method: &Method, path: &str) -> bool {
    if changes_state(method) {
        return true;
    }
    // Followed log streams hold a Docker connection open indefinitely. A
    // cross-site GET can't read the body, but with identity-header auth it can
    // still open the stream and exhaust panel or daemon connections.
    *method == Method::GET && is_bot_log_stream(path)
}

fn changes_state(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// `/api/bots/<name>/logs` — the followed log SSE endpoint.
fn is_bot_log_stream(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/bots/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let Some(name) = parts.next() else {
        return false;
    };
    !name.is_empty() && parts.next() == Some("logs") && parts.next().is_none()
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
}

/// The host the client believes it's talking to. `tailscale serve` passes the
/// original `Host` through, but a proxy that rewrites it should still say what
/// the client asked for in `X-Forwarded-Host`, so that wins when present.
fn request_host(headers: &HeaderMap) -> Option<String> {
    header(headers, X_FORWARDED_HOST)
        .and_then(|h| h.split(',').next().map(|h| h.trim().to_string()))
        .filter(|h| !h.is_empty())
        .or_else(|| header(headers, "host"))
}

/// The `host[:port]` out of an origin, or `None` for `null` and anything
/// unparseable — both of which must not be treated as matching.
fn authority(origin: &str) -> Option<String> {
    let rest = origin.split_once("://").map(|(_, rest)| rest)?;
    let authority = rest.split('/').next()?;
    (!authority.is_empty()).then(|| authority.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn ordinary_reads_are_never_blocked() {
        // Cross-origin reads can't be seen by the attacking page anyway, and
        // blocking them would break nothing but the panel's own navigation.
        let cross = headers(&[("sec-fetch-site", "cross-site")]);
        assert!(check(&Method::GET, "/api/bots", &cross).is_ok());
        assert!(check(&Method::GET, "/api/session", &cross).is_ok());
        assert!(check(&Method::HEAD, "/api/bots", &cross).is_ok());
    }

    #[test]
    fn a_cross_site_log_stream_is_refused() {
        // The attack: iframes on another page, each holding a followed Docker
        // log stream open forever under the operator's identity header.
        let cross = headers(&[("sec-fetch-site", "cross-site")]);
        let err = check(&Method::GET, "/api/bots/bot-a/logs", &cross).unwrap_err();
        assert!(err.contains("from another site"), "{err}");
        // Same-origin is fine — that's the SPA's own LogViewer.
        assert!(check(
            &Method::GET,
            "/api/bots/bot-a/logs",
            &headers(&[("sec-fetch-site", "same-origin")])
        )
        .is_ok());
    }

    #[test]
    fn a_cross_site_post_is_refused() {
        // The attack: a form on another page, submitted by a browser that
        // helpfully attaches the operator's tailnet identity.
        for site in ["cross-site", "same-site"] {
            let err = check(
                &Method::POST,
                "/api/bots/bot-a/stop",
                &headers(&[("sec-fetch-site", site)]),
            )
            .unwrap_err();
            assert!(err.contains("from another site"), "{err}");
        }
        assert!(check(
            &Method::DELETE,
            "/api/bots/bot-a",
            &headers(&[("sec-fetch-site", "cross-site")])
        )
        .is_err());
    }

    #[test]
    fn the_panels_own_pages_are_allowed() {
        for site in ["same-origin", "none"] {
            assert!(check(
                &Method::POST,
                "/api/bots/bot-a/stop",
                &headers(&[("sec-fetch-site", site)])
            )
            .is_ok());
        }
    }

    #[test]
    fn an_older_browser_is_judged_on_origin_against_host() {
        let same = headers(&[
            ("origin", "https://panel.tail1234.ts.net"),
            ("host", "panel.tail1234.ts.net"),
        ]);
        assert!(check(&Method::POST, "/api/bots/bot-a/stop", &same).is_ok());

        let other = headers(&[
            ("origin", "https://evil.example.com"),
            ("host", "panel.tail1234.ts.net"),
        ]);
        let err = check(&Method::POST, "/api/bots/bot-a/stop", &other).unwrap_err();
        assert!(err.contains("evil.example.com"), "{err}");

        // A sandboxed iframe posts `Origin: null`, which must not pass.
        let opaque = headers(&[("origin", "null"), ("host", "panel.tail1234.ts.net")]);
        assert!(check(&Method::POST, "/api/bots/bot-a/stop", &opaque).is_err());
    }

    #[test]
    fn a_rewritten_host_is_compared_against_what_the_client_asked_for() {
        // A proxy that rewrites Host must not make every mutation look foreign.
        let h = headers(&[
            ("origin", "https://panel.tail1234.ts.net"),
            ("x-forwarded-host", "panel.tail1234.ts.net"),
            ("host", "127.0.0.1:8420"),
        ]);
        assert!(check(&Method::POST, "/api/bots/bot-a/stop", &h).is_ok());
    }

    #[test]
    fn a_non_browser_client_is_left_alone() {
        // curl, a script, a healthcheck: no Origin, no Sec-Fetch-Site. Nothing to
        // protect against, because no browser is being tricked.
        assert!(check(
            &Method::POST,
            "/api/bots/bot-a/stop",
            &headers(&[("host", "panel.ts.net")])
        )
        .is_ok());
        assert!(check(&Method::POST, "/api/bots/bot-a/stop", &HeaderMap::new()).is_ok());
        assert!(check(&Method::GET, "/api/bots/bot-a/logs", &HeaderMap::new()).is_ok());
    }
}
