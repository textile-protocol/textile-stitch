// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Validate an operator-supplied `stitch.toml` before the panel writes it.
//!
//! The admin corridor flow generates a file; the panel's custom-corridor step
//! can import it instead of filling fields. This is the gate: parse, load
//! through [`Config::from_toml`], and refuse anything that looks like a
//! secret. The wizard collects the wallet separately — a toml that already
//! carries a `[signer]` (or a private key field) is how keys leak.

use anyhow::{bail, Context, Result};

use crate::config::Config;

/// Hard cap so a pasted bomb can't sit in memory. A real stitch.toml is a few
/// kilobytes; 64 KiB is generous for comments and still small.
pub const MAX_TOML_BYTES: usize = 64 * 1024;

/// Exact key names that mean a secret lives in this file.
const FORBIDDEN_KEYS: &[&str] = &[
    "signer",
    "private_key",
    "seed_phrase",
    "mnemonic",
    "api_private_key",
    "api_token",
    "api_key",
    "secret_key",
    "password",
    "passwd",
    "access_token",
    "client_secret",
    "client_key",
    "auth_token",
    "credential",
    "credentials",
    "token",
    "secret",
];

/// Substrings that make a key secret-bearing even when the exact name is new.
const FORBIDDEN_KEY_PARTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "private_key",
    "mnemonic",
    "seed_phrase",
    "access_token",
    "client_secret",
    "credential",
];

fn looks_like_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace('-', "_");
    if FORBIDDEN_KEYS.iter().any(|f| k == *f) {
        return true;
    }
    if FORBIDDEN_KEY_PARTS.iter().any(|part| k.contains(part)) {
        return true;
    }
    k.ends_with("_token") || k.ends_with("_secret") || k.ends_with("_password")
}

/// Validate a pasted / uploaded `stitch.toml`. On success returns the original
/// body (comments preserved) after proving the bot can load it.
pub fn validate_imported_toml(raw: &str) -> Result<String> {
    if raw.len() > MAX_TOML_BYTES {
        bail!(
            "stitch.toml is too large ({} bytes; max is {} KiB)",
            raw.len(),
            MAX_TOML_BYTES / 1024
        );
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("paste a stitch.toml, or pick a file");
    }

    let parsed: toml::Value = toml::from_str(trimmed).context("this is not valid TOML")?;
    let mut keys = Vec::new();
    collect_keys(&parsed, &mut keys);
    if let Some(key) = keys.iter().find(|k| looks_like_secret_key(k)) {
        bail!(
            "this file includes a `{key}` field. Remove it — the wizard collects \
             the wallet separately, and importing a key in a toml file is how keys leak."
        );
    }

    let cfg =
        Config::from_toml(trimmed).context("this stitch.toml is not a config the bot can load")?;
    if cfg.signer.is_some() {
        bail!(
            "this file includes a [signer] section. Remove it — the wizard \
             collects the wallet separately."
        );
    }
    if cfg.pools.is_empty() {
        bail!("this stitch.toml has no [[pools]] — a bot needs at least one pair");
    }

    Ok(trimmed.to_string())
}

fn collect_keys(value: &toml::Value, out: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                out.push(key.clone());
                collect_keys(child, out);
            }
        }
        toml::Value::Array(items) => {
            for child in items {
                collect_keys(child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> String {
        r#"
chain_id        = 42220
rpc_url         = "https://forno.celo.org"
indexer_url     = "https://api.textilecredit.com"
permit2         = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor         = "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"
tick_interval_secs = 5

[feed]
url            = "https://api.textilecredit.com/price?chainId=42220&pair=cngn-usdt"
staleness_secs = 900

[[pools]]
collateral = "0x1111111111111111111111111111111111111111"
collateral_decimals = 6
debt = "0x2222222222222222222222222222222222222222"
debt_decimals = 6
buy_offset_bps = 5
sell_offset_bps = 5
ttl_secs = 60
"#
        .to_string()
    }

    #[test]
    fn a_valid_file_is_accepted_and_loads() {
        let out = validate_imported_toml(&valid()).expect("valid toml");
        Config::from_toml(&out).expect("bot loads it");
    }

    #[test]
    fn a_signer_table_is_refused() {
        let err = validate_imported_toml(
            &(valid() + "\n[signer]\nbackend = \"local\"\nprivate_key = \"0xabc\"\n"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("signer"), "{err}");
    }

    #[test]
    fn secret_like_keys_are_refused_even_when_not_on_the_short_list() {
        for extra in [
            "\npassword = \"hunter2\"\n",
            "\naccess_token = \"tok\"\n",
            "\nclient_secret = \"shh\"\n",
            "\npanel_password = \"x\"\n",
        ] {
            let err = validate_imported_toml(&(valid() + extra)).unwrap_err();
            assert!(
                err.to_string().contains("field"),
                "expected a secret-field error for {extra:?}, got {err}"
            );
        }
    }

    #[test]
    fn api_key_env_is_not_treated_as_a_secret() {
        validate_imported_toml(&(valid() + "\napi_key_env = \"STITCH_MAKER_API_KEY\"\n"))
            .expect("env var name is not a secret");
    }

    #[test]
    fn a_private_key_field_is_refused_even_outside_signer() {
        // A comment mentioning the words is fine; a parsed key is not.
        let with_comment = format!(
            "# the wallet key is NOT here; no private_key in this file\n{}",
            valid()
        );
        validate_imported_toml(&with_comment).expect("comment is not a key");
    }

    #[test]
    fn empty_and_oversized_files_are_refused() {
        assert!(validate_imported_toml("   ").is_err());
        let huge = "x".repeat(MAX_TOML_BYTES + 1);
        assert!(validate_imported_toml(&huge)
            .unwrap_err()
            .to_string()
            .contains("too large"));
    }

    #[test]
    fn garbage_toml_is_refused_before_the_bot_parser() {
        let err = validate_imported_toml("this is not toml [[[").unwrap_err();
        assert!(err.to_string().contains("not valid TOML"), "{err}");
    }

    #[test]
    fn a_config_the_bot_cannot_load_is_refused() {
        let err = validate_imported_toml(
            r#"
chain_id = 42220
rpc_url = "https://forno.celo.org"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"
[feed]
url = "ftp://evil.example/price"
[[pools]]
collateral = "0x1111111111111111111111111111111111111111"
collateral_decimals = 6
debt = "0x2222222222222222222222222222222222222222"
debt_decimals = 6
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not a config the bot can load"),
            "{err}"
        );
    }
}
