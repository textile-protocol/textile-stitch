// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! `stitch-panel` — the process behind the Stitch web UI.
//!
//! One process per host. It manages bots by writing config files and driving
//! either the Docker Engine API or local `stitch` child processes
//! (`STITCH_PANEL_RUNTIME=process`); there is no database, because the
//! container/process list plus the config directories already are the state.
//!
//! Reaching a Docker-backed panel means reaching the Docker socket. The desktop
//! process runtime has no socket — it only supervises local bots. Either way the
//! panel binds to loopback by default, refuses a routable address without an
//! explicit override, and requires a credential on every API route. See
//! [`stitch_bot::panel::config`] for the environment it reads.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result};
use stitch_bot::panel::docker::{BollardDocker, ProcessRuntime};
use stitch_bot::panel::http::{router, AppState};
use stitch_bot::panel::{auth, AuthMode, PanelConfig, PanelRuntime};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("hash-password") => hash_password(),
        Some("--version" | "-V") => {
            println!("stitch-panel {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("stitch-panel: unknown command \"{other}\"\n\n{USAGE}");
            std::process::exit(2);
        }
        None => serve(),
    }
}

const USAGE: &str = "\
stitch-panel — Stitch web UI for a fleet of bots

USAGE:
    stitch-panel                 run Stitch
    stitch-panel hash-password   read a password from the terminal and print its
                                 argon2 hash, for STITCH_PANEL_PASSWORD_HASH.
                                 Reads stdin instead when piped, for scripts.

The panel is configured entirely from the environment:

    STITCH_PANEL_RUNTIME            docker (default) or process (desktop, no Docker)
    STITCH_PANEL_BIND               listen address        (default 127.0.0.1:8420)
    STITCH_PANEL_BOTS_DIR           config root as the panel sees it   (/data/bots)
    STITCH_PANEL_HOST_BOTS_DIR      the same root on the Docker host
    STITCH_PANEL_DOCKER_SOCKET      Docker socket         (/var/run/docker.sock)
    STITCH_PANEL_STITCH_BIN         path to the stitch binary (process runtime)
    STITCH_PANEL_BOT_IMAGE          image new bots run (docker runtime)
    STITCH_PANEL_TAILNET_USERS      comma-separated tailnet login allowlist
    STITCH_PANEL_PASSWORD_HASH      argon2 hash for password login
    STITCH_PANEL_ALLOW_INSECURE_BIND=1   permit a routable bind (you own the risk)
    STITCH_PANEL_TRUST_IDENTITY_HEADER=1 believe Tailscale-User-Login. Required for
                                         tailnet-login auth; also requires
                                         STITCH_PANEL_IDENTITY_PROXY_ONLY=1.
    STITCH_PANEL_IDENTITY_PROXY_ONLY=1   attest that an authenticated reverse proxy
                                         is the sole peer on the listener (sets and
                                         strips the identity header). Required with
                                         TRUST_IDENTITY_HEADER; set by the shipped
                                         Tailscale sidecar compose file.
";

/// Print an argon2 hash for a password typed at the terminal.
///
/// Read with no echo and never taken as an argument, so the password doesn't end
/// up in shell history or in another user's `ps` output.
fn hash_password() -> Result<()> {
    let password = read_password()?;
    anyhow::ensure!(
        password.chars().count() >= 12,
        "use at least 12 characters: this password guards the panel (and the Docker socket when using the docker runtime)"
    );
    println!("{}", auth::hash_password(&password)?);
    // Tip is for humans at a terminal. install-panel.sh pipes on stdin and
    // keeps stderr for real failures — don't pollute it with a success note.
    if std::io::stdin().is_terminal() {
        eprintln!("\nSet it as STITCH_PANEL_PASSWORD_HASH (quote it — it contains $).");
    }
    Ok(())
}

/// Get the password from the terminal with echo off, asking twice, or from stdin
/// when stdin is a pipe.
///
/// The piped path is what makes this usable from a provisioning script or a
/// `docker run -i` one-liner, where there is no terminal to prompt on. It asks
/// only once, because the caller already has the value in hand.
fn read_password() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the password from stdin")?;
        return Ok(strip_line_ending(&line).to_string());
    }
    let password = rpassword::prompt_password("Panel password: ")
        .context("reading the password from the terminal")?;
    let again = rpassword::prompt_password("Again: ").context("reading the confirmation")?;
    anyhow::ensure!(password == again, "those didn't match");
    Ok(password)
}

/// Drop the line terminator `read_line` keeps, and nothing else: trailing spaces
/// are legitimate password characters.
fn strip_line_ending(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .unwrap_or(line)
}

fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,stitch_bot=info".into()),
        )
        .init();

    let cfg = PanelConfig::from_env()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?
        .block_on(run(cfg))
}

async fn run(cfg: PanelConfig) -> Result<()> {
    std::fs::create_dir_all(&cfg.bots_dir)
        .with_context(|| format!("creating the bots root at {}", cfg.bots_dir.display()))?;

    let bind = cfg.bind;
    let runtime = cfg.runtime;
    let auth_summary = describe_auth(&cfg.auth, cfg.trust_identity_header);

    let state = match runtime {
        PanelRuntime::Docker => {
            let docker = Arc::new(BollardDocker::connect(&cfg.docker_socket)?);
            // Fail at startup rather than on the operator's first click: a panel that
            // can't reach the daemon can't do anything useful, and the cause is almost
            // always a missing socket mount.
            docker.ping().await.with_context(|| {
                format!(
                    "couldn't talk to the Docker daemon at {}. Mount the socket into the panel \
                     container (-v /var/run/docker.sock:/var/run/docker.sock) or point \
                     STITCH_PANEL_DOCKER_SOCKET at the right path.",
                    cfg.docker_socket.display()
                )
            })?;
            AppState::new(cfg, docker.clone()).with_container_files(docker)
        }
        PanelRuntime::Process => {
            let stitch_bin = ProcessRuntime::find_stitch_binary().with_context(|| {
                "STITCH_PANEL_RUNTIME=process but no stitch binary was found. Set \
                 STITCH_PANEL_STITCH_BIN or place `stitch` next to stitch-panel."
            })?;
            let docker = Arc::new(ProcessRuntime::new(stitch_bin.clone(), &cfg.bots_dir)?);
            tracing::info!(
                stitch_bin = %stitch_bin.display(),
                "process runtime: supervising local stitch binaries (no Docker)"
            );
            AppState::new(cfg, docker)
        }
    };

    // Report the fleet size once at startup. A panel that comes up seeing zero bots
    // on a host that has some is the signature of a wrong STITCH_PANEL_BOTS_DIR.
    let bots = state.fleet().await.map(|f| f.len()).unwrap_or_default();
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;

    tracing::info!(
        %bind,
        bots,
        runtime = runtime.as_str(),
        auth = %auth_summary,
        "stitch-panel {} is up",
        env!("CARGO_PKG_VERSION")
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving the panel")
}

/// A one-line summary of how the panel authenticates, logged at startup so an
/// operator can see it in `docker logs` without guessing.
fn describe_auth(auth: &AuthMode, trust_identity_header: bool) -> String {
    let mut parts = Vec::new();
    if !auth.allowed_users().is_empty() {
        parts.push(format!(
            "{} tailnet login(s){}",
            auth.allowed_users().len(),
            if trust_identity_header {
                ""
            } else {
                " (header NOT trusted — set STITCH_PANEL_TRUST_IDENTITY_HEADER=1)"
            }
        ));
    }
    if auth.password_hash().is_some() {
        parts.push("password".to_string());
    }
    parts.join(" + ")
}

/// Shut down on SIGTERM or Ctrl-C, letting in-flight requests finish. A settings
/// save that has written its file but not yet restarted the bot must not be cut in
/// half by a redeploy.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // Without SIGTERM we still have Ctrl-C; hanging here is correct so the
            // select falls through to it.
            Err(e) => {
                tracing::warn!("couldn't listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_piped_password_keeps_everything_but_the_line_ending() {
        assert_eq!(strip_line_ending("hunter2\n"), "hunter2");
        assert_eq!(strip_line_ending("hunter2\r\n"), "hunter2");
        // No terminator at all, which is what a `printf` without a newline gives.
        assert_eq!(strip_line_ending("hunter2"), "hunter2");
        // Spaces are password characters, not whitespace to tidy up.
        assert_eq!(strip_line_ending("two words \n"), "two words ");
        // Only the last line ending goes: a password can't contain a newline, but
        // stripping more than one would silently change a pasted value.
        assert_eq!(strip_line_ending("a\n\n"), "a\n");
    }

    #[test]
    fn the_auth_summary_names_every_mode_in_play() {
        let tailnet = AuthMode::Tailnet {
            allowed_users: vec!["a@example.com".into()],
        };
        assert_eq!(describe_auth(&tailnet, true), "1 tailnet login(s)");

        // Without the trust flag the header is ignored, and the operator needs to
        // see that in the startup line rather than discovering it at the login
        // page — along with the variable that fixes it.
        let untrusted = describe_auth(&tailnet, false);
        assert!(untrusted.contains("NOT trusted"), "{untrusted}");
        assert!(
            untrusted.contains("STITCH_PANEL_TRUST_IDENTITY_HEADER"),
            "{untrusted}"
        );

        let password = AuthMode::Password {
            hash: "$argon2id$v=19$stub".into(),
        };
        assert_eq!(describe_auth(&password, true), "password");

        let both = AuthMode::Either {
            allowed_users: vec!["a@example.com".into(), "b@example.com".into()],
            hash: "$argon2id$v=19$stub".into(),
        };
        assert_eq!(describe_auth(&both, true), "2 tailnet login(s) + password");
    }
}
