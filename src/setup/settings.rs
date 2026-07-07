// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Read and edit the handful of `stitch.toml` values the desktop Settings screen
//! exposes (RPC URL, price-feed URL, and the first pool's buy/sell spreads).
//!
//! Edits go through `toml_edit` so the template's comments and layout survive a
//! save, and every edit is re-validated through `Config::from_toml` before it is
//! handed back — a bad value fails here, so the caller never writes a broken file.
//! The operator wallet is NOT here: it lives in `stitch.key`, edited via
//! `writer::write_key`.

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::Config;

/// How a side's spread is expressed in the config. Editing preserves whichever
/// form the operator's config already uses rather than switching representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpreadKind {
    /// Basis points below/above the mid (`buy_offset_bps` / `sell_offset_bps`).
    #[default]
    Bps,
    /// Absolute soft-per-stable offset (`buy_offset_abs` / `sell_offset_abs`).
    Abs,
}

/// One side's spread as an editable value plus the representation it uses. `value`
/// is the number rendered as text (empty when the side has no spread configured).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpreadEdit {
    pub kind: SpreadKind,
    pub value: String,
}

/// The current editable settings, read from a `stitch.toml` for form prefill.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub rpc_url: String,
    pub feed_url: String,
    pub buy: SpreadEdit,
    pub sell: SpreadEdit,
    /// How many pools the config has. The screen only edits the first, and warns
    /// when there is more than one.
    pub pool_count: usize,
}

/// The desired new state of the four editable fields. Applied onto the existing
/// TOML text; the wallet key is handled separately.
#[derive(Debug, Clone)]
pub struct SettingsPatch {
    pub rpc_url: String,
    pub feed_url: String,
    pub buy: SpreadEdit,
    pub sell: SpreadEdit,
}

/// Read the current editable values from a `stitch.toml` body. Parses through the
/// real `Config` so an unreadable file surfaces the same error the bot would hit.
pub fn read_settings(toml_str: &str) -> Result<SettingsView> {
    let cfg = Config::from_toml(toml_str)?;
    let pool = cfg.pools.first().context("config has no [[pools]] entry")?;
    Ok(SettingsView {
        rpc_url: cfg.rpc_url.clone(),
        // The bot prefers the first pool's feed_url override over [feed].url
        // (see main.rs), so surface the endpoint that's actually effective.
        feed_url: pool
            .feed_url
            .clone()
            .unwrap_or_else(|| cfg.feed.url.clone()),
        buy: spread_edit(pool.buy_offset_bps, pool.buy_offset_abs),
        sell: spread_edit(pool.sell_offset_bps, pool.sell_offset_abs),
        pool_count: cfg.pools.len(),
    })
}

/// The current signer, read from a `stitch.toml` for form prefill. Only the
/// non-secret fields — secrets live in the env/secret file and are re-entered
/// when the operator changes the signer. No `[signer]` reads as the hot wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerView {
    Local,
    Turnkey {
        organization_id: String,
        sign_with: String,
        operator_address: String,
        api_base_url: String,
    },
    Mpcvault {
        vault_uuid: String,
        client_signer_pubkey: String,
        operator_address: String,
        api_base_url: String,
        callback_listen_addr: String,
    },
}

/// Read the current signer from a `stitch.toml` body. A missing `[signer]` (or an
/// unparseable config) reads as the hot wallet.
pub fn read_signer(toml_str: &str) -> SignerView {
    use crate::signer::SignerConfig;
    match Config::from_toml(toml_str).ok().and_then(|c| c.signer) {
        Some(SignerConfig::Turnkey(c)) => SignerView::Turnkey {
            organization_id: c.organization_id,
            sign_with: c.sign_with,
            operator_address: c.operator_address,
            api_base_url: c.api_base_url,
        },
        Some(SignerConfig::Mpcvault(c)) => SignerView::Mpcvault {
            vault_uuid: c.vault_uuid,
            client_signer_pubkey: c.client_signer_pubkey,
            operator_address: c.operator_address,
            api_base_url: c.api_base_url,
            callback_listen_addr: c.callback_listen_addr,
        },
        _ => SignerView::Local,
    }
}

/// Apply the patch onto `toml_str` and return the new TOML text. Preserves
/// comments/formatting, and re-validates the result before returning so a bad
/// edit fails here instead of on the next bot start.
pub fn apply_settings(toml_str: &str, patch: &SettingsPatch) -> Result<String> {
    // An empty endpoint still parses as a valid `Config` (both are plain
    // `String`s), so guard here or a cleared field would silently restart the bot
    // into a config that can't reach its RPC or feed.
    require_url(&patch.rpc_url, "RPC URL")?;
    require_url(&patch.feed_url, "price feed URL")?;

    let mut doc = toml_str
        .parse::<DocumentMut>()
        .context("parsing stitch.toml")?;

    set_value(
        doc.as_table_mut(),
        "rpc_url",
        Value::from(patch.rpc_url.trim()),
    );

    // Write the feed URL where the bot actually reads it: the first pool's
    // feed_url override when it has one, otherwise the bot-level [feed].url.
    // Editing [feed].url while a pool overrides it would look effective but be
    // ignored on restart.
    let pool_overrides_feed = doc
        .get("pools")
        .and_then(Item::as_array_of_tables)
        .and_then(|arr| arr.get(0))
        .is_some_and(|p| p.contains_key("feed_url"));

    if pool_overrides_feed {
        let pool = first_pool_mut(&mut doc)?;
        set_value(pool, "feed_url", Value::from(patch.feed_url.trim()));
    } else {
        let feed = doc
            .get_mut("feed")
            .and_then(Item::as_table_mut)
            .context("config has no [feed] table")?;
        set_value(feed, "url", Value::from(patch.feed_url.trim()));
    }

    let pool = first_pool_mut(&mut doc)?;
    apply_spread(pool, "buy", &patch.buy)?;
    apply_spread(pool, "sell", &patch.sell)?;

    let edited = doc.to_string();
    // Guard: never hand back something the bot can't load.
    Config::from_toml(&edited).context("the edited config is not valid")?;
    Ok(edited)
}

/// The first `[[pools]]` table, mutably. Errors if the config has no pools.
fn first_pool_mut(doc: &mut DocumentMut) -> Result<&mut Table> {
    doc.get_mut("pools")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|arr| arr.get_mut(0))
        .context("config has no [[pools]] entry")
}

/// Reject an endpoint that would leave the bot unable to reach its RPC or feed.
/// Both are used through reqwest's HTTP client, so fully parse the value and
/// require an http(s) scheme with a host — a bare `https://`, a `ws://`, or other
/// non-URL text would otherwise pass and fail every request after restart.
fn require_url(value: &str, field: &str) -> Result<()> {
    let v = value.trim();
    anyhow::ensure!(!v.is_empty(), "{field} can't be empty");
    let parsed = url::Url::parse(v)
        .with_context(|| format!("{field} must be a valid URL (like https://…)"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "{field} must be an http(s) URL (like https://…)"
    );
    anyhow::ensure!(
        parsed.host_str().is_some_and(|h| !h.is_empty()),
        "{field} must include a host (like https://api.example.com)"
    );
    Ok(())
}

/// Turn the two optional spread fields into an editable value + its kind.
fn spread_edit(bps: Option<u32>, abs: Option<f64>) -> SpreadEdit {
    match (bps, abs) {
        (Some(b), _) => SpreadEdit {
            kind: SpreadKind::Bps,
            value: b.to_string(),
        },
        (None, Some(a)) => SpreadEdit {
            kind: SpreadKind::Abs,
            value: a.to_string(),
        },
        (None, None) => SpreadEdit::default(),
    }
}

/// Write one side's spread back into the pool table, keeping the config's chosen
/// representation and removing the other form so the two can't disagree. An empty
/// value removes both offset keys, disabling that side (so the file always matches
/// what the field shows).
fn apply_spread(pool: &mut Table, side: &str, edit: &SpreadEdit) -> Result<()> {
    let raw = edit.value.trim();
    let bps_key = format!("{side}_offset_bps");
    let abs_key = format!("{side}_offset_abs");
    if raw.is_empty() {
        // Clearing a prefilled field disables the side: remove both offset forms
        // so the file matches the UI, rather than leaving the old spread in place
        // and reporting a save that didn't change anything.
        pool.remove(&bps_key);
        pool.remove(&abs_key);
        return Ok(());
    }
    match edit.kind {
        SpreadKind::Bps => {
            let n: u32 = raw
                .parse()
                .with_context(|| format!("{side} spread must be a whole number of basis points"))?;
            set_value(pool, &bps_key, Value::from(i64::from(n)));
            pool.remove(&abs_key);
        }
        SpreadKind::Abs => {
            let n: f64 = raw
                .parse()
                .with_context(|| format!("{side} spread must be a number"))?;
            // A negative (or non-finite) absolute offset crosses the book: the
            // bid would price above mid and the ask below it.
            anyhow::ensure!(
                n.is_finite() && n >= 0.0,
                "{side} spread must be a non-negative number"
            );
            set_value(pool, &abs_key, Value::from(n));
            pool.remove(&bps_key);
        }
    }
    Ok(())
}

/// Set a key's value while preserving its existing decor (the surrounding
/// whitespace and any inline `# comment`). Inserts a fresh key when it's absent.
fn set_value(table: &mut Table, key: &str, new: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        let mut next = new;
        *next.decor_mut() = existing.decor().clone();
        *existing = next;
    } else {
        table.insert(key, Item::Value(new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!("templates/cngn-usdt-bsc.toml");

    fn patch_from(view: &SettingsView) -> SettingsPatch {
        SettingsPatch {
            rpc_url: view.rpc_url.clone(),
            feed_url: view.feed_url.clone(),
            buy: view.buy.clone(),
            sell: view.sell.clone(),
        }
    }

    #[test]
    fn reads_current_values_from_a_template() {
        let v = read_settings(TEMPLATE).unwrap();
        assert!(v.rpc_url.starts_with("https://bsc-rpc.publicnode.com"));
        assert_eq!(
            v.feed_url,
            "https://api.textilecredit.com/price?chainId=56&pair=cngn-usdt"
        );
        assert_eq!(
            v.buy,
            SpreadEdit {
                kind: SpreadKind::Bps,
                value: "1".into()
            }
        );
        assert_eq!(
            v.sell,
            SpreadEdit {
                kind: SpreadKind::Bps,
                value: "1".into()
            }
        );
        assert_eq!(v.pool_count, 1);
    }

    #[test]
    fn a_noop_patch_keeps_the_file_byte_identical() {
        let view = read_settings(TEMPLATE).unwrap();
        let out = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();
        assert_eq!(
            out, TEMPLATE,
            "re-writing current values must not perturb the file"
        );
    }

    #[test]
    fn edits_all_four_fields_and_preserves_comments() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "https://rpc.example.com/key".into();
        view.feed_url = "https://feed.example.com/price".into();
        view.buy.value = "7".into();
        view.sell.value = "9".into();
        let out = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();

        let back = read_settings(&out).unwrap();
        assert_eq!(back.rpc_url, "https://rpc.example.com/key");
        assert_eq!(back.feed_url, "https://feed.example.com/price");
        assert_eq!(back.buy.value, "7");
        assert_eq!(back.sell.value, "9");

        // A block comment far from any edited line survives.
        assert!(out.contains("# Textile's own price endpoint."));
        // The inline comment on the edited spread line survives too.
        assert!(out.contains("# 1 bps below mid"));
        // Untouched keys are still there.
        assert!(out.contains("permit2"));
        assert!(out.contains("refresh_threshold_bps"));
    }

    #[test]
    fn writes_a_side_back_as_abs_when_the_source_used_abs() {
        // Start from a config whose buy side uses the absolute form.
        let src = TEMPLATE.replace(
            "buy_offset_bps = 1                              # 1 bps below mid",
            "buy_offset_abs = 0.0000015",
        );
        let mut view = read_settings(&src).unwrap();
        assert_eq!(view.buy.kind, SpreadKind::Abs);
        view.buy.value = "0.0000025".into();
        let out = apply_settings(&src, &patch_from(&view)).unwrap();
        assert!(out.contains("buy_offset_abs = 0.0000025"));
        assert!(!out.contains("buy_offset_bps"));
    }

    #[test]
    fn a_blank_or_non_http_endpoint_is_rejected() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "   ".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("RPC URL"));

        // A non-http scheme parses but is rejected by the scheme check.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "ws://node.example.com".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("http(s)"));

        // A scheme with no host (a common typo) is rejected too.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.rpc_url = "https://".into();
        assert!(apply_settings(TEMPLATE, &patch_from(&view)).is_err());

        let mut view = read_settings(TEMPLATE).unwrap();
        view.feed_url = "not-a-url".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("feed"));
    }

    #[test]
    fn feed_edit_follows_the_first_pools_override_when_present() {
        // A custom config where the first pool overrides the feed; the bot reads
        // this, not [feed].url.
        let src = TEMPLATE.replace(
            "collateral_decimals = 6",
            "collateral_decimals = 6\nfeed_url = \"https://pool-feed.example.com/old\"",
        );
        // read_settings surfaces the effective (override) endpoint.
        let mut view = read_settings(&src).unwrap();
        assert_eq!(view.feed_url, "https://pool-feed.example.com/old");

        // Saving writes back to the override, leaving [feed].url untouched.
        view.feed_url = "https://pool-feed.example.com/new".into();
        let out = apply_settings(&src, &patch_from(&view)).unwrap();
        assert!(out.contains("feed_url = \"https://pool-feed.example.com/new\""));
        assert!(out.contains(
            "url            = \"https://api.textilecredit.com/price?chainId=56&pair=cngn-usdt\""
        ));
    }

    #[test]
    fn clearing_a_prefilled_spread_removes_it_rather_than_leaving_it_stale() {
        let mut view = read_settings(TEMPLATE).unwrap();
        assert_eq!(view.buy.value, "1"); // template preloads a buy spread
        view.buy.value = "   ".into(); // operator clears it
        let out = apply_settings(TEMPLATE, &patch_from(&view)).unwrap();
        assert!(!out.contains("buy_offset_bps"));
        assert!(!out.contains("buy_offset_abs"));
        // The sell side and the rest of the config are untouched and still valid.
        assert!(out.contains("sell_offset_bps"));
        let back = read_settings(&out).unwrap();
        assert_eq!(back.buy, SpreadEdit::default());
    }

    #[test]
    fn a_negative_absolute_spread_is_rejected() {
        // Start from a config whose sell side uses the absolute form.
        let src = TEMPLATE.replace(
            "sell_offset_bps = 1                             # 1 bps above mid",
            "sell_offset_abs = 0.0000015",
        );
        let mut view = read_settings(&src).unwrap();
        assert_eq!(view.sell.kind, SpreadKind::Abs);
        view.sell.value = "-0.0000015".into();
        let err = apply_settings(&src, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("non-negative"));
    }

    #[test]
    fn a_non_numeric_spread_is_rejected_before_returning() {
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy.value = "wide".into();
        let err = apply_settings(TEMPLATE, &patch_from(&view)).unwrap_err();
        assert!(err.to_string().contains("basis points"));
    }

    #[test]
    fn an_edit_that_would_break_the_config_errors_and_returns_nothing_usable() {
        // An empty RPC URL still parses as a string, so force an invalid value a
        // different way: a spread that overflows u32 fails to parse as bps.
        let mut view = read_settings(TEMPLATE).unwrap();
        view.buy.value = "99999999999".into();
        assert!(apply_settings(TEMPLATE, &patch_from(&view)).is_err());
    }
}
