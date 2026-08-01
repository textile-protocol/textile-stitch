// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Who is allowed to drive the panel.
//!
//! Reaching the panel means reaching the Docker socket, which means owning the
//! host. Two ways in:
//!
//! - **Identity header.** `tailscale serve` sets `Tailscale-User-Login` on every
//!   proxied request. It is only an identity when that proxy is the sole thing
//!   that can reach the listener, which is a property of the deployment rather
//!   than of anything the panel can see — so it is believed only when both
//!   `STITCH_PANEL_TRUST_IDENTITY_HEADER` and `STITCH_PANEL_IDENTITY_PROXY_ONLY`
//!   are set, and ignored outright otherwise. See [`PanelConfig`](super::PanelConfig).
//! - **Password.** An argon2id hash in the environment, exchanged for a session
//!   cookie. For operators who don't front the panel with `tailscale serve`.
//!
//! Sessions live in memory. A panel restart logs everyone out, which is the right
//! trade for a single-process tool with no database.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use subtle::ConstantTimeEq;

use super::config::PanelConfig;

/// Name of the session cookie. `SameSite=Strict` on the way out means a browser
/// won't attach it to a cross-site request, which is what stands in for a CSRF
/// token here.
pub const SESSION_COOKIE: &str = "stitch_panel_session";

/// Header `tailscale serve` sets with the authenticated tailnet login.
pub const IDENTITY_HEADER: &str = "tailscale-user-login";

/// How long a password session lasts. Absolute, not sliding: an operator who
/// walked away from a laptop gets logged out on a predictable schedule.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Session tokens are 256 bits of OS randomness, which is far past guessable and
/// keeps the cookie short enough to read in a debugger.
const TOKEN_BYTES: usize = 32;

/// Cap on live sessions. A password login is cheap to request, and an unbounded
/// map would let an attacker who knows the password grow the panel's heap.
const MAX_SESSIONS: usize = 64;

/// How many password verifications may run at once.
///
/// Argon2id at the OWASP baseline is memory-hard on purpose: 19 MiB and two passes
/// per attempt. That's the point against an offline attacker and a liability against
/// an online one, because `/api/login` is the one route that answers before you have
/// a credential. Unbounded, a handful of parallel POSTs is hundreds of megabytes and
/// every blocking thread the runtime has — the Docker controls stop answering while
/// someone guesses passwords.
///
/// Two is enough for the humans this panel is for (one operator, maybe a colleague)
/// and small enough that saturating it costs ~38 MiB rather than the host.
pub const MAX_CONCURRENT_VERIFICATIONS: usize = 2;

/// Who the panel decided a request is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Authenticated by the reverse proxy, carrying the tailnet login.
    Tailnet(String),
    /// Authenticated by password, holding a session cookie.
    Password,
}

impl Identity {
    /// A short label for logs and the UI. Never contains secrets.
    pub fn label(&self) -> &str {
        match self {
            Identity::Tailnet(user) => user,
            Identity::Password => "password",
        }
    }
}

/// Why a request was turned away. Deliberately coarse: the client learns that it
/// is not authenticated, not which of the checks failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No usable credential at all.
    Missing,
    /// A credential was presented and rejected — wrong password, unknown tailnet
    /// login, expired session.
    Rejected,
}

impl AuthError {
    pub fn message(self) -> &'static str {
        match self {
            AuthError::Missing => "sign in to use the panel",
            AuthError::Rejected => "not authorized",
        }
    }
}

/// Hash a password for `STITCH_PANEL_PASSWORD_HASH`, using argon2id at the
/// crate's default parameters (19 MiB, 2 passes), which is the OWASP baseline.
pub fn hash_password(plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing the password failed: {e}"))?;
    Ok(hash.to_string())
}

/// Check a password against a stored PHC hash.
///
/// A malformed hash in the environment is a misconfiguration, not a login
/// failure, so it comes back as an error rather than a quiet `false` that would
/// look to the operator like a typo'd password.
/// Check a password on a blocking worker, under a hard concurrency cap.
///
/// Two things wrong with calling [`verify_password`] straight from a handler, and
/// this fixes both. It is synchronous and memory-hard, so it occupies an async worker
/// thread for its whole duration — `spawn_blocking` moves it to the pool meant for
/// that. And it has no ceiling, so parallel requests multiply the 19 MiB and the
/// threads — the permit gives it one.
///
/// Refuses rather than queues when the cap is reached. Waiting would let an attacker
/// build an unbounded backlog of pending logins, and an operator who is told "busy,
/// try again" is better served than one whose request hangs.
pub async fn verify_password_bounded(
    permits: &tokio::sync::Semaphore,
    stored: &str,
    plain: &str,
) -> Result<Option<bool>> {
    let Ok(_permit) = permits.try_acquire() else {
        return Ok(None);
    };
    let (stored, plain) = (stored.to_string(), plain.to_string());
    tokio::task::spawn_blocking(move || verify_password(&stored, &plain))
        .await
        .context("the password check didn't finish")?
        .map(Some)
}

pub fn verify_password(stored: &str, plain: &str) -> Result<bool> {
    let parsed = PasswordHash::new(stored)
        .map_err(|e| anyhow::anyhow!("STITCH_PANEL_PASSWORD_HASH is not a valid argon2 hash: {e}"))
        .context("the panel cannot check passwords until that is fixed")?;
    Ok(Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok())
}

/// In-memory session store.
#[derive(Debug, Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, Instant>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a session and return the token to set as a cookie.
    pub fn create(&self) -> String {
        let mut raw = [0u8; TOKEN_BYTES];
        OsRng.fill_bytes(&mut raw);
        let token = hex_lower(&raw);

        let mut live = self.lock();
        prune(&mut live, Instant::now());
        // Full store: drop the session closest to expiry rather than refusing the
        // login, so a legitimate operator is never locked out by stale entries.
        if live.len() >= MAX_SESSIONS {
            if let Some(oldest) = live
                .iter()
                .min_by_key(|(_, expiry)| **expiry)
                .map(|(k, _)| k.clone())
            {
                live.remove(&oldest);
            }
        }
        live.insert(token.clone(), Instant::now() + SESSION_TTL);
        token
    }

    /// True if the token names a live session.
    pub fn valid(&self, token: &str) -> bool {
        let now = Instant::now();
        let mut live = self.lock();
        prune(&mut live, now);
        // Compare against every candidate in constant time. A plain map lookup
        // would be fine for a random 256-bit token, but the cost here is a few
        // dozen byte comparisons and it removes the question entirely.
        live.keys()
            .any(|known| known.as_bytes().ct_eq(token.as_bytes()).into())
    }

    /// Drop a session on logout.
    pub fn revoke(&self, token: &str) {
        self.lock().remove(token);
    }

    /// Live session count, after pruning. Used by tests and the health endpoint.
    pub fn len(&self) -> usize {
        let mut live = self.lock();
        prune(&mut live, Instant::now());
        live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned lock means another thread panicked mid-update. The map is a set
    /// of opaque tokens with no cross-entry invariant, so recovering is safe and
    /// strictly better than taking the whole panel down.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn prune(live: &mut HashMap<String, Instant>, now: Instant) {
    live.retain(|_, expiry| *expiry > now);
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decide who a request is, from the headers the panel can see.
///
/// The identity header wins when it is trusted and allowlisted, because that is
/// the intended deployment. Otherwise the session cookie is tried. A presented
/// credential that fails is [`AuthError::Rejected`]; nothing presented at all is
/// [`AuthError::Missing`], which is what tells the UI to show a login form.
pub fn authorize(
    cfg: &PanelConfig,
    sessions: &Sessions,
    identity_header: Option<&str>,
    session_cookie: Option<&str>,
) -> Result<Identity, AuthError> {
    let mut saw_credential = false;

    if cfg.trust_identity_header {
        if let Some(raw) = identity_header.map(str::trim).filter(|h| !h.is_empty()) {
            saw_credential = true;
            let login = raw.to_lowercase();
            if cfg.auth.allowed_users().contains(&login) {
                return Ok(Identity::Tailnet(login));
            }
        }
    }

    if cfg.auth.password_hash().is_some() {
        if let Some(token) = session_cookie.filter(|t| !t.is_empty()) {
            saw_credential = true;
            if sessions.valid(token) {
                return Ok(Identity::Password);
            }
        }
    }

    Err(if saw_credential {
        AuthError::Rejected
    } else {
        AuthError::Missing
    })
}

/// Pull one cookie's value out of a `Cookie:` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// The `Set-Cookie` value for a new session.
///
/// `Secure` is omitted deliberately: the panel is reached over HTTP on loopback
/// and TLS is terminated by `tailscale serve` in front of it, so a `Secure`
/// cookie would never be stored. `HttpOnly` keeps it away from page scripts and
/// `SameSite=Strict` keeps it off cross-site requests.
pub fn session_cookie_header(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

/// The `Set-Cookie` value that clears the session on logout.
pub fn clear_cookie_header() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::config::AuthMode;

    fn cfg(auth: AuthMode, trust_identity_header: bool) -> PanelConfig {
        let mut cfg = PanelConfig::for_test("/data/bots", "/data/bots");
        cfg.auth = auth;
        cfg.trust_identity_header = trust_identity_header;
        cfg
    }

    fn tailnet(users: &[&str]) -> AuthMode {
        AuthMode::Tailnet {
            allowed_users: users.iter().map(|u| u.to_string()).collect(),
        }
    }

    #[test]
    fn a_password_round_trips_through_its_hash() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "correct horse battery staple").unwrap());
        assert!(!verify_password(&hash, "Correct horse battery staple").unwrap());
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        // Distinct salts; otherwise two operators picking the same password would
        // be visible to anyone who reads the environment.
        let a = hash_password("hunter2").unwrap();
        let b = hash_password("hunter2").unwrap();
        assert_ne!(a, b);
        assert!(verify_password(&a, "hunter2").unwrap());
        assert!(verify_password(&b, "hunter2").unwrap());
    }

    #[test]
    fn a_malformed_stored_hash_is_an_error_not_a_failed_login() {
        // Told plainly, the operator fixes the env var. Reported as a bad
        // password, they'd spend the afternoon retyping it.
        let err = verify_password("not-a-phc-string", "whatever").unwrap_err();
        assert!(format!("{err:#}").contains("STITCH_PANEL_PASSWORD_HASH"));
    }

    #[test]
    fn sessions_are_created_validated_and_revoked() {
        let sessions = Sessions::new();
        let token = sessions.create();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert!(sessions.valid(&token));
        assert_eq!(sessions.len(), 1);

        sessions.revoke(&token);
        assert!(!sessions.valid(&token));
        assert!(sessions.is_empty());
    }

    #[test]
    fn tokens_are_unique_and_unrelated() {
        let sessions = Sessions::new();
        let a = sessions.create();
        let b = sessions.create();
        assert_ne!(a, b);
        assert!(sessions.valid(&a) && sessions.valid(&b));
        // A near-miss token is not a session.
        let mut tampered = a.clone();
        tampered.pop();
        tampered.push(if a.ends_with('0') { '1' } else { '0' });
        assert!(!sessions.valid(&tampered));
    }

    #[test]
    fn an_expired_session_stops_working() {
        let sessions = Sessions::new();
        let token = sessions.create();
        // Reach in and backdate the expiry rather than sleeping for 12 hours.
        sessions
            .lock()
            .insert(token.clone(), Instant::now() - Duration::from_secs(1));
        assert!(!sessions.valid(&token));
        assert!(sessions.is_empty());
    }

    #[test]
    fn the_session_store_is_bounded() {
        let sessions = Sessions::new();
        let tokens: Vec<_> = (0..MAX_SESSIONS + 10).map(|_| sessions.create()).collect();
        assert!(sessions.len() <= MAX_SESSIONS);
        // The most recent login always survives; that's the operator at the keyboard.
        assert!(sessions.valid(tokens.last().unwrap()));
    }

    #[test]
    fn an_allowlisted_identity_header_authorizes() {
        let cfg = cfg(tailnet(&["alice@example.com"]), true);
        let id = authorize(&cfg, &Sessions::new(), Some("Alice@Example.com"), None).unwrap();
        assert_eq!(id, Identity::Tailnet("alice@example.com".into()));
    }

    #[test]
    fn an_unlisted_identity_header_is_rejected() {
        let cfg = cfg(tailnet(&["alice@example.com"]), true);
        let err = authorize(&cfg, &Sessions::new(), Some("bob@example.com"), None).unwrap_err();
        assert_eq!(err, AuthError::Rejected);
    }

    #[test]
    fn an_untrusted_identity_header_is_ignored_entirely() {
        // Without the explicit trust flag the header is just a client-supplied
        // string, so honouring it would hand the Docker socket to anyone who can
        // reach the listener and spell the operator's email — including any local
        // process when the panel runs in the host's network namespace. It must not
        // even count as a presented credential.
        let cfg = cfg(tailnet(&["alice@example.com"]), false);
        let err = authorize(&cfg, &Sessions::new(), Some("alice@example.com"), None).unwrap_err();
        assert_eq!(err, AuthError::Missing);
    }

    #[test]
    fn a_session_cookie_authorizes_in_password_mode() {
        let cfg = cfg(
            AuthMode::Password {
                hash: "$argon2id$x".into(),
            },
            true,
        );
        let sessions = Sessions::new();
        let token = sessions.create();
        assert_eq!(
            authorize(&cfg, &sessions, None, Some(&token)).unwrap(),
            Identity::Password
        );
        assert_eq!(
            authorize(&cfg, &sessions, None, Some("deadbeef")).unwrap_err(),
            AuthError::Rejected
        );
        assert_eq!(
            authorize(&cfg, &sessions, None, None).unwrap_err(),
            AuthError::Missing
        );
    }

    #[test]
    fn a_cookie_is_ignored_when_passwords_are_not_configured() {
        // Tailnet-only mode has no password to check, so a forged cookie must not
        // be treated as a credential at all.
        let cfg = cfg(tailnet(&["alice@example.com"]), true);
        let sessions = Sessions::new();
        let token = sessions.create();
        assert_eq!(
            authorize(&cfg, &sessions, None, Some(&token)).unwrap_err(),
            AuthError::Missing
        );
    }

    #[test]
    fn either_mode_takes_whichever_arrives() {
        let cfg = cfg(
            AuthMode::Either {
                allowed_users: vec!["alice@example.com".into()],
                hash: "$argon2id$x".into(),
            },
            true,
        );
        let sessions = Sessions::new();
        let token = sessions.create();
        assert!(matches!(
            authorize(&cfg, &sessions, Some("alice@example.com"), None),
            Ok(Identity::Tailnet(_))
        ));
        assert_eq!(
            authorize(&cfg, &sessions, None, Some(&token)).unwrap(),
            Identity::Password
        );
        // An unlisted login still falls through to the cookie.
        assert_eq!(
            authorize(&cfg, &sessions, Some("bob@example.com"), Some(&token)).unwrap(),
            Identity::Password
        );
    }

    #[test]
    fn cookies_are_parsed_out_of_a_shared_header() {
        let header = "other=1; stitch_panel_session=abc123; last=2";
        assert_eq!(
            cookie_value(header, SESSION_COOKIE).as_deref(),
            Some("abc123")
        );
        assert_eq!(cookie_value(header, "missing"), None);
        // A prefix match must not be mistaken for the real cookie.
        assert_eq!(
            cookie_value("xstitch_panel_session=no", SESSION_COOKIE),
            None
        );
    }

    #[test]
    fn the_session_cookie_is_locked_down() {
        let set = session_cookie_header("abc");
        assert!(set.contains("HttpOnly"));
        assert!(set.contains("SameSite=Strict"));
        assert!(set.contains("Path=/"));
        assert!(clear_cookie_header().contains("Max-Age=0"));
    }
}
