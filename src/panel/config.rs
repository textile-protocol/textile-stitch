// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Panel configuration, read from the environment at startup.
//!
//! The panel holds market-maker keys and a Docker socket, so the defaults here
//! are deliberately the safe ones: loopback bind, no auth bypass, and a hard
//! refusal to start on a routable address. Everything an operator would want to
//! relax has to be set explicitly.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Default bots root inside the panel container. The host directory holding the
/// per-bot config folders is bind-mounted here.
pub const DEFAULT_BOTS_DIR: &str = "/data/bots";

/// Default Docker socket path.
#[cfg(unix)]
pub const DEFAULT_DOCKER_SOCKET: &str = "/var/run/docker.sock";
#[cfg(windows)]
pub const DEFAULT_DOCKER_SOCKET: &str = r"\\.\pipe\docker_engine";
#[cfg(not(any(unix, windows)))]
pub const DEFAULT_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// Default listen address. Loopback because the intended deployment puts the
/// panel in a Tailscale sidecar's network namespace and lets `tailscale serve`
/// terminate TLS in front of it.
pub const DEFAULT_BIND: &str = "127.0.0.1:8420";

/// Default bot image. Panel-created containers pin whatever this resolves to at
/// creation time; operators should point it at a `sha-*` tag in production.
pub const DEFAULT_BOT_IMAGE: &str = "ghcr.io/textile-protocol/textile-stitch:latest";

/// The uid and gid the bot image's `stitch` user has, pinned in its Dockerfile.
/// Overridable for anyone running a custom image built with a different user.
pub const DEFAULT_BOT_UID: u32 = 1000;

/// How the panel supervises bots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelRuntime {
    /// Docker Engine API (server installs, `install-panel.sh`).
    Docker,
    /// Local `stitch` child processes (desktop tray app). No Docker required.
    Process,
}

impl PanelRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            PanelRuntime::Docker => "docker",
            PanelRuntime::Process => "process",
        }
    }
}

/// How a request proves it's an authorized operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// Trust `Tailscale-User-Login` from the reverse proxy in front of us, and
    /// require the value to be in the allowlist. Only sound when the panel is
    /// bound to loopback inside the tailscale namespace, which [`PanelConfig`]
    /// enforces.
    Tailnet { allowed_users: Vec<String> },
    /// A single shared password, stored as an argon2 PHC hash. For setups that
    /// don't front the panel with `tailscale serve`.
    Password { hash: String },
    /// Both accepted: an identity header from the proxy, or a password login.
    Either {
        allowed_users: Vec<String>,
        hash: String,
    },
}

impl AuthMode {
    /// The tailnet allowlist, if this mode accepts identity headers.
    pub fn allowed_users(&self) -> &[String] {
        match self {
            AuthMode::Tailnet { allowed_users } | AuthMode::Either { allowed_users, .. } => {
                allowed_users
            }
            AuthMode::Password { .. } => &[],
        }
    }

    /// The password hash, if this mode accepts password logins.
    pub fn password_hash(&self) -> Option<&str> {
        match self {
            AuthMode::Password { hash } | AuthMode::Either { hash, .. } => Some(hash),
            AuthMode::Tailnet { .. } => None,
        }
    }
}

/// Everything the panel needs to run.
#[derive(Debug, Clone)]
pub struct PanelConfig {
    /// Where per-bot config directories live, as the panel sees them.
    pub bots_dir: PathBuf,
    /// Where those same directories live on the Docker host. Bind mounts are
    /// resolved by the daemon, not by us, so a path that is correct inside the
    /// panel container is wrong in a mount spec unless we translate it.
    ///
    /// In process runtime this equals [`Self::bots_dir`] — there is no host/container
    /// split.
    pub host_bots_dir: PathBuf,
    /// Unix socket path for the Docker Engine API. Ignored in process runtime.
    pub docker_socket: PathBuf,
    pub bind: SocketAddr,
    pub auth: AuthMode,
    /// Whether `Tailscale-User-Login` on an incoming request may be believed.
    ///
    /// Set only when both `STITCH_PANEL_TRUST_IDENTITY_HEADER` and
    /// `STITCH_PANEL_IDENTITY_PROXY_ONLY` are opted in — see
    /// [`trust_identity_header`]. Off means the header isn't even read, because a
    /// client-supplied one would let anyone who can reach the listener and knows
    /// an operator's email drive the Docker socket.
    pub trust_identity_header: bool,
    /// Image used for bots the panel creates (Docker runtime). Process runtime
    /// ignores this and runs the local `stitch` binary.
    pub bot_image: String,
    /// The uid the bot image runs as, pinned to 1000 in its Dockerfile.
    ///
    /// The panel usually runs as root — it needs the Docker socket — so anything
    /// it writes into a bot directory lands root-owned. The bot runs as `stitch`
    /// and its entrypoint locks the run directory down with `chmod 700`, which a
    /// non-owner can't do, so it exits before reading a line of config. Every
    /// directory the panel hands to a bot is chowned to this uid.
    ///
    /// Process runtime defaults this to the current user so chown is a no-op.
    pub bot_uid: u32,
    /// Docker Engine vs local process supervision.
    pub runtime: PanelRuntime,
    /// Textile API origin the wizard reads the corridor list from.
    ///
    /// `None` turns the fetch off and pins the panel to the corridors compiled
    /// into this binary — set `STITCH_PANEL_CORRIDOR_API=off` for a panel that
    /// must never call home. Tests default to `None` so no suite depends on the
    /// network.
    pub corridor_api_url: Option<String>,
}

impl PanelConfig {
    /// Read the config from the process environment, validating as we go so a
    /// misconfigured panel fails at startup rather than on first request.
    pub fn from_env() -> Result<Self> {
        let runtime = runtime_from_env()?;
        let bots_dir = path_var("STITCH_PANEL_BOTS_DIR", DEFAULT_BOTS_DIR);
        // Defaults to the in-container path, which is correct when the panel runs
        // directly on the host. Containerised panels must set this to the host
        // path they bind-mounted, or every bot mount would point at a path the
        // daemon can't see.
        let host_bots_dir = std::env::var_os("STITCH_PANEL_HOST_BOTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| bots_dir.clone());

        let bind: SocketAddr = string_var("STITCH_PANEL_BIND", DEFAULT_BIND)
            .parse()
            .context("STITCH_PANEL_BIND must be an address like 127.0.0.1:8420")?;

        let insecure = allow_insecure_bind();
        check_bind_address(bind.ip(), insecure)?;

        let auth = auth_from_env()?;
        let trust_identity_header = trust_identity_header()?;
        if !trust_identity_header && auth.password_hash().is_none() {
            bail!(
                "the panel is configured to authenticate by tailnet login, but nothing has \
                 told it the `Tailscale-User-Login` header is trustworthy, so every request \
                 would be rejected. That header is only an identity if an authenticated \
                 proxy is the sole thing that can reach the listener — bind alone doesn't \
                 prove it, because a panel running on the host shares the host's loopback \
                 with every local process. Run it the way docker-compose.panel.yml does \
                 (in the Tailscale sidecar's network namespace) and set both \
                 STITCH_PANEL_TRUST_IDENTITY_HEADER=1 and \
                 STITCH_PANEL_IDENTITY_PROXY_ONLY=1, or set STITCH_PANEL_PASSWORD_HASH \
                 and log in with a password instead."
            );
        }

        let default_uid = match runtime {
            PanelRuntime::Process => current_uid_runtime(),
            PanelRuntime::Docker => DEFAULT_BOT_UID,
        };

        Ok(Self {
            bots_dir,
            host_bots_dir,
            docker_socket: path_var("STITCH_PANEL_DOCKER_SOCKET", DEFAULT_DOCKER_SOCKET),
            bind,
            auth,
            trust_identity_header,
            bot_image: string_var("STITCH_PANEL_BOT_IMAGE", DEFAULT_BOT_IMAGE),
            bot_uid: uid_var("STITCH_PANEL_BOT_UID", default_uid)?,
            runtime,
            corridor_api_url: corridor_api_from_env()?,
        })
    }

    /// The per-bot config directory as the panel sees it.
    pub fn bot_dir(&self, id: &str) -> PathBuf {
        self.bots_dir.join(id)
    }

    /// The per-bot config directory as the Docker daemon sees it, for mount specs.
    pub fn host_bot_dir(&self, id: &str) -> PathBuf {
        self.host_bots_dir.join(id)
    }

    /// Translate a host path reported by the Docker API back into a path the
    /// panel can read. The inverse of [`Self::host_bot_dir`], used when adopting
    /// containers whose mounts were written by someone else.
    pub fn to_panel_path(&self, host_path: &Path) -> PathBuf {
        match host_path.strip_prefix(&self.host_bots_dir) {
            Ok(rest) => self.bots_dir.join(rest),
            // Outside the bots root: an adopted bot whose config lives elsewhere.
            // Return it unchanged so the caller can report it honestly rather
            // than silently reading the wrong file.
            Err(_) => host_path.to_path_buf(),
        }
    }
}

#[cfg(test)]
impl PanelConfig {
    /// A config for tests: private bind, password auth, and the two directory
    /// roots the caller cares about. Tests that assert on anything else override
    /// the field directly, so this stays the one place the shape is spelled out.
    ///
    /// `bot_uid` is the uid running the tests rather than the production 1000, so
    /// handing a directory to "the bot" is a no-op instead of a chown only root
    /// could perform.
    pub(crate) fn for_test(
        bots_dir: impl Into<PathBuf>,
        host_bots_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            bots_dir: bots_dir.into(),
            host_bots_dir: host_bots_dir.into(),
            docker_socket: PathBuf::from(DEFAULT_DOCKER_SOCKET),
            bind: DEFAULT_BIND.parse().expect("the default bind must parse"),
            auth: AuthMode::Password {
                hash: "$argon2id$test".into(),
            },
            trust_identity_header: true,
            bot_image: DEFAULT_BOT_IMAGE.to_string(),
            bot_uid: current_uid(),
            runtime: PanelRuntime::Docker,
            // No suite may depend on Textile being reachable. Tests that want
            // the remote catalog point this at their own mock server.
            corridor_api_url: None,
        }
    }
}

/// The corridor-list origin: Textile's API by default, an operator-chosen origin
/// when set, or nothing at all for `off` / `none` / an empty value.
///
/// Validated here rather than at first use so a typo fails at startup with a
/// clear message, instead of silently degrading every wizard load to the
/// built-in list minutes later.
fn corridor_api_from_env() -> Result<Option<String>> {
    let raw = match std::env::var("STITCH_PANEL_CORRIDOR_API") {
        Err(_) => return Ok(Some(crate::setup::DEFAULT_CORRIDOR_API.to_string())),
        Ok(v) => v.trim().to_string(),
    };
    if raw.is_empty() || raw.eq_ignore_ascii_case("off") || raw.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let parsed = url::Url::parse(&raw)
        .with_context(|| format!("STITCH_PANEL_CORRIDOR_API must be a URL, not {raw:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("STITCH_PANEL_CORRIDOR_API must be an http(s) URL, not {raw:?}");
    }
    Ok(Some(raw))
}

fn runtime_from_env() -> Result<PanelRuntime> {
    match std::env::var("STITCH_PANEL_RUNTIME") {
        Err(_) => Ok(PanelRuntime::Docker),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" | "docker" => Ok(PanelRuntime::Docker),
            "process" | "native" | "desktop" => Ok(PanelRuntime::Process),
            other => bail!("STITCH_PANEL_RUNTIME must be \"docker\" or \"process\", not {other:?}"),
        },
    }
}

/// The uid the current process runs as (desktop process runtime + tests).
#[cfg(unix)]
pub(crate) fn current_uid() -> u32 {
    // Safe: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
pub(crate) fn current_uid() -> u32 {
    DEFAULT_BOT_UID
}

fn current_uid_runtime() -> u32 {
    current_uid()
}

/// Reject a listen address that would expose the panel beyond the operator's
/// private network. Loopback is the intended deployment; a Tailscale address is
/// accepted for operators who bind the panel directly to `tailscale0` instead of
/// sharing the sidecar's namespace.
///
/// This is the only thing standing between a misconfigured `STITCH_PANEL_BIND`
/// and a key-holding, Docker-socket-wielding UI on the public internet, so it
/// fails closed and the override is explicit.
pub fn check_bind_address(ip: IpAddr, allow_insecure: bool) -> Result<()> {
    if allow_insecure || ip.is_loopback() || is_tailnet_addr(ip) {
        return Ok(());
    }
    bail!(
        "refusing to bind the panel to {ip}: it is neither loopback nor a Tailscale address. \
         The panel exposes the Docker socket and operator keys, so it must sit behind \
         `tailscale serve` (bind 127.0.0.1) or on a tailnet address. \
         Set STITCH_PANEL_ALLOW_INSECURE_BIND=1 only if you have your own authenticated proxy."
    )
}

/// True for addresses in the ranges Tailscale assigns: `100.64.0.0/10` (the
/// CGNAT range) for IPv4 and `fd7a:115c:a1e0::/48` for IPv6.
fn is_tailnet_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

fn allow_insecure_bind() -> bool {
    flag("STITCH_PANEL_ALLOW_INSECURE_BIND")
}

/// Whether `Tailscale-User-Login` can be believed.
///
/// Never implicitly. The header is only as good as the guarantee that nothing but
/// the proxy can reach the listener, and that guarantee lives in the deployment,
/// not in anything the panel can observe:
///
/// - In the shipped sidecar layout the panel shares the Tailscale container's
///   network namespace, so its loopback is that namespace's loopback and the only
///   peer on it is `tailscale serve`, which sets the header itself.
/// - Run the same binary directly on the host and its loopback is the *host's*
///   loopback, which every local user and every container with
///   `network_mode: host` can dial. Any unprivileged process could then send
///   `Tailscale-User-Login: <an allowlisted login>` and drive the Docker socket —
///   root on the machine, from an account that had none.
///
/// The bind address can't tell those two apart, so it isn't asked. Trust needs
/// two explicit opt-ins:
///
/// 1. `STITCH_PANEL_TRUST_IDENTITY_HEADER=1` — believe the header at all.
/// 2. `STITCH_PANEL_IDENTITY_PROXY_ONLY=1` — operator attestation that an
///    authenticated reverse proxy is the *sole* peer on the listener (sets the
///    header itself and strips any client-supplied value).
///
/// `docker-compose.panel.yml` sets both for the sidecar layout where that
/// attestation holds. Host installs must use password auth instead.
fn trust_identity_header() -> Result<bool> {
    resolve_trust_identity_header(
        flag("STITCH_PANEL_TRUST_IDENTITY_HEADER"),
        flag("STITCH_PANEL_IDENTITY_PROXY_ONLY"),
    )
}

/// Pure form of [`trust_identity_header`] for tests and the env reader.
pub(crate) fn resolve_trust_identity_header(
    trust: bool,
    proxy_only_attestation: bool,
) -> Result<bool> {
    if !trust {
        return Ok(false);
    }
    if !proxy_only_attestation {
        bail!(
            "STITCH_PANEL_TRUST_IDENTITY_HEADER=1 is set, but \
             STITCH_PANEL_IDENTITY_PROXY_ONLY is not. Believing \
             `Tailscale-User-Login` without an authenticated proxy as the sole \
             peer on the listener hands the Docker socket to anyone who can \
             reach the panel and spell an allowlisted login — including every \
             local process when the panel shares the host's loopback. \
             Set STITCH_PANEL_IDENTITY_PROXY_ONLY=1 only for the shipped \
             Tailscale sidecar layout (`network_mode: service:tailscale`) or an \
             equivalent proxy-only deployment, or drop TRUST_IDENTITY_HEADER and \
             use STITCH_PANEL_PASSWORD_HASH instead."
        );
    }
    Ok(true)
}

fn flag(key: &str) -> bool {
    is_opt_in(std::env::var(key).ok().as_deref())
}

/// Whether an environment value is an explicit opt-in. Only exactly `1` or `true`
/// count: these flags relax a control on the Docker socket, so anything else —
/// including `yes`, `TRUE`, or a stray space — has to fail closed.
fn is_opt_in(raw: Option<&str>) -> bool {
    matches!(raw, Some("1") | Some("true"))
}

/// Build the auth mode from the environment. At least one method is required —
/// there is no anonymous mode, because reaching the panel means owning the host.
fn auth_from_env() -> Result<AuthMode> {
    let users = parse_user_list(&string_var("STITCH_PANEL_TAILNET_USERS", ""));
    let hash = std::env::var("STITCH_PANEL_PASSWORD_HASH")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());

    match (users.is_empty(), hash) {
        (false, Some(hash)) => Ok(AuthMode::Either {
            allowed_users: users,
            hash,
        }),
        (false, None) => Ok(AuthMode::Tailnet {
            allowed_users: users,
        }),
        (true, Some(hash)) => Ok(AuthMode::Password { hash }),
        (true, None) => bail!(
            "the panel needs an auth method: set STITCH_PANEL_TAILNET_USERS to a \
             comma-separated allowlist of tailnet logins, or STITCH_PANEL_PASSWORD_HASH \
             to an argon2 hash (generate one with `stitch-panel hash-password`)."
        ),
    }
}

/// Split and normalise a comma-separated login allowlist. Logins are compared
/// case-insensitively because they're email-shaped and the proxy's casing isn't
/// guaranteed to match what the operator typed.
fn parse_user_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A uid from the environment. Rejected rather than defaulted when it isn't a
/// number, because silently using 1000 would produce a bot that can't read its
/// own key on a host where the image was built with a different user.
fn uid_var(key: &str, default: u32) -> Result<u32> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .with_context(|| format!("{key} must be a uid like {default}, got \"{raw}\"")),
    }
}

fn string_var(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn path_var(key: &str, default: &str) -> PathBuf {
    PathBuf::from(string_var(key, default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_tailnet_binds_are_accepted() {
        for ip in ["127.0.0.1", "::1", "100.100.42.7", "fd7a:115c:a1e0::1"] {
            check_bind_address(ip.parse().unwrap(), false)
                .unwrap_or_else(|e| panic!("{ip} should be allowed: {e}"));
        }
    }

    #[test]
    fn routable_binds_are_refused_unless_overridden() {
        // 0.0.0.0 is the dangerous default people reach for in Docker; it must
        // not start a key-holding panel.
        for ip in ["0.0.0.0", "192.168.1.10", "1.2.3.4", "10.0.0.5"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                check_bind_address(ip, false).is_err(),
                "{ip} must be refused"
            );
            assert!(
                check_bind_address(ip, true).is_ok(),
                "{ip} should pass with the explicit override"
            );
        }
    }

    #[test]
    fn tailnet_range_boundaries_are_exact() {
        // 100.64.0.0/10 is 100.64.x.x through 100.127.x.x. Neighbours outside it
        // are ordinary routable addresses and must not be mistaken for a tailnet.
        assert!(is_tailnet_addr("100.64.0.0".parse().unwrap()));
        assert!(is_tailnet_addr("100.127.255.255".parse().unwrap()));
        assert!(!is_tailnet_addr("100.63.255.255".parse().unwrap()));
        assert!(!is_tailnet_addr("100.128.0.0".parse().unwrap()));
        // A near-miss IPv6 prefix is not a tailnet either.
        assert!(!is_tailnet_addr("fd7a:115c:a1e1::1".parse().unwrap()));
    }

    #[test]
    fn user_list_is_normalised_and_blank_entries_dropped() {
        let users = parse_user_list(" Alice@example.com , ,bob@example.com,");
        assert_eq!(users, vec!["alice@example.com", "bob@example.com"]);
        assert!(parse_user_list("  ,  ").is_empty());
    }

    #[test]
    fn auth_mode_exposes_only_what_it_accepts() {
        let tailnet = AuthMode::Tailnet {
            allowed_users: vec!["a@b.c".into()],
        };
        assert_eq!(tailnet.allowed_users().len(), 1);
        assert!(tailnet.password_hash().is_none());

        let password = AuthMode::Password {
            hash: "$argon2id$…".into(),
        };
        assert!(password.allowed_users().is_empty());
        assert!(password.password_hash().is_some());

        let either = AuthMode::Either {
            allowed_users: vec!["a@b.c".into()],
            hash: "$argon2id$…".into(),
        };
        assert_eq!(either.allowed_users().len(), 1);
        assert!(either.password_hash().is_some());
    }

    fn cfg_with_roots(panel: &str, host: &str) -> PanelConfig {
        PanelConfig::for_test(panel, host)
    }

    #[test]
    fn a_bind_address_never_grants_trust_by_itself() {
        // Loopback and tailnet addresses may both listen, and neither says
        // anything about who can reach the listener: a panel in the host's network
        // namespace shares 127.0.0.1 with every local process, and a tailnet bind
        // is dialled directly by every peer with no proxy in between to overwrite
        // the header. Trust comes from the two explicit identity-header flags,
        // which is why `trust_identity_header` takes no bind-address arguments.
        for addr in ["127.0.0.1", "::1", "100.101.102.103", "fd7a:115c:a1e0::1"] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(check_bind_address(ip, false).is_ok(), "{addr} should bind");
        }
    }

    #[test]
    fn trusting_the_identity_header_requires_proxy_only_attestation() {
        // TRUST alone used to be enough, which is the host-loopback footgun:
        // any local process could spoof Tailscale-User-Login. The second flag is
        // the operator saying "an authenticated proxy is the sole peer".
        assert!(!resolve_trust_identity_header(false, false).unwrap());
        assert!(!resolve_trust_identity_header(false, true).unwrap());
        let err = resolve_trust_identity_header(true, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("STITCH_PANEL_IDENTITY_PROXY_ONLY"),
            "{err:#}"
        );
        assert!(resolve_trust_identity_header(true, true).unwrap());
    }

    #[test]
    fn only_an_explicit_value_counts_as_an_opt_in() {
        // These flags relax a control on the Docker socket, so a typo — or a
        // truthy-looking value from a templating tool — has to fail closed.
        assert!(is_opt_in(Some("1")));
        assert!(is_opt_in(Some("true")));
        for raw in ["", "0", "false", "yes", "TRUE", " 1"] {
            assert!(!is_opt_in(Some(raw)), "{raw:?} must not opt in");
        }
        assert!(!is_opt_in(None));
    }

    #[test]
    fn bot_dirs_translate_between_panel_and_host_views() {
        let cfg = cfg_with_roots("/data/bots", "/home/ec2-user/stitch");
        assert_eq!(cfg.bot_dir("bot-a"), PathBuf::from("/data/bots/bot-a"));
        assert_eq!(
            cfg.host_bot_dir("bot-a"),
            PathBuf::from("/home/ec2-user/stitch/bot-a")
        );
        // A host path under the root maps back to something the panel can read.
        assert_eq!(
            cfg.to_panel_path(Path::new("/home/ec2-user/stitch/bot-a/stitch.toml")),
            PathBuf::from("/data/bots/bot-a/stitch.toml")
        );
    }

    #[test]
    fn a_host_path_outside_the_root_is_returned_unchanged() {
        // An adopted bot can keep its config anywhere. Rewriting such a path
        // would point us at a file that doesn't exist, so it passes through and
        // the caller reports the bot as unreadable instead.
        let cfg = cfg_with_roots("/data/bots", "/home/ec2-user/stitch");
        let outside = Path::new("/srv/other/stitch.toml");
        assert_eq!(cfg.to_panel_path(outside), outside);
    }
}
