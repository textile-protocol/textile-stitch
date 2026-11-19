// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Reading a shared OTC desk feed (`/price?otc=<key>`).
//!
//! A desk feed with desk wallets carries a `desk` object on every answer, a
//! 200 or a 503: who the desk's wallets are, which pair and chain the desk
//! trades, what its virtual account holds and its per-trade cap. A 200 also
//! carries the desk's price, read as any other feed print. A 404 means the
//! feed is unknown or was removed. Pure parsing here; the RFQ responder polls
//! and decides (see `crate::rfq::desk`).

use std::collections::BTreeSet;

use alloy_primitives::{Address, U256};
use serde::Deserialize;

use crate::pricing::feed::{check_session, parse_quote, PriceBasis, Quote};
use crate::pricing::tick::feed_age_secs;

/// The feed's verdict on the desk's price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskStatus {
    /// The desk's bid and ask are current and may be quoted.
    Live,
    Stale,
    Unset,
    Disabled,
    /// A status this build doesn't know. Never quoted, like the three above.
    Other,
}

impl DeskStatus {
    fn parse(raw: &str) -> Self {
        match raw {
            "live" => Self::Live,
            "stale" => Self::Stale,
            "unset" => Self::Unset,
            "disabled" => Self::Disabled,
            _ => Self::Other,
        }
    }
}

/// The `desk` object, parsed. Amounts are atomic units of the pair's tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskInfo {
    pub status: DeskStatus,
    pub chain_id: u64,
    pub collateral: Address,
    pub debt: Address,
    /// The desk's wallets as the feed lists them.
    pub takers: BTreeSet<Address>,
    /// What the desk's virtual account holds in collateral. Never negative:
    /// the feed reads an overdrawn side as zero.
    pub account_collateral: U256,
    /// What it holds in debt.
    pub account_debt: U256,
    /// The per-trade cap on the debt leg.
    pub max_order_debt: U256,
}

/// What one answer said about the desk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskReading {
    /// The feed is shared and carries its desk.
    Shared(DeskInfo),
    /// A 404: the feed is unknown or was removed.
    Gone,
}

/// One fetch, classified. Built once a second per desk feed, so the size
/// gap between the variants costs nothing worth boxing for.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum DeskFetch {
    /// The feed answered as a desk feed does. `quote` is the price on a 200
    /// that parses as a feed print; anything else is the reason there is none.
    Answered {
        reading: DeskReading,
        quote: Result<Quote, String>,
    },
    /// Unreachable, an error that isn't the feed's own, or a body that doesn't
    /// parse. The last good reading stands, for a while.
    Failed(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDesk {
    status: String,
    chain_id: u64,
    collateral_token: String,
    debt_token: String,
    takers: Vec<String>,
    account: RawAccount,
    max_order_debt: String,
}

#[derive(Deserialize)]
struct RawAccount {
    collateral: String,
    debt: String,
}

/// The body's `desk`, when it has one. Strict: a desk object with any field
/// missing or malformed is an error, never a partial desk, since its numbers
/// cap what the bot pays.
pub fn parse_desk(body: &str) -> anyhow::Result<Option<DeskInfo>> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    let Some(raw) = value.get("desk").filter(|d| !d.is_null()) else {
        return Ok(None);
    };
    let raw: RawDesk = serde_json::from_value(raw.clone())?;
    let address = |s: &str, field: &str| -> anyhow::Result<Address> {
        s.trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("desk.{field} is not an address: {s:?}"))
    };
    Ok(Some(DeskInfo {
        status: DeskStatus::parse(&raw.status),
        chain_id: raw.chain_id,
        collateral: address(&raw.collateral_token, "collateralToken")?,
        debt: address(&raw.debt_token, "debtToken")?,
        takers: raw
            .takers
            .iter()
            .map(|t| address(t, "takers"))
            .collect::<anyhow::Result<_>>()?,
        account_collateral: atomic(&raw.account.collateral, "account.collateral")?,
        account_debt: atomic(&raw.account.debt, "account.debt")?,
        max_order_debt: atomic(&raw.max_order_debt, "maxOrderDebt")?,
    }))
}

/// A decimal string of atomic units. No sign, no hex, no exponent.
fn atomic(raw: &str, field: &str) -> anyhow::Result<U256> {
    anyhow::ensure!(
        !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()),
        "desk.{field} must be a decimal amount, got {raw:?}"
    );
    U256::from_str_radix(raw, 10).map_err(|_| anyhow::anyhow!("desk.{field} is out of range"))
}

/// Classify one HTTP answer. `asked_off_hours` is whether the URL asked for
/// off-hours prices, as [`check_session`] needs; `now` is Unix seconds.
///
/// - 404: [`DeskReading::Gone`], whatever the body.
/// - 200 or 503 with a desk: the desk, and on a 200 the print parsed exactly
///   as a pool feed's is.
/// - Anything else is [`DeskFetch::Failed`], a 200 without a desk or with one
///   that doesn't parse included. Only a 404 uncaps the desk's wallets: the
///   API books every fill with them on the desk's account, so a body that
///   merely lost its `desk` must not.
pub fn classify(
    status: u16,
    body: &str,
    basis: PriceBasis,
    asked_off_hours: bool,
    now: u64,
) -> DeskFetch {
    if status == 404 {
        return DeskFetch::Answered {
            reading: DeskReading::Gone,
            quote: Err("feed not found".into()),
        };
    }
    if status != 200 && status != 503 {
        return DeskFetch::Failed(format!("HTTP {status}"));
    }
    let desk = match parse_desk(body) {
        Ok(desk) => desk,
        Err(e) => return DeskFetch::Failed(format!("{e:#}")),
    };
    let Some(info) = desk else {
        return DeskFetch::Failed(format!("HTTP {status} without a desk"));
    };
    let reading = DeskReading::Shared(info);
    let quote = if status == 200 {
        print_of(body, basis, asked_off_hours, now).map_err(|e| format!("{e:#}"))
    } else {
        Err(format!("HTTP {status}"))
    };
    DeskFetch::Answered { reading, quote }
}

/// The checks [`crate::pricing::feed::HttpFeed`] applies to a 200, minus the
/// issuer spread gate a desk feed has no use for.
fn print_of(
    body: &str,
    basis: PriceBasis,
    asked_off_hours: bool,
    now: u64,
) -> anyhow::Result<Quote> {
    let quote = parse_quote(body, basis)?;
    anyhow::ensure!(
        feed_age_secs(quote.timestamp, now).is_some(),
        "feed timestamp must be nonzero Unix seconds at or before the bot clock"
    );
    check_session(&quote, asked_off_hours)?;
    Ok(quote)
}

/// GET the feed and hand back its status and body. Only a transport failure
/// is an error: a 503 or a 404 is an answer.
/// One GET of a desk feed. Transport errors drop the URL: reqwest prints it
/// in its errors, and a desk feed's URL carries its `otc` key.
pub async fn fetch(client: &reqwest::Client, url: &str) -> anyhow::Result<(u16, String)> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(reqwest::Error::without_url)?;
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[tokio::test]
    async fn a_failed_fetch_does_not_repeat_the_key() {
        // Nothing listens on port 1: the connection is refused at once.
        let err = fetch(
            &reqwest::Client::new(),
            "http://127.0.0.1:1/price?otc=otc-secret&pair=cngn-usdt",
        )
        .await
        .unwrap_err();
        let shown = format!("{err:#}");
        assert!(!shown.contains("otc-secret"), "{shown}");
    }

    fn desk_json(status: &str) -> String {
        format!(
            r#"{{"status":"{status}","chainId":56,
               "collateralToken":"0x00000000000000000000000000000000000000c1",
               "debtToken":"0x00000000000000000000000000000000000000d1",
               "takers":["0x00000000000000000000000000000000000000ab"],
               "account":{{"collateral":"140358240590000000000000000","debt":"18250000000000000000"}},
               "maxOrderDebt":"100000000000000000000000"}}"#
        )
    }

    fn live_body() -> String {
        format!(
            r#"{{"pair":"cngn-usdt","chainId":56,"price":0.00068,"bid":0.00067,"ask":0.00069,
               "timestamp":{},"source":"otc","otc":"tradepal","revision":3,"desk":{}}}"#,
            NOW - 5,
            desk_json("live")
        )
    }

    #[test]
    fn a_live_answer_carries_the_desk_and_the_print() {
        let DeskFetch::Answered { reading, quote } =
            classify(200, &live_body(), PriceBasis::Served, false, NOW)
        else {
            panic!("a 200 desk feed answers");
        };
        let DeskReading::Shared(info) = reading else {
            panic!("the desk is read");
        };
        assert_eq!(info.status, DeskStatus::Live);
        assert_eq!(info.chain_id, 56);
        assert_eq!(info.takers.len(), 1);
        assert_eq!(
            info.account_collateral,
            U256::from_str_radix("140358240590000000000000000", 10).unwrap()
        );
        assert_eq!(
            info.account_debt,
            U256::from(18_250_000_000_000_000_000u128)
        );
        assert_eq!(
            info.max_order_debt,
            U256::from(100_000_000_000_000_000_000_000u128)
        );
        let quote = quote.unwrap();
        assert_eq!(quote.sides(), Some((0.00067, 0.00069)));
    }

    #[test]
    fn a_503_carries_the_desk_and_no_price() {
        let body = format!(
            r#"{{"error":"stale","status":"stale","otc":"tradepal","revision":3,"desk":{}}}"#,
            desk_json("stale")
        );
        let DeskFetch::Answered { reading, quote } =
            classify(503, &body, PriceBasis::Served, false, NOW)
        else {
            panic!("a 503 with a desk is an answer");
        };
        assert!(matches!(reading, DeskReading::Shared(i) if i.status == DeskStatus::Stale));
        assert!(quote.is_err());
    }

    #[test]
    fn a_404_is_gone_and_a_200_without_a_desk_is_a_failed_read() {
        assert!(matches!(
            classify(404, "{}", PriceBasis::Served, false, NOW),
            DeskFetch::Answered {
                reading: DeskReading::Gone,
                ..
            }
        ));
        let bare = format!(r#"{{"price":1.0,"timestamp":{}}}"#, NOW - 1);
        assert!(
            matches!(
                classify(200, &bare, PriceBasis::Served, false, NOW),
                DeskFetch::Failed(_)
            ),
            "a desk feed that lost its desk must not uncap the desk's wallets"
        );
        let null_desk = format!(r#"{{"price":1.0,"timestamp":{},"desk":null}}"#, NOW - 1);
        assert!(matches!(
            classify(200, &null_desk, PriceBasis::Served, false, NOW),
            DeskFetch::Failed(_)
        ));
    }

    #[test]
    fn errors_that_are_not_the_feeds_own_fail_the_fetch() {
        for (status, body) in [
            (502, "<html>bad gateway</html>".to_string()),
            (503, r#"{"error":"down"}"#.to_string()),
            (500, live_body()),
            (200, "not json".to_string()),
        ] {
            assert!(
                matches!(
                    classify(status, &body, PriceBasis::Served, false, NOW),
                    DeskFetch::Failed(_)
                ),
                "{status} {body}"
            );
        }
    }

    #[test]
    fn a_malformed_desk_fails_the_fetch_rather_than_reading_partly() {
        for (from, to) in [
            ("\"18250000000000000000\"", "\"-1\""),
            ("\"18250000000000000000\"", "18250000000000000000"),
            ("\"100000000000000000000000\"", "\"1e23\""),
            ("\"0x00000000000000000000000000000000000000ab\"", "\"nope\""),
            ("\"maxOrderDebt\"", "\"maxOrderDebtX\""),
        ] {
            let body = live_body().replacen(from, to, 1);
            assert!(
                matches!(
                    classify(200, &body, PriceBasis::Served, false, NOW),
                    DeskFetch::Failed(_)
                ),
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn a_broken_print_keeps_the_desk_but_has_no_price() {
        let crossed = live_body().replace("\"bid\":0.00067", "\"bid\":0.0007");
        let DeskFetch::Answered { reading, quote } =
            classify(200, &crossed, PriceBasis::Served, false, NOW)
        else {
            panic!("the desk still parses");
        };
        assert!(matches!(reading, DeskReading::Shared(_)));
        assert!(quote.is_err());
        let future = live_body().replace(&format!("{}", NOW - 5), &format!("{}", NOW + 60));
        let DeskFetch::Answered { quote, .. } =
            classify(200, &future, PriceBasis::Served, false, NOW)
        else {
            panic!("the desk still parses");
        };
        assert!(quote.is_err(), "a future timestamp is no print");
    }

    #[test]
    fn an_unknown_status_is_kept_and_never_live() {
        let body = live_body().replacen("\"live\"", "\"paused\"", 1);
        let DeskFetch::Answered {
            reading: DeskReading::Shared(info),
            ..
        } = classify(200, &body, PriceBasis::Served, false, NOW)
        else {
            panic!("an unknown status still reads");
        };
        assert_eq!(info.status, DeskStatus::Other);
    }
}
