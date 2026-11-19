// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Which panel routes a hosted panel serves.
//!
//! One table, [`ROUTES`], classifies every route the panel registers, keyed by
//! method and the route template axum matched. [`enforce`] runs after routing
//! on a hosted panel and refuses anything the table doesn't allow, including a
//! route that isn't in the table at all: a route added to the panel later is
//! refused on hosted until someone classifies it here. The full profile never
//! runs this layer.
//!
//! "Allow" means the handler runs as usual, with the hosted runtime under it
//! (the broker, not Docker) and, for settings saves, the quoting envelope.

use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::panel::http::{ApiError, AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Allow,
    /// Refused, with the reason shown to the operator.
    Refuse(&'static str),
}

const ADD_BOT: &str = "adding a bot or a corridor is done by the host's administrators";
const SIGNER: &str = "signers and keys are managed by the host, not the panel";
const CHAIN: &str = "RPCs, chain, pools and vaults are managed by the host's administrators";
const FUNDS: &str =
    "withdrawals, fee-share sends and approvals are done from the client's custody, not the panel";
const TAGS: &str = "this host only runs images by @sha256 digest; use Deploy digest";
const RAW: &str = "raw config and key files are not readable or writable on a hosted panel";
const SELF_UPDATE: &str = "the hosted panel is updated through the host's Terraform, by digest";
const STRATEGY: &str =
    "quote modules, perps and manual sales aren't in the agreed quoting envelope";
const IDENTITY: &str = "agent identity registration signs with the bot's key and is admin-only";
const DESKTOP: &str = "desktop controls don't exist on a hosted panel";
const LAYOUT: &str = "container layout is owned by the host's compose file";
const PASSWORD: &str = "the hosted panel password is set by the host's administrators";
const FETCH: &str = "the hosted panel doesn't fetch operator-supplied URLs";

/// Every route the panel registers, by method and template, and what a hosted
/// panel does with it. The PR description's route table is generated from this.
pub const ROUTES: &[(&str, &str, Rule)] = &[
    // Public: how you sign in.
    ("GET", "/api/session", Rule::Allow),
    ("POST", "/api/login", Rule::Allow),
    ("POST", "/api/logout", Rule::Allow),
    ("GET", "/api/health", Rule::Allow),
    // Status, logs, change log.
    ("GET", "/api/bots", Rule::Allow),
    ("GET", "/api/bots/{name}", Rule::Allow),
    ("GET", "/api/changes", Rule::Allow),
    ("GET", "/api/bots/{name}/logs", Rule::Allow),
    ("GET", "/api/bots/{name}/settings", Rule::Allow),
    ("GET", "/api/bots/{name}/funding", Rule::Allow),
    ("GET", "/api/bots/{name}/allowances", Rule::Allow),
    ("GET", "/api/bots/{name}/identity", Rule::Allow),
    ("GET", "/api/bots/{name}/modules", Rule::Allow),
    ("GET", "/api/bots/{name}/perps", Rule::Allow),
    ("GET", "/api/bots/{name}/modules/manual-sales", Rule::Allow),
    // Lifecycle.
    ("POST", "/api/bots/{name}/start", Rule::Allow),
    ("POST", "/api/bots/{name}/stop", Rule::Allow),
    ("POST", "/api/bots/{name}/restart", Rule::Allow),
    // Quoting edits, checked against the envelope in the handler.
    ("PATCH", "/api/bots/{name}/settings", Rule::Allow),
    // Update and rollback by digest (hosted-only route).
    ("POST", "/api/bots/{name}/image", Rule::Allow),
    // Refused.
    ("POST", "/api/panel/password", Rule::Refuse(PASSWORD)),
    ("GET", "/api/corridors", Rule::Refuse(ADD_BOT)),
    ("GET", "/api/feed/mid", Rule::Refuse(FETCH)),
    ("POST", "/api/wallets/generate", Rule::Refuse(SIGNER)),
    ("POST", "/api/signer/check", Rule::Refuse(SIGNER)),
    (
        "POST",
        "/api/signer/fireblocks/vaults",
        Rule::Refuse(SIGNER),
    ),
    (
        "POST",
        "/api/signer/fireblocks/verify",
        Rule::Refuse(SIGNER),
    ),
    ("POST", "/api/signer/gcpkms/verify", Rule::Refuse(SIGNER)),
    ("POST", "/api/signer/awskms/verify", Rule::Refuse(SIGNER)),
    ("POST", "/api/bots", Rule::Refuse(ADD_BOT)),
    ("DELETE", "/api/bots/{name}", Rule::Refuse(ADD_BOT)),
    ("PUT", "/api/bots/{name}/perps", Rule::Refuse(STRATEGY)),
    (
        "POST",
        "/api/bots/{name}/perps/recall/resume",
        Rule::Refuse(FUNDS),
    ),
    ("PATCH", "/api/bots/{name}/name", Rule::Refuse(LAYOUT)),
    ("POST", "/api/bots/{name}/recreate", Rule::Refuse(LAYOUT)),
    ("POST", "/api/bots/{name}/update", Rule::Refuse(TAGS)),
    ("GET", "/api/bots/{name}/versions", Rule::Refuse(TAGS)),
    ("POST", "/api/bots/{name}/rollback", Rule::Refuse(TAGS)),
    ("POST", "/api/bots/{name}/migrate", Rule::Refuse(LAYOUT)),
    ("PUT", "/api/bots/{name}/modules", Rule::Refuse(STRATEGY)),
    (
        "PUT",
        "/api/bots/{name}/modules/enabled",
        Rule::Refuse(STRATEGY),
    ),
    (
        "POST",
        "/api/bots/{name}/modules/manual-sales",
        Rule::Refuse(STRATEGY),
    ),
    (
        "POST",
        "/api/bots/{name}/modules/manual-sales/{id}/close",
        Rule::Refuse(STRATEGY),
    ),
    ("POST", "/api/bots/{name}/rfq/enroll", Rule::Refuse(SIGNER)),
    (
        "POST",
        "/api/bots/{name}/rfq/verify-email",
        Rule::Refuse(SIGNER),
    ),
    ("POST", "/api/bots/{name}/rfq/status", Rule::Refuse(SIGNER)),
    ("POST", "/api/bots/{name}/vault", Rule::Refuse(CHAIN)),
    ("DELETE", "/api/bots/{name}/vault", Rule::Refuse(CHAIN)),
    ("POST", "/api/bots/{name}/vault/check", Rule::Refuse(CHAIN)),
    ("GET", "/api/bots/{name}/config", Rule::Refuse(RAW)),
    ("PUT", "/api/bots/{name}/config", Rule::Refuse(RAW)),
    ("POST", "/api/bots/{name}/pools", Rule::Refuse(CHAIN)),
    (
        "DELETE",
        "/api/bots/{name}/pools/{index}",
        Rule::Refuse(CHAIN),
    ),
    ("POST", "/api/bots/{name}/approve", Rule::Refuse(FUNDS)),
    (
        "POST",
        "/api/bots/{name}/approve/custody",
        Rule::Refuse(FUNDS),
    ),
    ("POST", "/api/bots/{name}/withdraw", Rule::Refuse(FUNDS)),
    ("GET", "/api/desktop", Rule::Refuse(DESKTOP)),
    ("PATCH", "/api/desktop", Rule::Refuse(DESKTOP)),
    ("POST", "/api/bots/{name}/dry-run", Rule::Refuse(SIGNER)),
    (
        "POST",
        "/api/bots/{name}/identity/register",
        Rule::Refuse(IDENTITY),
    ),
    (
        "POST",
        "/api/bots/{name}/identity/register/custody",
        Rule::Refuse(IDENTITY),
    ),
    (
        "POST",
        "/api/bots/{name}/identity/link",
        Rule::Refuse(IDENTITY),
    ),
    ("GET", "/api/compose-export", Rule::Refuse(LAYOUT)),
    ("GET", "/api/updates", Rule::Refuse(SELF_UPDATE)),
    ("POST", "/api/panel/update", Rule::Refuse(SELF_UPDATE)),
];

/// What a hosted panel does with `method template`. Unlisted means refused.
pub fn rule(method: &Method, template: &str) -> Rule {
    ROUTES
        .iter()
        .find(|(m, p, _)| *m == method.as_str() && *p == template)
        .map(|(_, _, rule)| *rule)
        .unwrap_or(Rule::Refuse("this route isn't available on a hosted panel"))
}

/// Route layer for a hosted panel. Runs after routing, so the template is known.
pub async fn enforce(
    State(_state): State<AppState>,
    matched: Option<MatchedPath>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let template = matched.as_ref().map(MatchedPath::as_str).unwrap_or("");
    match rule(req.method(), template) {
        Rule::Allow => Ok(next.run(req).await),
        Rule::Refuse(reason) => {
            tracing::info!(method = %req.method(), route = template, "hosted policy refused a request");
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                format!("Not available on this hosted panel: {reason}."),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (m, p, _) in ROUTES {
            assert!(seen.insert((m, p)), "{m} {p} is listed twice");
        }
    }

    #[test]
    fn unlisted_routes_are_refused() {
        assert!(matches!(
            rule(&Method::POST, "/api/bots/{name}/exec"),
            Rule::Refuse(_)
        ));
        assert!(matches!(rule(&Method::GET, ""), Rule::Refuse(_)));
        assert!(matches!(
            rule(&Method::DELETE, "/api/bots"),
            Rule::Refuse(_)
        ));
    }

    /// Every `.route("…")` path in the router source is in the table, so a route
    /// added on dev shows up here as a failing test rather than silently refused
    /// on hosted (or, worse, silently allowed if the default ever changed).
    #[test]
    fn every_registered_route_is_classified() {
        let source = include_str!("../http/mod.rs");
        let paths: Vec<&str> = source
            .split(".route(")
            .skip(1)
            .filter_map(|rest| rest.trim_start().strip_prefix('"'))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        assert!(paths.len() > 40, "parsed only {} routes", paths.len());
        for path in paths {
            assert!(
                ROUTES.iter().any(|(_, p, _)| *p == path),
                "{path} is registered in http/mod.rs but missing from hosted::policy::ROUTES"
            );
        }
    }
}
