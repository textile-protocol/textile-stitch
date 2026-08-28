// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Maker enrollment: the wire protocol behind "Connect this bot to Textile".
//!
//! The bot proves it controls its funding wallet by signing a `MakerEnroll`
//! EIP-712 digest, POSTs that to `/v2/maker/enroll`, and the venue returns a
//! maker id, a stream URL, and an API key. The key is written to `rfq-api.key`
//! beside the config; it never goes into the TOML.
//!
//! Lives outside `panel` because there are two front doors — the panel's
//! Settings → Connect button and `stitch connect` on the CLI — and a maker that
//! enrolled one way must be indistinguishable from one that enrolled the other.
//! The panel adds its own concerns on top (config locking, layout checks, file
//! ownership for the container uid); everything here is common to both.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::hex;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::eip712::{maker_enroll_digest, maker_enroll_environment};
use crate::setup;
use crate::signer::DynSigner;

/// How long to wait on the venue before giving up. Enroll is one round trip and
/// an operator is watching, so this is short enough to fail visibly.
const ENROLL_TIMEOUT_SECS: u64 = 20;

/// Token metadata for a seated corridor. Custom (non-catalog) bots match on
/// these instead of `identify_corridor`, which is `None` for them.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EnrollCorridorPair {
    pub slug: String,
    pub collateral_token: String,
    pub debt_token: String,
}

/// What the venue returns from `/v2/maker/enroll`.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EnrollResponse {
    pub maker_id: String,
    pub maker_slug: String,
    pub environment: String,
    pub api_key: String,
    pub stream_url: String,
    #[serde(default)]
    pub validation_contract: Option<String>,
    #[serde(default)]
    pub corridors: Vec<String>,
    #[serde(default)]
    pub corridor_pairs: Vec<EnrollCorridorPair>,
    #[serde(default)]
    pub flagged: bool,
}

/// HTTP origin for the venue's maker routes, from a stream URL or an API base.
///
/// Every maker route hangs off one origin, so callers derive the origin once
/// and append their own path — otherwise each new endpoint needs its own
/// scheme-rewriting and suffix-stripping copy.
pub fn maker_venue_origin(stream_or_origin: &str) -> String {
    let trimmed = stream_or_origin.trim();
    let http = if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    };
    let http = http.trim_end_matches('/');
    for suffix in [
        "/v2/maker/stream",
        "/v2/maker/enroll",
        "/v2/maker/access-request",
        "/v2/maker/access-status",
    ] {
        if let Some(base) = http.strip_suffix(suffix) {
            return base.to_string();
        }
    }
    http.to_string()
}

/// Derive `https://host/v2/maker/enroll` from a stream URL or an API origin.
pub fn maker_enroll_url(stream_or_origin: &str) -> String {
    format!("{}/v2/maker/enroll", maker_venue_origin(stream_or_origin))
}

/// Where the panel asks Textile to seat this maker.
pub fn maker_access_request_url(stream_or_origin: &str) -> String {
    format!(
        "{}/v2/maker/access-request",
        maker_venue_origin(stream_or_origin)
    )
}

/// Where a caller polls for the seating decision.
pub fn maker_access_status_url(stream_or_origin: &str) -> String {
    format!(
        "{}/v2/maker/access-status",
        maker_venue_origin(stream_or_origin)
    )
}

/// The venue origin for a config: an explicit override, else the configured
/// `[rfq].url`, else the indexer origin.
pub fn venue_origin_from_config(cfg: &Config, override_url: Option<&str>) -> String {
    if let Some(url) = override_url.map(str::trim).filter(|u| !u.is_empty()) {
        return maker_venue_origin(url);
    }
    if let Some(rfq) = &cfg.rfq {
        if !rfq.url.trim().is_empty() {
            return maker_venue_origin(&rfq.url);
        }
    }
    maker_venue_origin(&cfg.indexer_url)
}

/// The enroll endpoint for a config.
pub fn enroll_url_from_config(cfg: &Config, override_url: Option<&str>) -> String {
    format!(
        "{}/v2/maker/enroll",
        venue_origin_from_config(cfg, override_url)
    )
}

/// Sign `MakerEnroll` with the bot's own wallet and register with the venue.
///
/// The signature is over the wallet the bot trades from, which is what ties the
/// maker id to it — so this deliberately takes the same `DynSigner` the bot
/// runs with rather than any key an operator might paste.
pub async fn register_maker(
    cfg: &Config,
    signer: &DynSigner,
    venue: &str,
) -> Result<EnrollResponse> {
    let address = signer.address();
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("reading the clock for the enroll timestamp")?
        .as_millis() as u64;
    let environment = maker_enroll_environment(cfg.chain_id);
    let digest = maker_enroll_digest(environment, address, cfg.chain_id, issued_at);
    let signature = signer
        .sign_digest(digest)
        .await
        .context("signing the enroll digest")?;

    let payload = json!({
        "chainId": cfg.chain_id,
        "signingAddress": format!("{address:?}"),
        "issuedAt": issued_at,
        "signature": hex::encode_prefixed(signature),
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(ENROLL_TIMEOUT_SECS))
        .build()
        .context("building the enroll HTTP client")?;
    let response = client
        .post(venue)
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("could not reach Textile enroll at {venue}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("Textile enroll returned an unreadable body")?;
    if !status.is_success() {
        match venue_error_message(&text) {
            Some(message) => bail!("{message}"),
            None => bail!("Textile enroll failed ({status})"),
        }
    }
    let enrolled: EnrollResponse =
        serde_json::from_str(&text).context("Textile enroll returned an unexpected body")?;
    if enrolled.api_key.trim().is_empty() || enrolled.maker_id.trim().is_empty() {
        bail!("Textile enroll did not return a maker id and key");
    }
    Ok(enrolled)
}

/// What an enrollment did to the config, so a caller can say so in its own
/// voice — a JSON message for the panel, a printed line for the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollOutcome {
    /// Live on a corridor: RFQ on, and the ladder off for an RFQ-default bot.
    Live,
    /// Registered, but Textile flagged this maker. No quotes will arrive.
    Flagged,
    /// Registered with no corridor seated on this chain yet. RFQ stays off.
    Waiting,
}

/// Fold an enrollment into a config: pick the corridor, write `[rfq]`, and
/// return the edited TOML alongside what happened.
///
/// `rfq_default` decides whether going live also turns the public ladder off.
pub fn apply_enrollment(
    current_toml: &str,
    cfg: &Config,
    enrolled: &EnrollResponse,
    rfq_default: bool,
) -> Result<(String, EnrollOutcome)> {
    // Every configured pool, not just the first: the responder builds a book per
    // pool and binds them all, so a multi-pool bot whose second pair is the
    // seated one should go live on that pair rather than report Waiting.
    let seats: Vec<Option<String>> = cfg
        .pools
        .iter()
        .map(|p| {
            let configured = setup::pool_identity(cfg.chain_id, p).map(|c| c.id);
            pick_enroll_corridor(
                enrolled.flagged,
                // The fallback is per *pool*, from that pool's own identity. A
                // legacy enroll response carries `corridors` with no
                // `corridorPairs`, so the slug has to come from somewhere else —
                // but keyed on the bot (`identify_corridor` reads pool 0) it
                // would either stamp pool 0's slug onto unrelated pairs, whose
                // books then collide under one corridor id, or leave a bot whose
                // *second* pool is the catalog one stuck at Waiting. Keyed on
                // the pool, a pool gets a slug only if it is that corridor.
                //
                // `pool_identity`, not `identify_pair`: a corridor listed after
                // this release has no catalog entry, and against a legacy venue
                // that pool would never be seated at all.
                configured.as_deref(),
                Some((p.collateral.as_str(), p.debt.as_str())),
                &enrolled.corridors,
                &enrolled.corridor_pairs,
            )
        })
        .collect();
    // Live means the responder will actually answer, which is the same two
    // conditions `Config::rfq_quotable` applies, because both decide that one
    // question:
    //
    //   * *every* pool buildable — `rfq::build_runtime` collects
    //     `book_from_pool` over all of them, so one malformed sibling aborts
    //     the responder even when the seated pool is fine, and
    //   * a seat on a pool with usable capacity, or the pair we go live on
    //     publishes no levels. Session binding drops the sibling that *did*
    //     have capacity, because it has no slug.
    //
    // Either half missing and going Live enables RFQ, takes the bot-wide
    // ladder down, and leaves a bot quoting on neither surface.
    let all_buildable = cfg.pools.iter().all(|p| p.rfq_book_buildable());
    let waiting = !all_buildable
        || !seats.iter().enumerate().any(|(index, seat)| {
            seat.is_some()
                && cfg
                    .pools
                    .get(index)
                    .is_some_and(|p| p.rfq_has_usable_capacity())
        });
    let outcome = match (enrolled.flagged, waiting) {
        (true, _) => EnrollOutcome::Flagged,
        (false, true) => EnrollOutcome::Waiting,
        (false, false) => EnrollOutcome::Live,
    };

    // Stamp each pool with its own slug. `rfq_connect_patch` also carries the
    // bot-wide `[rfq]` fields; re-applying those per pool is idempotent, and a
    // pool with no seat keeps an empty slug so it simply never matches a
    // request. With no pools at all there is still the bot-wide block to write.
    let mut edited = current_toml.to_string();
    for (index, seat) in seats.iter().enumerate() {
        let current =
            setup::read_settings_at(&edited, index).map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let patch = setup::rfq_connect_patch(
            &current,
            enrolled.stream_url.clone(),
            enrolled.maker_id.clone(),
            enrolled.validation_contract.clone().unwrap_or_default(),
            seat.clone().unwrap_or_default(),
            rfq_default,
            !waiting,
        );
        edited = setup::apply_settings(&edited, &patch)?;
    }
    if seats.is_empty() {
        let current = setup::read_settings_at(&edited, 0).map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let patch = setup::rfq_connect_patch(
            &current,
            enrolled.stream_url.clone(),
            enrolled.maker_id.clone(),
            enrolled.validation_contract.clone().unwrap_or_default(),
            String::new(),
            rfq_default,
            false,
        );
        edited = setup::apply_settings(&edited, &patch)?;
    }
    Ok((edited, outcome))
}

/// Which corridor slug this bot should quote, if any.
///
/// A flagged maker gets none. Otherwise prefer an exact token match against the
/// seats the venue returned (this is what makes custom, non-catalog pools work),
/// then fall back to the configured catalog corridor when the venue seated it.
/// Never invents a slug the venue did not name.
pub fn pick_enroll_corridor(
    flagged: bool,
    configured: Option<&str>,
    pool: Option<(&str, &str)>,
    corridors: &[String],
    pairs: &[EnrollCorridorPair],
) -> Option<String> {
    if flagged {
        return None;
    }
    if let Some((collateral, debt)) = pool {
        if let Some(pair) = pairs
            .iter()
            .find(|p| tokens_match(collateral, debt, &p.collateral_token, &p.debt_token))
        {
            return Some(pair.slug.clone());
        }
    }
    configured.and_then(|id| corridors.iter().find(|c| c.as_str() == id).cloned())
}

fn tokens_match(coll_a: &str, debt_a: &str, coll_b: &str, debt_b: &str) -> bool {
    (eq_addr(coll_a, coll_b) && eq_addr(debt_a, debt_b))
        || (eq_addr(coll_a, debt_b) && eq_addr(debt_a, coll_b))
}

fn eq_addr(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Pull the human-readable message out of a venue error body, whatever shape it
/// arrived in. `None` when the body isn't JSON or carries no message.
pub fn venue_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(slug: &str, collateral: &str, debt: &str) -> EnrollCorridorPair {
        EnrollCorridorPair {
            slug: slug.into(),
            collateral_token: collateral.into(),
            debt_token: debt.into(),
        }
    }

    #[test]
    fn pick_enroll_corridor_requires_the_configured_pair() {
        let empty: &[EnrollCorridorPair] = &[];
        assert_eq!(
            pick_enroll_corridor(
                false,
                Some("cngn-usdt-bsc"),
                None,
                &["cngn-usdt-bsc".into()],
                empty,
            ),
            Some("cngn-usdt-bsc".into())
        );
        assert_eq!(
            pick_enroll_corridor(
                false,
                Some("cngn-usdt-bsc"),
                None,
                &["wars-usdt-bsc".into()],
                empty,
            ),
            None
        );
        assert_eq!(
            pick_enroll_corridor(
                true,
                Some("cngn-usdt-bsc"),
                None,
                &["cngn-usdt-bsc".into()],
                empty,
            ),
            None
        );
        assert_eq!(
            pick_enroll_corridor(false, None, None, &["cngn-usdt-bsc".into()], empty),
            None
        );
    }

    /// A two-pool config where only the *second* pair is seated.
    const TWO_POOLS: &str = r#"
        chain_id = 56
        rpc_url = "http://x"
        indexer_url = "https://api.example"
        permit2 = "0x0000000000000000000000000000000000000000"
        reactor = "0x0000000000000000000000000000000000000000"
        tick_interval_secs = 5
        book_enabled = false
        [feed]
        url = "https://x"
        staleness_secs = 30
        [[pools]]
        collateral = "0x0000000000000000000000000000000000000001"
        collateral_decimals = 6
        debt = "0x0000000000000000000000000000000000000002"
        debt_decimals = 6
        buy_offset_bps = 1
        buy_order_size_debt = "1000000000"
        ttl_secs = 120
        refresh_threshold_bps = 10
        [[pools]]
        collateral = "0x0000000000000000000000000000000000000003"
        collateral_decimals = 6
        debt = "0x0000000000000000000000000000000000000004"
        debt_decimals = 6
        buy_offset_bps = 1
        buy_order_size_debt = "1000000000"
        ttl_secs = 120
        refresh_threshold_bps = 10
    "#;

    fn enrolled_with(pairs: Vec<EnrollCorridorPair>) -> EnrollResponse {
        EnrollResponse {
            maker_id: "mk_test".into(),
            maker_slug: "acme".into(),
            environment: "LIVE".into(),
            api_key: "tx_live_secret".into(),
            stream_url: "wss://api.example/v2/maker/stream".into(),
            validation_contract: Some("0x00000000000000000000000000000000000000aa".into()),
            corridors: pairs.iter().map(|p| p.slug.clone()).collect(),
            corridor_pairs: pairs,
            flagged: false,
        }
    }

    #[test]
    fn a_seat_on_any_pool_goes_live_and_lands_on_that_pool() {
        // The responder builds a book per pool and binds them all, so a bot
        // whose *second* pair is the seated one must go live on it rather than
        // report Waiting off the back of pool 1.
        let cfg = Config::from_toml(TWO_POOLS).expect("two-pool config parses");
        let enrolled = enrolled_with(vec![EnrollCorridorPair {
            slug: "second-pair".into(),
            collateral_token: "0x0000000000000000000000000000000000000003".into(),
            debt_token: "0x0000000000000000000000000000000000000004".into(),
        }]);

        let (edited, outcome) = apply_enrollment(TWO_POOLS, &cfg, &enrolled, true).unwrap();
        assert_eq!(outcome, EnrollOutcome::Live);

        let back = Config::from_toml(&edited).expect("edited config parses");
        assert_eq!(back.pools[0].rfq_corridor.as_deref().unwrap_or(""), "");
        assert_eq!(
            back.pools[1].rfq_corridor.as_deref(),
            Some("second-pair"),
            "the slug belongs to the pool that actually matched"
        );
        assert!(back.rfq_active(), "RFQ goes on for the seated pool");
    }

    #[test]
    fn the_catalog_fallback_never_leaks_onto_another_pool() {
        // Legacy enroll shape: `corridors` only, no `corridorPairs`. The
        // catalog id describes the first pool (that is what `identify_corridor`
        // matches on), so offering it to pool 2 would publish two books with
        // different tokens under one corridor id.
        let cfg = Config::from_toml(TWO_POOLS).unwrap();
        let mut enrolled = enrolled_with(vec![]);
        enrolled.corridors = vec!["cngn-usdt-bsc".into()];

        let (edited, _) = apply_enrollment(TWO_POOLS, &cfg, &enrolled, true).unwrap();
        let back = Config::from_toml(&edited).unwrap();
        assert_eq!(
            back.pools[1].rfq_corridor.as_deref().unwrap_or(""),
            "",
            "the second pool must not inherit the first pool's catalog slug"
        );
    }

    #[test]
    fn a_legacy_response_seats_a_catalog_pair_on_any_pool() {
        // Legacy shape again — `corridors` only — but here the *second* pool is
        // the catalog one. Keyed on the bot, the fallback reads pool 0 and this
        // bot sits at Waiting forever with a seated, quotable pair configured.
        // Keyed on the pair, pool 1 gets the slug it actually is.
        let bsc = TWO_POOLS
            .replace("book_enabled = false\n", "")
            .replace(
                "collateral = \"0x0000000000000000000000000000000000000003\"",
                "collateral = \"0xa8AEA66B361a8d53e8865c62D142167Af28Af058\"",
            )
            .replace(
                "debt = \"0x0000000000000000000000000000000000000004\"",
                "debt = \"0x55d398326f99059fF775485246999027B3197955\"",
            );
        let cfg = Config::from_toml(&bsc).expect("config parses");
        let mut enrolled = enrolled_with(vec![]);
        enrolled.corridors = vec!["cngn-usdt-bsc".into()];

        let (edited, outcome) = apply_enrollment(&bsc, &cfg, &enrolled, true).unwrap();
        assert_eq!(outcome, EnrollOutcome::Live);
        let back = Config::from_toml(&edited).unwrap();
        assert_eq!(
            back.pools[0].rfq_corridor.as_deref().unwrap_or(""),
            "",
            "pool 0 is not that corridor and must not be stamped with it"
        );
        assert_eq!(
            back.pools[1].rfq_corridor.as_deref(),
            Some("cngn-usdt-bsc"),
            "the catalog slug lands on the pool whose pair it describes"
        );
    }

    #[test]
    fn a_seat_on_a_pool_that_cannot_quote_still_waits() {
        // The seated pool has a slug but no usable capacity, while the sibling
        // that *can* quote has no seat. Going live here would enable RFQ, take
        // the bot-wide ladder down, and publish nothing.
        let no_capacity = TWO_POOLS.replace(
            concat!(
                "        collateral = \"0x0000000000000000000000000000000000000003\"\n",
                "        collateral_decimals = 6\n",
                "        debt = \"0x0000000000000000000000000000000000000004\"\n",
                "        debt_decimals = 6\n",
                "        buy_offset_bps = 1\n",
                "        buy_order_size_debt = \"1000000000\"\n"
            ),
            concat!(
                "        collateral = \"0x0000000000000000000000000000000000000003\"\n",
                "        collateral_decimals = 6\n",
                "        debt = \"0x0000000000000000000000000000000000000004\"\n",
                "        debt_decimals = 6\n"
            ),
        );
        // Ladder on, so "left alone" is observable rather than vacuous.
        let no_capacity = no_capacity.replace("book_enabled = false\n", "");
        let cfg = Config::from_toml(&no_capacity).expect("config parses");
        assert!(
            !cfg.pools[1].rfq_has_usable_capacity(),
            "pool 2 cannot quote"
        );
        assert!(cfg.pools[0].rfq_has_usable_capacity(), "pool 1 can");

        // Only the capacity-less pool is seated.
        let enrolled = enrolled_with(vec![EnrollCorridorPair {
            slug: "second-pair".into(),
            collateral_token: "0x0000000000000000000000000000000000000003".into(),
            debt_token: "0x0000000000000000000000000000000000000004".into(),
        }]);

        let (edited, outcome) = apply_enrollment(&no_capacity, &cfg, &enrolled, true).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::Waiting,
            "a seat the pool cannot use is not a reason to go live"
        );
        let back = Config::from_toml(&edited).unwrap();
        assert!(!back.rfq_active(), "RFQ stays off");
        assert!(back.book_enabled, "and the ladder is left alone");
    }

    #[test]
    fn a_malformed_sibling_pool_keeps_the_whole_bot_waiting() {
        // `rfq::build_runtime` collects `book_from_pool` over every pool with
        // `?`, so one pool whose token address does not parse aborts the
        // responder even though the seated pool is perfectly fine. Calling that
        // Live would enable RFQ, take the bot-wide ladder down, and leave the
        // bot quoting on neither surface — the same all-pools condition
        // `Config::rfq_quotable` applies.
        let broken = TWO_POOLS.replace("book_enabled = false\n", "").replace(
            "debt = \"0x0000000000000000000000000000000000000004\"",
            "debt = \"not-an-address\"",
        );
        let cfg = Config::from_toml(&broken).expect("config parses");
        assert!(cfg.pools[0].rfq_book_buildable() && cfg.pools[0].rfq_has_usable_capacity());
        assert!(!cfg.pools[1].rfq_book_buildable(), "the sibling is broken");

        // Pool 1 — the good one — is the seated pair.
        let enrolled = enrolled_with(vec![EnrollCorridorPair {
            slug: "first-pair".into(),
            collateral_token: "0x0000000000000000000000000000000000000001".into(),
            debt_token: "0x0000000000000000000000000000000000000002".into(),
        }]);

        let (edited, outcome) = apply_enrollment(&broken, &cfg, &enrolled, true).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::Waiting,
            "a responder that cannot start is not a live enrollment"
        );
        let back = Config::from_toml(&edited).unwrap();
        assert!(!back.rfq_active(), "RFQ stays off");
        assert!(back.book_enabled, "and the ladder is left alone");
    }

    #[test]
    fn no_seat_on_any_pool_still_waits() {
        let cfg = Config::from_toml(TWO_POOLS).unwrap();
        let enrolled = enrolled_with(vec![EnrollCorridorPair {
            slug: "unrelated".into(),
            collateral_token: "0x00000000000000000000000000000000000000ff".into(),
            debt_token: "0x00000000000000000000000000000000000000ee".into(),
        }]);

        let (edited, outcome) = apply_enrollment(TWO_POOLS, &cfg, &enrolled, true).unwrap();
        assert_eq!(outcome, EnrollOutcome::Waiting);
        let back = Config::from_toml(&edited).unwrap();
        assert!(!back.rfq_active(), "nothing seated means RFQ stays off");
    }

    #[test]
    fn pick_enroll_corridor_matches_custom_tokens() {
        let seats = [pair(
            "ops-custom-bsc",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )];
        assert_eq!(
            pick_enroll_corridor(
                false,
                None,
                Some((
                    "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa",
                    "0xBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBbBb",
                )),
                &["ops-custom-bsc".into()],
                &seats,
            ),
            Some("ops-custom-bsc".into())
        );
        assert_eq!(
            pick_enroll_corridor(
                false,
                None,
                Some((
                    "0xcccccccccccccccccccccccccccccccccccccccc",
                    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )),
                &["ops-custom-bsc".into()],
                &seats,
            ),
            None
        );
        assert_eq!(
            pick_enroll_corridor(
                true,
                None,
                Some((
                    "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )),
                &["ops-custom-bsc".into()],
                &seats,
            ),
            None
        );
    }

    #[test]
    fn enroll_url_from_stream_and_origin() {
        assert_eq!(
            maker_enroll_url("wss://api.textilecredit.com/v2/maker/stream"),
            "https://api.textilecredit.com/v2/maker/enroll"
        );
        assert_eq!(
            maker_enroll_url("ws://localhost:10000/v2/maker/stream"),
            "http://localhost:10000/v2/maker/enroll"
        );
        assert_eq!(
            maker_enroll_url("https://api.textilecredit.com"),
            "https://api.textilecredit.com/v2/maker/enroll"
        );
        assert_eq!(
            maker_enroll_url("https://api.textilecredit.com/v2/maker/enroll"),
            "https://api.textilecredit.com/v2/maker/enroll"
        );
    }
}
