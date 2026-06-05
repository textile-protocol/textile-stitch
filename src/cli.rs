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
    /// only the committed liquidity instead of an unlimited allowance.
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
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    let config = config.ok_or_else(|| anyhow!("--config <path> is required"))?;
    if approve {
        return Ok(Command::Approve {
            config,
            dry_run,
            exact,
        });
    }
    if exact {
        return Err(anyhow!("--exact only applies to `approve`"));
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
}
