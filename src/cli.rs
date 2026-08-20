// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Command-line parsing for the bot. Kept as a pure function over an argument
//! iterator so it's unit-testable without touching the real process args.

use anyhow::anyhow;

/// What the operator asked the binary to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the bot against a config file.
    Run { config: String, dry_run: bool },
    /// Approve the config's input tokens to Permit2 and exit. `exact` approves
    /// only fixed configured liquidity instead of an unlimited allowance.
    Approve {
        config: String,
        dry_run: bool,
        exact: bool,
    },
    /// Print the version and exit.
    Version,
    /// Self-update to the latest release and exit.
    Update,
    /// Print usage and exit.
    Help,
    /// Interactively create a config in `dir` (or the default dir if None).
    Init { dir: Option<String> },
    /// Register this bot's wallet with Textile and write `[rfq]` +
    /// `rfq-api.key`. `venue_url` overrides the enroll endpoint (tests, and
    /// operators pointed at a private venue).
    Connect {
        config: String,
        venue_url: Option<String>,
    },
}

/// Parse a command from an argument iterator (already skipping argv[0]).
///
/// `--version`, `--update`, and `--help` are terminal: they short-circuit and
/// never require a config. Otherwise the binary runs, which needs `--config`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> anyhow::Result<Command> {
    let mut config: Option<String> = None;
    let mut dry_run = false;
    let mut approve = false;
    let mut exact = false;
    let mut init = false;
    let mut connect = false;
    let mut venue_url: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" | "-V" => return Ok(Command::Version),
            "--update" => return Ok(Command::Update),
            "--help" | "-h" => return Ok(Command::Help),
            // Verb: `stitch approve --config <path> [--exact] [--dry-run]`.
            "approve" => approve = true,
            "--config" => config = Some(it.next().ok_or_else(|| anyhow!("--config needs a path"))?),
            "--dry-run" => dry_run = true,
            "--exact" => exact = true,
            "init" => init = true,
            // Verb: `stitch connect --config <path> [--venue <url>]`.
            "connect" => connect = true,
            "--venue" => venue_url = Some(it.next().ok_or_else(|| anyhow!("--venue needs a URL"))?),
            "--dir" => dir = Some(it.next().ok_or_else(|| anyhow!("--dir needs a path"))?),
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    // Verbs are mutually exclusive, and so are their flags. Both used to be
    // resolved by an if-chain in priority order, which meant a malformed
    // invocation silently ran the higher-priority one and dropped the rest:
    // `stitch approve connect --config x --exact` enrolled — issuing a
    // credential and rewriting the config — while ignoring both the approval
    // and a flag documented as approve-only. Validate the whole invocation
    // before dispatching, so a wrong command line is an error rather than the
    // wrong live operation.
    let verbs: Vec<&str> = [(init, "init"), (connect, "connect"), (approve, "approve")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    if verbs.len() > 1 {
        return Err(anyhow!("pick one verb, got `{}`", verbs.join("` and `")));
    }
    if dir.is_some() && !init {
        return Err(anyhow!("--dir only applies to `init`"));
    }
    if venue_url.is_some() && !connect {
        return Err(anyhow!("--venue only applies to `connect`"));
    }
    if exact && !approve {
        return Err(anyhow!("--exact only applies to `approve`"));
    }
    if dry_run && connect {
        // `--dry-run` means "read, don't write", and enrollment has no such
        // shape: the venue round trip *is* the operation.
        return Err(anyhow!(
            "--dry-run does not apply to `connect`: enrolling issues a maker credential and \
             rewrites the config, so there is nothing to simulate"
        ));
    }
    if dry_run && init {
        return Err(anyhow!("--dry-run does not apply to `init`"));
    }

    if init {
        return Ok(Command::Init { dir });
    }
    let config = config.ok_or_else(|| anyhow!("--config <path> is required"))?;
    if connect {
        return Ok(Command::Connect { config, venue_url });
    }
    if approve {
        return Ok(Command::Approve {
            config,
            dry_run,
            exact,
        });
    }
    Ok(Command::Run { config, dry_run })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_vec(args: &[&str]) -> anyhow::Result<Command> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn a_malformed_invocation_never_picks_a_verb_for_you() {
        // These used to resolve by if-chain priority, so the extra verb and the
        // approve-only flag were dropped and Connect ran — issuing a credential
        // and rewriting the config off a command line that asked for neither.
        let err = parse_vec(&["approve", "connect", "--config", "s.toml", "--exact"])
            .expect_err("two verbs must not silently resolve to one");
        assert!(err.to_string().contains("connect"), "{err}");
        assert!(err.to_string().contains("approve"), "{err}");

        // Flags belong to their verb, whichever verb won before.
        for (args, want) in [
            (vec!["connect", "--config", "s.toml", "--exact"], "--exact"),
            (
                vec!["approve", "--config", "s.toml", "--venue", "http://x"],
                "--venue",
            ),
            (vec!["init", "--dir", "d", "--exact"], "--exact"),
            (vec!["--config", "s.toml", "--dir", "d"], "--dir"),
            (vec!["init", "--dry-run"], "--dry-run"),
        ] {
            let err = parse_vec(&args).expect_err(&format!("{args:?} must be rejected"));
            assert!(err.to_string().contains(want), "{args:?}: {err}");
        }

        // The valid shapes still parse.
        assert_eq!(
            parse_vec(&["approve", "--config", "s.toml", "--exact", "--dry-run"]).unwrap(),
            Command::Approve {
                config: "s.toml".into(),
                dry_run: true,
                exact: true
            }
        );
        assert_eq!(
            parse_vec(&["init", "--dir", "d"]).unwrap(),
            Command::Init {
                dir: Some("d".into())
            }
        );
    }

    #[test]
    fn connect_refuses_dry_run() {
        // Enrollment issues a credential and rewrites the config on the venue's
        // say-so. Accepting `--dry-run` and doing it anyway is the one outcome
        // an operator who typed that flag did not ask for.
        let err = parse_vec(&["connect", "--config", "stitch.toml", "--dry-run"])
            .expect_err("connect must refuse --dry-run");
        assert!(err.to_string().contains("--dry-run"), "{err}");
        assert!(err.to_string().contains("connect"), "{err}");
        // Without it, connect still parses.
        assert_eq!(
            parse_vec(&["connect", "--config", "stitch.toml"]).unwrap(),
            Command::Connect {
                config: "stitch.toml".into(),
                venue_url: None
            }
        );
    }

    #[test]
    fn config_only_runs_without_dry_run() {
        let cmd = parse_vec(&["--config", "stitch.toml"]).unwrap();
        assert_eq!(
            cmd,
            Command::Run {
                config: "stitch.toml".into(),
                dry_run: false
            }
        );
    }

    #[test]
    fn dry_run_flag_sets_dry_run() {
        let cmd = parse_vec(&["--config", "stitch.toml", "--dry-run"]).unwrap();
        assert_eq!(
            cmd,
            Command::Run {
                config: "stitch.toml".into(),
                dry_run: true
            }
        );
    }

    #[test]
    fn version_flag_short_circuits() {
        assert_eq!(parse_vec(&["--version"]).unwrap(), Command::Version);
        assert_eq!(parse_vec(&["-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn version_wins_over_other_args() {
        // Asking for the version should never require a config.
        assert_eq!(
            parse_vec(&["--config", "x.toml", "--version"]).unwrap(),
            Command::Version
        );
    }

    #[test]
    fn update_flag_is_recognized() {
        assert_eq!(parse_vec(&["--update"]).unwrap(), Command::Update);
    }

    #[test]
    fn help_flag_is_recognized() {
        assert_eq!(parse_vec(&["--help"]).unwrap(), Command::Help);
        assert_eq!(parse_vec(&["-h"]).unwrap(), Command::Help);
    }

    #[test]
    fn approve_verb_defaults_to_max() {
        let cmd = parse_vec(&["approve", "--config", "stitch.toml"]).unwrap();
        assert_eq!(
            cmd,
            Command::Approve {
                config: "stitch.toml".into(),
                dry_run: false,
                exact: false,
            }
        );
    }

    #[test]
    fn approve_verb_accepts_exact_and_dry_run() {
        let cmd =
            parse_vec(&["approve", "--config", "stitch.toml", "--exact", "--dry-run"]).unwrap();
        assert_eq!(
            cmd,
            Command::Approve {
                config: "stitch.toml".into(),
                dry_run: true,
                exact: true,
            }
        );
    }

    #[test]
    fn approve_still_needs_a_config() {
        assert!(parse_vec(&["approve"]).is_err());
    }

    #[test]
    fn exact_without_approve_is_an_error() {
        assert!(parse_vec(&["--config", "stitch.toml", "--exact"]).is_err());
    }

    #[test]
    fn missing_config_is_an_error() {
        assert!(parse_vec(&["--dry-run"]).is_err());
    }

    #[test]
    fn unknown_arg_is_an_error() {
        assert!(parse_vec(&["--config", "x.toml", "--frobnicate"]).is_err());
    }

    #[test]
    fn config_without_value_is_an_error() {
        assert!(parse_vec(&["--config"]).is_err());
    }

    #[test]
    fn init_verb_without_dir() {
        assert_eq!(parse_vec(&["init"]).unwrap(), Command::Init { dir: None });
    }

    #[test]
    fn init_verb_with_dir() {
        assert_eq!(
            parse_vec(&["init", "--dir", "/opt/stitch"]).unwrap(),
            Command::Init {
                dir: Some("/opt/stitch".into())
            }
        );
    }

    #[test]
    fn init_does_not_require_config() {
        assert!(parse_vec(&["init"]).is_ok());
    }

    #[test]
    fn connect_verb_needs_a_config() {
        assert!(parse_vec(&["connect"]).is_err());
        assert_eq!(
            parse_vec(&["connect", "--config", "stitch.toml"]).unwrap(),
            Command::Connect {
                config: "stitch.toml".into(),
                venue_url: None,
            }
        );
    }

    #[test]
    fn connect_verb_accepts_a_venue_override() {
        assert_eq!(
            parse_vec(&[
                "connect",
                "--config",
                "stitch.toml",
                "--venue",
                "https://api.example/v2/maker/enroll",
            ])
            .unwrap(),
            Command::Connect {
                config: "stitch.toml".into(),
                venue_url: Some("https://api.example/v2/maker/enroll".into()),
            }
        );
    }

    #[test]
    fn venue_without_connect_is_an_error() {
        assert!(parse_vec(&["--config", "x.toml", "--venue", "https://x"]).is_err());
    }

    #[test]
    fn venue_without_value_is_an_error() {
        assert!(parse_vec(&["connect", "--config", "x.toml", "--venue"]).is_err());
    }
}
