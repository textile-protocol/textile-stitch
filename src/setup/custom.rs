// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Rendering a `stitch.toml` for an operator-supplied ("custom") corridor.
//!
//! The [`catalog`](crate::setup::catalog) ships vetted corridors; this lets an
//! operator stand a bot up on a pair the catalog doesn't have yet. We ask only
//! for what can't be defaulted — the chain, its RPC, the settlement reactor, the
//! two tokens, and a price feed — and fill everything else (Permit2, the indexer,
//! spreads, sizes, cadence) with defaults. All of it is editable afterwards from
//! the bot's Settings / raw config screen, which is the point of keeping this
//! form short.
//!
//! The spread/cadence defaults are the *conservative* ones — the wide-spread,
//! short-TTL, re-post-every-tick profile the shipped WETH/NVDA presets use, not
//! the tight 1 bp of an FX pair. A custom pair's volatility is unknown, and a
//! wide default can't pick the operator off on a fast move; they tighten it in
//! Settings if the pair is a stablecoin. See [`CUSTOM_OFFSET_BPS`].
//!
//! Two guards keep an operator off the default hosted indexer when it can't
//! actually settle their orders (each would otherwise create a healthy-looking
//! bot that fails every submission): the chain must be one the hosted service has
//! a reactor for (see [`INDEXER_SUPPORTED_CHAINS`]), and a custom Permit2 can't be
//! paired with it (the hosted indexer signs against the canonical Permit2). Both
//! are lifted when the operator supplies their own indexer.

use alloy_primitives::Address;
use anyhow::{ensure, Context, Result};
use serde::Deserialize;

/// Canonical Permit2, the same address on every chain, so operators never supply
/// it — it's a default, not a form field.
const CANONICAL_PERMIT2: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

/// Textile's hosted indexer — the default order sink / estimate source.
const DEFAULT_INDEXER_URL: &str = "https://api.textilecredit.com";

/// Chains the hosted indexer at [`DEFAULT_INDEXER_URL`] can actually settle on —
/// the ones for which the hosted service has a `SETTLEMENT_V3_FILLER_REACTOR`
/// configured. It's not enough for the indexer to *reach* a chain: `submitFillerOrder`
/// looks up `expectedReactor` per chain and rejects with "Filler reactor is not
/// configured for this chain" when the constant is absent, so a bot on a
/// reactor-less chain would create fine and then fail *every* submission. Keying
/// on the reactor (not just `getChain`) keeps out chains like Arbitrum and the
/// Sepolia testnets, which the indexer can reach but has no reactor for.
///
/// Source of truth: `packages/constants/src/addresses.*.json`
/// (`SETTLEMENT_V3_FILLER_REACTOR`); keep this in sync when a reactor is deployed
/// to a new chain. An operator on any other chain must point the bot at their own
/// indexer instead.
const INDEXER_SUPPORTED_CHAINS: &[u64] = &[
    1,     // Ethereum mainnet
    56,    // BNB Smart Chain
    97,    // BNB Smart Chain testnet
    137,   // Polygon
    4663,  // Robinhood Chain
    8453,  // Base
    42220, // Celo
];

/// Default ladder spread for a custom corridor, in basis points per side. A
/// custom pair's volatility is unknown, so this matches the *conservative*
/// profile the shipped WETH/NVDA presets use (5 bps, not the 1 bp of a tight FX
/// pair): a wide default can't pick the operator off on a fast move, and they can
/// tighten it in Settings for a stable pair. See [`CUSTOM_TTL_SECS`] /
/// [`CUSTOM_REFRESH_THRESHOLD_BPS`], which round out the same defensive profile.
const CUSTOM_OFFSET_BPS: u32 = 5;

/// Default order TTL for a custom corridor. Short, like the volatile presets, so
/// a resting quote can't sit stale long enough to be arbed on a fast move. Must
/// stay above the live-order deadline margin (30s) or orders never become
/// fillable — 60s clears it with headroom.
const CUSTOM_TTL_SECS: u64 = 60;

/// Default re-quote deadband for a custom corridor. Zero = re-post every tick, so
/// quotes track the feed rather than resting through a move. Matches the volatile
/// presets.
const CUSTOM_REFRESH_THRESHOLD_BPS: u32 = 0;

/// The smallest ladder slice, in whole debt tokens. Scaled to the debt token's
/// decimals when the template is rendered, so "10" means 10 USDT whether the
/// token is 6dp or 18dp.
const MIN_SLICE_WHOLE_DEBT: u128 = 10;

/// Decimals above this make the atomic min-slice math (`10 * 10^decimals`)
/// overflow `u128` and describe a token that doesn't exist anyway. ERC-20
/// decimals are a `u8`; real tokens sit at 6–18.
const MAX_TOKEN_DECIMALS: u8 = 36;

/// The minimum an operator has to type to stand a bot up on a pair the catalog
/// doesn't ship. Everything else is defaulted (see the module docs) and editable
/// later. Deserialized straight from the wizard's create request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomCorridor {
    pub chain_id: u64,
    pub rpc_url: String,
    /// `SETTLEMENT_V3_FILLER_REACTOR` on this chain. No default — a bot pointed at
    /// the wrong (or zero) reactor posts orders that can never be filled.
    pub reactor: String,
    /// The soft asset the bot buys low and sells high (e.g. cNGN).
    pub collateral: String,
    pub collateral_decimals: u8,
    /// The stable asset quoted against (e.g. USDT).
    pub debt: String,
    pub debt_decimals: u8,
    /// HTTP price source returning `{ price, timestamp }` (debt per collateral).
    pub feed_url: String,
    /// Override the canonical Permit2. Blank/absent uses [`CANONICAL_PERMIT2`].
    #[serde(default)]
    pub permit2: Option<String>,
    /// Override the default indexer. Blank/absent uses [`DEFAULT_INDEXER_URL`].
    #[serde(default)]
    pub indexer_url: Option<String>,
}

impl CustomCorridor {
    /// Render a `stitch.toml` from these inputs, or fail with the first bad
    /// field. The result is parsed back through [`Config::from_toml`] before it's
    /// returned, so a config this produces is one the bot can actually load.
    pub fn render(&self) -> Result<String> {
        ensure!(self.chain_id > 0, "chain id must be greater than zero");
        let rpc_url = require_http_url(&self.rpc_url, "RPC URL")?;
        let feed_url = require_http_url(&self.feed_url, "price feed URL")?;
        let custom_indexer = blank_to_none(self.indexer_url.as_deref());
        let on_hosted_indexer = custom_indexer.is_none();
        let indexer_url = match custom_indexer {
            Some(u) => require_http_url(u, "indexer URL")?,
            None => {
                // The hosted indexer only settles on chains it has a reactor for.
                // Standing a bot up on any other chain against it would create
                // fine and then fail every submission, so refuse here and point
                // the operator at their own indexer instead of a silent dead bot.
                // A custom indexer (the `Some` arm) is their call.
                ensure!(
                    INDEXER_SUPPORTED_CHAINS.contains(&self.chain_id),
                    "the hosted indexer doesn't settle on chain {}. Set your own indexer URL for \
                     this chain, or pick a chain it supports ({}).",
                    self.chain_id,
                    INDEXER_SUPPORTED_CHAINS
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                DEFAULT_INDEXER_URL.to_string()
            }
        };

        let reactor = require_nonzero_address(&self.reactor, "reactor address")?;
        let collateral = require_address(&self.collateral, "collateral token address")?;
        let debt = require_address(&self.debt, "debt token address")?;
        ensure!(
            !collateral.eq_ignore_ascii_case(&debt),
            "the collateral and debt tokens must be different addresses"
        );
        let permit2 = match blank_to_none(self.permit2.as_deref()) {
            Some(p) => {
                // The hosted indexer reconstructs the order digest with its own
                // fixed canonical Permit2, so a bot signing against a different
                // one would have every order rejected as "Signature does not match
                // maker." A Permit2 override only makes sense with a custom indexer
                // that uses the same address.
                ensure!(
                    !on_hosted_indexer,
                    "a custom Permit2 address only works with your own indexer — the hosted \
                     indexer signs against the canonical Permit2, so it would reject every order. \
                     Set your own indexer URL, or drop the Permit2 override."
                );
                require_nonzero_address(p, "Permit2 address")?
            }
            None => CANONICAL_PERMIT2.to_string(),
        };

        let collateral_decimals =
            require_decimals(self.collateral_decimals, "collateral decimals")?;
        let debt_decimals = require_decimals(self.debt_decimals, "debt decimals")?;
        let min_slice = min_slice_atomic(self.debt_decimals)?;

        let toml = render_template(&CustomTemplate {
            chain_id: self.chain_id,
            rpc_url: &rpc_url,
            indexer_url: &indexer_url,
            permit2: &permit2,
            reactor: &reactor,
            feed_url: &feed_url,
            collateral: &collateral,
            collateral_decimals,
            debt: &debt,
            debt_decimals,
            min_slice,
        });

        // The single guarantee callers rely on: what we return loads. Rendering
        // the fields into a template can't drift from what the bot parses,
        // because the same parser has to accept it here first.
        crate::config::Config::from_toml(&toml)
            .context("the custom corridor details produced a config the bot can't load")?;
        Ok(toml)
    }
}

/// Everything the template needs, already validated and normalized.
struct CustomTemplate<'a> {
    chain_id: u64,
    rpc_url: &'a str,
    indexer_url: &'a str,
    permit2: &'a str,
    reactor: &'a str,
    feed_url: &'a str,
    collateral: &'a str,
    collateral_decimals: u8,
    debt: &'a str,
    debt_decimals: u8,
    min_slice: u128,
}

/// The `stitch.toml` body. Deliberately mirrors the shipped presets in
/// `src/setup/templates/` — same comments, same defaults — so a custom bot reads
/// the same as a catalog one and every value has an obvious home in Settings.
fn render_template(t: &CustomTemplate) -> String {
    format!(
        "# Textile Stitch — custom corridor on chain {chain_id}.
# Generated by the panel's \"Custom corridor\" form. Everything here is editable
# from the bot's Settings (or Tools -> Edit raw config). The wallet key is NOT
# here; it lives in stitch.key / the environment.

chain_id        = {chain_id}
rpc_url         = \"{rpc_url}\"
indexer_url     = \"{indexer_url}\"
permit2         = \"{permit2}\"  # canonical Permit2 (same on every chain)
reactor         = \"{reactor}\"  # SETTLEMENT_V3_FILLER_REACTOR on this chain
tick_interval_secs = 5

# Price source. Returns {{ price, timestamp }} where price is debt-per-collateral.
[feed]
url            = \"{feed_url}\"
staleness_secs = 900

[[pools]]
collateral = \"{collateral}\"  # soft asset (bought on the bid, sold on the ask)
collateral_decimals = {collateral_decimals}
debt = \"{debt}\"  # stable asset
debt_decimals = {debt_decimals}

# Conservative defaults for an unknown pair: wide spread, short TTL, re-post
# every tick — a resting quote can't be arbed on a fast move. Tighten for a
# stable pair in Settings.
# Buy low: bid below the mid, paying the debt token for the collateral token.
buy_offset_bps = {offset_bps}
buy_total_liquidity_debt = \"max\"
buy_min_slice_debt = \"{min_slice}\"
buy_max_orders = 40

# Sell high: ask above the mid, selling the collateral token for the debt token.
sell_offset_bps = {offset_bps}
sell_total_liquidity_collateral = \"max\"
sell_min_slice_debt = \"{min_slice}\"
sell_max_orders = 40

ttl_secs = {ttl_secs}
refresh_threshold_bps = {refresh_threshold_bps}
",
        chain_id = t.chain_id,
        rpc_url = t.rpc_url,
        indexer_url = t.indexer_url,
        permit2 = t.permit2,
        reactor = t.reactor,
        feed_url = t.feed_url,
        offset_bps = CUSTOM_OFFSET_BPS,
        ttl_secs = CUSTOM_TTL_SECS,
        refresh_threshold_bps = CUSTOM_REFRESH_THRESHOLD_BPS,
        collateral = t.collateral,
        collateral_decimals = t.collateral_decimals,
        debt = t.debt,
        debt_decimals = t.debt_decimals,
        min_slice = t.min_slice,
    )
}

/// The smallest ladder slice in atomic debt units: `10 * 10^decimals`. Scaling
/// it to the token keeps the "10 whole tokens" minimum honest regardless of
/// decimals — a fixed literal would be a dust order at 18dp and an enormous one
/// at 2dp.
fn min_slice_atomic(debt_decimals: u8) -> Result<u128> {
    10u128
        .checked_pow(debt_decimals as u32)
        .and_then(|unit| unit.checked_mul(MIN_SLICE_WHOLE_DEBT))
        .with_context(|| format!("debt decimals ({debt_decimals}) is too large"))
}

fn require_decimals(decimals: u8, field: &str) -> Result<u8> {
    ensure!(
        decimals <= MAX_TOKEN_DECIMALS,
        "{field} ({decimals}) is too large; real tokens are 6-18"
    );
    Ok(decimals)
}

/// Parse an EVM address, returning it checksummed so the written config is
/// canonical whatever case the operator pasted.
fn require_address(value: &str, field: &str) -> Result<String> {
    let addr = value
        .trim()
        .parse::<Address>()
        .with_context(|| format!("{field} is not a valid 0x-prefixed address"))?;
    Ok(addr.to_checksum(None))
}

/// As [`require_address`], but rejects the zero address — used for the reactor
/// and Permit2, where a zero placeholder means "no contract" and would make the
/// bot quote into nothing while looking healthy.
fn require_nonzero_address(value: &str, field: &str) -> Result<String> {
    let addr = value
        .trim()
        .parse::<Address>()
        .with_context(|| format!("{field} is not a valid 0x-prefixed address"))?;
    ensure!(addr != Address::ZERO, "{field} can't be the zero address");
    Ok(addr.to_checksum(None))
}

/// Validate an http(s) URL with a host, matching the Settings screen's rule so a
/// corridor stood up here can't reach a state the editor would refuse.
fn require_http_url(value: &str, field: &str) -> Result<String> {
    let v = value.trim();
    ensure!(!v.is_empty(), "{field} is required");
    let parsed = url::Url::parse(v)
        .with_context(|| format!("{field} must be a valid URL (like https://…)"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "{field} must be an http(s) URL (like https://…)"
    );
    ensure!(
        parsed.host_str().is_some_and(|h| !h.is_empty()),
        "{field} must include a host (like https://api.example.com)"
    );
    Ok(v.to_string())
}

fn blank_to_none(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A supported chain (so the default indexer accepts it) but deliberately NOT
    // a catalog pair — made-up tokens, so `identify_corridor` can't match it to a
    // shipped preset and the "renders as a custom corridor" assertion means
    // something.
    fn valid() -> CustomCorridor {
        CustomCorridor {
            chain_id: 42220, // Celo, indexer-supported
            rpc_url: "https://rpc.example.com".to_string(),
            reactor: "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3".to_string(),
            collateral: "0x1111111111111111111111111111111111111111".to_string(),
            collateral_decimals: 6,
            debt: "0x2222222222222222222222222222222222222222".to_string(),
            debt_decimals: 6,
            feed_url: "https://feed.example.com/price".to_string(),
            permit2: None,
            indexer_url: None,
        }
    }

    #[test]
    fn a_valid_custom_corridor_renders_a_config_the_bot_can_load() {
        let toml = valid().render().expect("valid input renders");
        let cfg = crate::config::Config::from_toml(&toml).expect("and the bot loads it");
        assert_eq!(cfg.chain_id, 42220);
        assert_eq!(cfg.pools.len(), 1);
        // The catalog can't identify a custom corridor (it's not shipped), which
        // is exactly why the panel shows it as "Custom corridor" rather than
        // mislabeling it as a preset.
        assert!(crate::setup::identify_corridor(&toml).is_none());
    }

    #[test]
    fn the_defaults_fill_in_permit2_and_the_indexer() {
        let toml = valid().render().unwrap();
        assert!(
            toml.contains(CANONICAL_PERMIT2),
            "canonical Permit2 defaulted"
        );
        assert!(toml.contains(DEFAULT_INDEXER_URL), "indexer defaulted");
    }

    #[test]
    fn the_spread_defaults_are_the_conservative_volatile_profile() {
        // A custom pair's volatility is unknown, so the defaults must be the wide
        // 5 bps / short-TTL / re-post-every-tick profile, not the tight 1 bp of an
        // FX pair — otherwise "max" liquidity rests at a hair-thin spread and a
        // fast move picks the operator off.
        let toml = valid().render().unwrap();
        let cfg = crate::config::Config::from_toml(&toml).unwrap();
        let pool = &cfg.pools[0];
        assert_eq!(pool.buy_offset_bps, Some(5), "{toml}");
        assert_eq!(pool.sell_offset_bps, Some(5), "{toml}");
        assert_eq!(pool.ttl_secs, 60, "{toml}");
        assert_eq!(pool.refresh_threshold_bps, 0, "{toml}");
    }

    #[test]
    fn a_chain_the_hosted_indexer_cannot_settle_is_refused() {
        // The hosted indexer rejects chains it has no reactor for, so a bot there
        // would create and then fail every submission. Refuse up front.
        let err = CustomCorridor {
            chain_id: 12345, // not in INDEXER_SUPPORTED_CHAINS
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("hosted indexer"), "{err}");
    }

    #[test]
    fn a_reactor_less_but_reachable_chain_is_refused_on_the_hosted_indexer() {
        // Arbitrum is reachable by the indexer's chain reader but has no hosted
        // SETTLEMENT_V3_FILLER_REACTOR, so `submitFillerOrder` would reject every
        // order. The allowlist keys on the reactor, not just reachability, so this
        // must be refused.
        for chain in [42161u64, 11155111, 84532, 11142220] {
            let err = CustomCorridor {
                chain_id: chain,
                ..valid()
            }
            .render()
            .unwrap_err();
            assert!(
                err.to_string().contains("hosted indexer"),
                "chain {chain}: {err}"
            );
        }
    }

    #[test]
    fn a_permit2_override_is_refused_with_the_hosted_indexer() {
        // The hosted indexer rebuilds the digest with the canonical Permit2, so
        // signing against a different one gets every order rejected as a signature
        // mismatch. A Permit2 override needs a matching custom indexer.
        let err = CustomCorridor {
            permit2: Some("0x1111111111111111111111111111111111111111".to_string()),
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("Permit2"), "{err}");

        // With a custom indexer it's allowed and written through.
        let toml = CustomCorridor {
            permit2: Some("0x000000000022D473030F116dDEE9F6B43aC78BA3".to_string()),
            indexer_url: Some("https://my-indexer.example.com".to_string()),
            ..valid()
        }
        .render()
        .expect("a custom indexer lifts the Permit2 restriction");
        assert!(toml.contains("https://my-indexer.example.com"), "{toml}");
    }

    #[test]
    fn an_unsupported_chain_is_allowed_with_a_custom_indexer() {
        // The escape hatch: point the bot at your own indexer and any chain is
        // your call, not ours.
        let toml = CustomCorridor {
            chain_id: 12345,
            indexer_url: Some("https://my-indexer.example.com".to_string()),
            ..valid()
        }
        .render()
        .expect("a custom indexer lifts the chain restriction");
        assert!(toml.contains("https://my-indexer.example.com"), "{toml}");
    }

    #[test]
    fn the_min_slice_scales_to_the_debt_decimals() {
        // 6dp → 10 * 10^6.
        let toml = valid().render().unwrap();
        assert!(toml.contains("\"10000000\""), "10 USDT at 6dp: {toml}");
        // 18dp → 10 * 10^18, so the "10 whole tokens" floor holds either way.
        let toml = CustomCorridor {
            debt_decimals: 18,
            ..valid()
        }
        .render()
        .unwrap();
        assert!(
            toml.contains("\"10000000000000000000\""),
            "10 tokens at 18dp: {toml}"
        );
    }

    #[test]
    fn a_pasted_lowercase_address_is_written_checksummed() {
        let toml = CustomCorridor {
            reactor: "0xa9aa0a64769cbed4d3b1ceb4df01cde915c235b3".to_string(),
            ..valid()
        }
        .render()
        .unwrap();
        assert!(
            toml.contains("0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"),
            "reactor re-checksummed: {toml}"
        );
    }

    #[test]
    fn a_zero_reactor_is_refused() {
        let err = CustomCorridor {
            reactor: "0x0000000000000000000000000000000000000000".to_string(),
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("reactor"), "{err}");
    }

    #[test]
    fn a_garbage_token_address_is_refused_by_field_name() {
        let err = CustomCorridor {
            collateral: "not-an-address".to_string(),
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(
            err.to_string().contains("collateral token address"),
            "{err}"
        );
    }

    #[test]
    fn the_same_token_on_both_sides_is_refused() {
        let err = CustomCorridor {
            debt: valid().collateral.clone(),
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("different addresses"), "{err}");
    }

    #[test]
    fn a_non_http_rpc_is_refused() {
        let err = CustomCorridor {
            rpc_url: "wss://forno.celo.org".to_string(),
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("RPC URL"), "{err}");
    }

    #[test]
    fn a_blank_feed_is_refused() {
        let err = CustomCorridor {
            feed_url: "   ".to_string(),
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("price feed URL"), "{err}");
    }

    #[test]
    fn absurd_decimals_are_refused_before_the_slice_math_overflows() {
        let err = CustomCorridor {
            debt_decimals: 200,
            ..valid()
        }
        .render()
        .unwrap_err();
        assert!(err.to_string().contains("debt decimals"), "{err}");
    }
}
