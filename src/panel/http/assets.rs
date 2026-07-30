// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The embedded single-page app.
//!
//! The built frontend is compiled into the binary by `rust-embed`, so the panel
//! ships as one file with no static-file directory to get out of sync with it. A
//! `cargo build` with no frontend build still works: `build.rs` creates the folder
//! `rust-embed` insists on, and a request then gets the placeholder below instead
//! of a 404 nobody can debug.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use super::ApiError;

/// The Vite build output, relative to the crate root.
#[derive(rust_embed::Embed)]
#[folder = "web/dist"]
struct Assets;

/// Serve a built asset, falling back to `index.html` so client-side routes reload.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(res) = lookup(path) {
        return res;
    }
    // An unrouted API path is a bug on one side or the other, most often a client
    // newer than the binary serving it. Answer in the shape the caller is parsing:
    // handing it the HTML shell turns a clear 404 into a JSON parse error.
    if path == "api" || path.starts_with("api/") {
        return ApiError::not_found(format!("no such endpoint: /{path}")).into_response();
    }
    // A path with an extension that isn't there is a genuine 404 — serving the
    // HTML shell as a stylesheet just produces a confusing console error.
    if std::path::Path::new(path).extension().is_some() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    lookup("index.html").unwrap_or_else(placeholder)
}

fn lookup(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Hashed filenames from Vite are safe to cache hard; index.html must not be,
    // or an upgraded panel keeps serving the old shell.
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache),
            ],
            file.data.into_owned(),
        )
            .into_response(),
    )
}

/// What a binary built without the frontend serves. Says what to do rather than
/// leaving an operator staring at a blank page.
fn placeholder() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("no-frontend.html"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_asset_with_an_extension_is_a_404() {
        let res = serve("/assets/nope-abc123.js".parse().unwrap()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_client_route_falls_back_to_the_shell_or_says_why_it_cant() {
        // With a built frontend this is the SPA shell; without one it's the
        // placeholder. Either way it must not be a bare 404.
        let res = serve("/bots/bot-a".parse().unwrap()).await;
        assert!(
            res.status() == StatusCode::OK || res.status() == StatusCode::SERVICE_UNAVAILABLE,
            "unexpected status {}",
            res.status()
        );
    }

    #[tokio::test]
    async fn an_unrouted_api_path_is_json_not_the_html_shell() {
        // A client calling an endpoint this binary doesn't have should read the
        // 404 it's parsing for, not choke on `<!doctype html>`.
        for path in ["/api/nope", "/api/bots/bot-a/teleport", "/api"] {
            let res = serve(path.parse().unwrap()).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
            let ct = res
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(ct.starts_with("application/json"), "{path} served {ct}");
        }
    }

    #[tokio::test]
    async fn the_shell_is_never_cached_hard() {
        // Otherwise an upgraded panel serves the old app against the new API.
        if let Some(res) = lookup("index.html") {
            let cache = res
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert_eq!(cache, "no-cache");
        }
    }
}
