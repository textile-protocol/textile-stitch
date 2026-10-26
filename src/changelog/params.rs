// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The parameters the change log tracks, read out of a config, and the diff
//! between two readings.
//!
//! Values are the ones the bot runs with, not the text in the file: loader
//! defaults are applied (`[price_guard]` absent reads as 1500 / 1000 / 300),
//! a pool on the bot-level feed reports that feed as its `feed_url`, and the
//! RFQ freshness window is the clamped one the responder enforces. Deleting a
//! line that set a default therefore records the default it falls back to,
//! which is what actually changed.
//!
//! Every value is a string so one column holds bps, seconds, atomic amounts
//! and URLs. `None` means the parameter is unset (a side with no spread, a
//! pool that no longer exists).

use std::collections::BTreeMap;

use crate::config::{rfq_staleness_secs_for_pool, Config, PoolConfig};

/// Scope of bot-wide parameters. Pool parameters use [`pool_scope`].
pub const BOT_SCOPE: &str = "bot";

/// Which parameter, where. Ordered so a diff comes out grouped by scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParamKey {
    pub scope: String,
    pub name: String,
}

/// One parameter's reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// Human name for the scope, e.g. `ZARU → USDC`. Informational only.
    pub scope_label: Option<String>,
    pub unit: String,
    pub value: Option<String>,
}

pub type ParamMap = BTreeMap<ParamKey, Param>;

/// One parameter that differs between two readings.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamChange {
    pub key: ParamKey,
    pub scope_label: Option<String>,
    pub unit: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Read the tracked parameters out of the text of a config.
///
/// Serde only, without [`Config::validate`]: the snapshot may have been
/// written by an older release whose loader accepted something this one
/// refuses, and it still has to be readable to diff against. `None` when the
/// text isn't a config at all.
pub fn params_of(toml: &str) -> Option<ParamMap> {
    toml::from_str::<Config>(toml).ok().map(|cfg| extract(&cfg))
}

/// Every tracked parameter of a parsed config.
pub fn extract(cfg: &Config) -> ParamMap {
    bot_params(cfg)
        .into_iter()
        .chain(cfg.pools.iter().flat_map(|pool| pool_params(cfg, pool)))
        .chain(module_params(cfg))
        .collect()
}

/// What changed from `before` to `after`, in key order. A key present on one
/// side only is a change from or to unset.
pub fn diff(before: &ParamMap, after: &ParamMap) -> Vec<ParamChange> {
    let keys: std::collections::BTreeSet<&ParamKey> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter_map(|key| {
            let (old, new) = (before.get(key), after.get(key));
            let old_value = old.and_then(|p| p.value.clone());
            let new_value = new.and_then(|p| p.value.clone());
            if old_value == new_value {
                return None;
            }
            let described = new.or(old)?;
            Some(ParamChange {
                key: key.clone(),
                scope_label: described.scope_label.clone(),
                unit: described.unit.clone(),
                before: old_value,
                after: new_value,
            })
        })
        .collect()
}

/// The scope string of one pool: its token pair, lowercased. The pair is the
/// only identity a pool has that an add or remove can't renumber.
pub fn pool_scope(pool: &PoolConfig) -> String {
    format!(
        "pool:{}/{}",
        pool.collateral.trim().to_lowercase(),
        pool.debt.trim().to_lowercase()
    )
}

const BPS: &str = "bps";
const SECONDS: &str = "seconds";
const BOOL: &str = "bool";
const URL: &str = "url";
const PRICE_OFFSET: &str = "price units (collateral per debt)";

fn bot_param(name: &str, unit: &str, value: Option<String>) -> (ParamKey, Param) {
    (
        ParamKey {
            scope: BOT_SCOPE.to_string(),
            name: name.to_string(),
        },
        Param {
            scope_label: None,
            unit: unit.to_string(),
            value,
        },
    )
}

fn bot_params(cfg: &Config) -> Vec<(ParamKey, Param)> {
    let guard = &cfg.price_guard;
    vec![
        bot_param("max_loss_bps", BPS, Some(guard.max_loss_bps.to_string())),
        bot_param("max_jump_bps", BPS, Some(guard.max_jump_bps.to_string())),
        bot_param("settle_secs", SECONDS, Some(guard.settle_secs.to_string())),
        bot_param(
            "staleness_secs",
            SECONDS,
            Some(cfg.feed.staleness_secs.to_string()),
        ),
        bot_param(
            "rfq_enabled",
            BOOL,
            Some(cfg.rfq.as_ref().is_some_and(|r| r.enabled).to_string()),
        ),
        bot_param("book_enabled", BOOL, Some(cfg.book_enabled.to_string())),
        bot_param(
            "modules_enabled",
            BOOL,
            Some(cfg.modules_enabled.to_string()),
        ),
        bot_param(
            "change_log.ship",
            BOOL,
            Some(cfg.change_log.ship.to_string()),
        ),
    ]
}

fn pool_params(cfg: &Config, pool: &PoolConfig) -> Vec<(ParamKey, Param)> {
    let scope = pool_scope(pool);
    let label = pool_label(cfg.chain_id, pool);
    let debt_units = atomic_unit("debt", pool.debt_decimals);
    let collateral_units = atomic_unit("collateral", pool.collateral_decimals);
    let feed = pool.feed_url.as_deref().unwrap_or(&cfg.feed.url);
    let entry = |name: &str, unit: &str, value: Option<String>| {
        (
            ParamKey {
                scope: scope.clone(),
                name: name.to_string(),
            },
            Param {
                scope_label: label.clone(),
                unit: unit.to_string(),
                value,
            },
        )
    };
    vec![
        entry(
            "buy_offset_bps",
            BPS,
            pool.buy_offset_bps.map(|v| v.to_string()),
        ),
        entry(
            "buy_offset_abs",
            PRICE_OFFSET,
            pool.buy_offset_abs.map(|v| v.to_string()),
        ),
        entry(
            "sell_offset_bps",
            BPS,
            pool.sell_offset_bps.map(|v| v.to_string()),
        ),
        entry(
            "sell_offset_abs",
            PRICE_OFFSET,
            pool.sell_offset_abs.map(|v| v.to_string()),
        ),
        entry(
            "buy_total_liquidity_debt",
            &debt_units,
            trimmed(pool.buy_total_liquidity_debt.as_deref()),
        ),
        entry(
            "sell_total_liquidity_collateral",
            &collateral_units,
            trimmed(pool.sell_total_liquidity_collateral.as_deref()),
        ),
        entry(
            "buy_order_size_debt",
            &debt_units,
            trimmed(pool.buy_order_size_debt.as_deref()),
        ),
        entry(
            "sell_order_size_collateral",
            &collateral_units,
            trimmed(pool.sell_order_size_collateral.as_deref()),
        ),
        entry("feed_url", URL, Some(redact(feed.trim()))),
        entry(
            "rfq_staleness_secs",
            SECONDS,
            Some(rfq_staleness_secs_for_pool(&cfg.feed, pool).to_string()),
        ),
    ]
}

/// `[modules]` leaves, flattened to dotted names, only while modules run.
///
/// The block is always present with defaults, so reading it on a bot that
/// doesn't run modules would put thirty unused values in every baseline.
/// Turning modules on records them appearing next to `modules_enabled`
/// flipping, and turning them off records them going away.
fn module_params(cfg: &Config) -> Vec<(ParamKey, Param)> {
    if !cfg.modules_enabled {
        return Vec::new();
    }
    let Ok(toml::Value::Table(table)) = toml::Value::try_from(&cfg.modules) else {
        return Vec::new();
    };
    flatten("modules", &toml::Value::Table(table))
        .into_iter()
        .map(|(name, value)| {
            let unit = unit_from_name(&name).to_string();
            bot_param(&name, &unit, Some(value))
        })
        .collect()
}

/// Leaf values of a TOML tree as `(dotted.name, text)`. Arrays are leaves.
fn flatten(prefix: &str, value: &toml::Value) -> Vec<(String, String)> {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .flat_map(|(k, v)| flatten(&format!("{prefix}.{k}"), v))
            .collect(),
        toml::Value::String(s) => vec![(prefix.to_string(), redact(s))],
        other => vec![(prefix.to_string(), other.to_string())],
    }
}

fn unit_from_name(name: &str) -> &'static str {
    if name.ends_with("_bps") {
        BPS
    } else if name.ends_with("_secs") {
        SECONDS
    } else if name.ends_with("enabled") || name.ends_with("_aware") {
        BOOL
    } else {
        ""
    }
}

fn atomic_unit(leg: &str, decimals: u8) -> String {
    format!("{leg} token atomic units ({decimals} decimals)")
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The pool's display name: what the file stamped, else the shipped catalog.
fn pool_label(chain_id: u64, pool: &PoolConfig) -> Option<String> {
    pool.corridor_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| {
            crate::setup::identify_pair(chain_id, &pool.collateral, &pool.debt)
                .map(|c| c.display_name.to_string())
        })
}

/// Query parameters whose values are credentials, by name.
const SECRET_QUERY_KEYS: [&str; 7] = [
    "key",
    "token",
    "secret",
    "signature",
    "sig",
    "auth",
    "password",
];

/// Strip credentials out of a URL before it leaves the config.
///
/// Records may ship off the host, and a feed or dealer URL can carry an API
/// key in its userinfo or query string. Each secret becomes `REDACTED#` plus
/// eight hex digits of its SHA-256, so rotating a key still shows up as a
/// change without the key itself being written down. Anything that isn't an
/// absolute URL is returned unchanged.
pub fn redact(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.to_string();
    };
    if url.cannot_be_a_base() {
        return value.to_string();
    }
    let mut touched = false;
    if !url.username().is_empty() || url.password().is_some() {
        let userinfo = format!("{}:{}", url.username(), url.password().unwrap_or(""));
        let _ = url.set_username(&fingerprint(&userinfo));
        let _ = url.set_password(None);
        touched = true;
    }
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.iter().any(|(k, _)| is_secret_key(k)) {
        let redacted: Vec<(String, String)> = pairs
            .into_iter()
            .map(|(k, v)| {
                let v = if is_secret_key(&k) {
                    fingerprint(&v)
                } else {
                    v
                };
                (k, v)
            })
            .collect();
        url.query_pairs_mut().clear().extend_pairs(redacted);
        touched = true;
    }
    if touched {
        url.to_string()
    } else {
        value.to_string()
    }
}

fn is_secret_key(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_QUERY_KEYS.iter().any(|s| lower.contains(s))
}

fn fingerprint(secret: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(secret.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("REDACTED#{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZARU: &str = include_str!("testdata/zaru.toml");

    fn value(map: &ParamMap, scope: &str, name: &str) -> Option<String> {
        map.get(&ParamKey {
            scope: scope.to_string(),
            name: name.to_string(),
        })
        .and_then(|p| p.value.clone())
    }

    const POOL: &str = "pool:0x00000000000000000000000000000000000000aa/0x00000000000000000000000000000000000000bb";

    #[test]
    fn reads_the_contract_parameters_with_loader_defaults_applied() {
        let map = params_of(ZARU).expect("the fixture parses");
        assert_eq!(value(&map, POOL, "buy_offset_bps").as_deref(), Some("40"));
        assert_eq!(value(&map, POOL, "sell_offset_bps").as_deref(), Some("45"));
        assert_eq!(value(&map, POOL, "buy_offset_abs"), None);
        assert_eq!(
            value(&map, POOL, "buy_total_liquidity_debt").as_deref(),
            Some("250000000000")
        );
        assert_eq!(
            value(&map, POOL, "sell_total_liquidity_collateral").as_deref(),
            Some("4500000000000")
        );
        // No [price_guard] table: the loader's defaults are what runs.
        assert_eq!(
            value(&map, BOT_SCOPE, "max_loss_bps").as_deref(),
            Some("1500")
        );
        assert_eq!(
            value(&map, BOT_SCOPE, "max_jump_bps").as_deref(),
            Some("1000")
        );
        assert_eq!(
            value(&map, BOT_SCOPE, "settle_secs").as_deref(),
            Some("300")
        );
        assert_eq!(
            value(&map, BOT_SCOPE, "staleness_secs").as_deref(),
            Some("120")
        );
        // The pool rides the bot-level feed, so that is its feed.
        assert_eq!(
            value(&map, POOL, "feed_url").as_deref(),
            Some("https://api.textilecredit.com/price?pair=zaru-usdc")
        );
        assert!(value(&map, POOL, "rfq_staleness_secs").is_some());
        assert_eq!(
            value(&map, BOT_SCOPE, "change_log.ship").as_deref(),
            Some("false")
        );
        let pool = map
            .get(&ParamKey {
                scope: POOL.to_string(),
                name: "buy_total_liquidity_debt".to_string(),
            })
            .unwrap();
        assert_eq!(pool.scope_label.as_deref(), Some("ZARU → USDC"));
        assert_eq!(pool.unit, "debt token atomic units (6 decimals)");
    }

    #[test]
    fn module_parameters_appear_only_while_modules_run() {
        let off = params_of(ZARU).unwrap();
        assert!(!off.keys().any(|k| k.name.starts_with("modules.")));
        let on = params_of(&ZARU.replacen(
            "tick_interval_secs",
            "modules_enabled = true\ntick_interval_secs",
            1,
        ))
        .unwrap();
        assert_eq!(
            value(&on, BOT_SCOPE, "modules.spreads.max_extra_bps").as_deref(),
            Some("100")
        );
        assert_eq!(
            on.get(&ParamKey {
                scope: BOT_SCOPE.into(),
                name: "modules.spreads.max_extra_bps".into()
            })
            .unwrap()
            .unit,
            "bps"
        );
    }

    #[test]
    fn the_inventory_curve_is_logged_with_the_other_modules() {
        // Its settings live under [modules], so they are recorded the way
        // every module setting is: while modules run.
        let on = params_of(&format!(
            "modules_enabled = true\n{ZARU}\n[modules.curve]\nenabled = true\nend_above_buy = 1.5\n"
        ))
        .expect("the fixture with modules parses");
        assert_eq!(
            value(&on, BOT_SCOPE, "modules.curve.enabled").as_deref(),
            Some("true")
        );
        assert_eq!(
            value(&on, BOT_SCOPE, "modules.curve.end_above_buy").as_deref(),
            Some("1.5")
        );
        assert_eq!(
            value(&on, BOT_SCOPE, "modules.curve.shape").as_deref(),
            Some("2.0")
        );
        let off = params_of(ZARU).unwrap();
        assert!(!off.keys().any(|k| k.name.starts_with("modules.curve")));
    }

    #[test]
    fn an_unchanged_config_diffs_to_nothing() {
        let a = params_of(ZARU).unwrap();
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn a_spread_edit_is_one_change_with_both_values() {
        let before = params_of(ZARU).unwrap();
        let after = params_of(&ZARU.replace("buy_offset_bps = 40", "buy_offset_bps = 25")).unwrap();
        let changes = diff(&before, &after);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].key.name, "buy_offset_bps");
        assert_eq!(changes[0].key.scope, POOL);
        assert_eq!(changes[0].before.as_deref(), Some("40"));
        assert_eq!(changes[0].after.as_deref(), Some("25"));
        assert_eq!(changes[0].unit, "bps");
    }

    #[test]
    fn switching_a_spread_form_records_both_sides_of_the_switch() {
        let before = params_of(ZARU).unwrap();
        let after =
            params_of(&ZARU.replace("sell_offset_bps = 45", "sell_offset_abs = 0.35")).unwrap();
        let changes = diff(&before, &after);
        let names: Vec<_> = changes.iter().map(|c| c.key.name.as_str()).collect();
        assert_eq!(names, ["sell_offset_abs", "sell_offset_bps"]);
        assert_eq!(changes[0].before, None);
        assert_eq!(changes[0].after.as_deref(), Some("0.35"));
        assert_eq!(changes[1].before.as_deref(), Some("45"));
        assert_eq!(changes[1].after, None);
    }

    #[test]
    fn dropping_a_price_guard_line_records_the_default_it_falls_back_to() {
        let tuned = format!("{ZARU}\n[price_guard]\nmax_loss_bps = 500\n");
        let changes = diff(&params_of(&tuned).unwrap(), &params_of(ZARU).unwrap());
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].key.name, "max_loss_bps");
        assert_eq!(changes[0].before.as_deref(), Some("500"));
        assert_eq!(changes[0].after.as_deref(), Some("1500"));
    }

    #[test]
    fn a_new_pool_appears_as_values_set_from_nothing() {
        let before = params_of(ZARU).unwrap();
        let second = ZARU.split("[[pools]]").nth(1).unwrap().replace(
            "0x00000000000000000000000000000000000000AA",
            "0x00000000000000000000000000000000000000CC",
        );
        let after = params_of(&format!("{ZARU}\n[[pools]]{second}")).unwrap();
        let changes = diff(&before, &after);
        assert!(!changes.is_empty());
        assert!(changes.iter().all(|c| c
            .key
            .scope
            .starts_with("pool:0x00000000000000000000000000000000000000cc/")));
        assert!(changes.iter().all(|c| c.before.is_none()));
    }

    #[test]
    fn text_that_is_not_a_config_reads_as_none() {
        assert!(params_of("not = [valid").is_none());
        assert!(params_of("chain_id = 1").is_none());
    }

    #[test]
    fn credentials_in_urls_are_fingerprinted_not_written_down() {
        let plain = "https://api.textilecredit.com/price?pair=zaru-usdc";
        assert_eq!(redact(plain), plain);
        // Same vector as the API's redactUrl test, so both sides store one form.
        assert_eq!(
            redact("https://feed.example/p?apiKey=sk_live_123"),
            "https://feed.example/p?apiKey=REDACTED%2346e8faff"
        );
        let keyed = redact("https://feed.example/price?pair=zar&apiKey=sk_live_123");
        assert!(!keyed.contains("sk_live_123"), "{keyed}");
        assert!(keyed.contains("pair=zar"), "{keyed}");
        assert!(keyed.contains("apiKey=REDACTED%23"), "{keyed}");
        let rotated = redact("https://feed.example/price?pair=zar&apiKey=sk_live_456");
        assert_ne!(keyed, rotated, "a rotated key must still read as a change");
        let userinfo = redact("https://user:hunter2@feed.example/price");
        assert!(!userinfo.contains("hunter2"), "{userinfo}");
        assert_eq!(redact("not a url"), "not a url");
    }
}
