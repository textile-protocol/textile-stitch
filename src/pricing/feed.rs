// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The price feed — a tiny interface any source can satisfy, plus one HTTP
//! reference adapter. The feed only has to return a current price and a
//! timestamp; operators point it at whatever source they trust. A feed may
//! also flag a print `degraded`, and Stitch then quotes nothing off it.

use anyhow::Context as _;
use serde::Deserialize;

use crate::net::http_client;
use crate::pricing::issuer_spread::{lenient_slot, Gate, SpreadSlot};
use crate::pricing::off_hours::{self, Session};
use crate::pricing::tick::feed_age_secs;
use crate::time::unix_now;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Quote {
    /// Debt per collateral (USDT per cNGN), the operator's fair price.
    pub price: f64,
    /// Nonzero Unix seconds the price was observed; no future skew is allowed.
    pub timestamp: u64,
    /// The source says this print is a protected value rather than a live
    /// one. Textile's `/price` sets it on a frozen or move-clamped cNGN center
    /// and on the raw-Monierate fallback. Missing means not degraded, so
    /// market-rate corridors and third-party feeds that never send it are
    /// unaffected. Only a boolean is accepted: anything else fails the fetch.
    #[serde(default)]
    pub degraded: bool,
    /// The reference book the inventory curve anchors on (`reference` on
    /// `/price`). Absent on every feed that doesn't publish one, which is all
    /// of them except Textile's cNGN corridors; a curve corridor without it
    /// quotes flat. See [`crate::pricing::curve`].
    ///
    /// Read leniently: a malformed book reads as none, so the curve answers
    /// flat and the price itself stays usable. An optional input of one
    /// module must not take a corridor dark.
    #[serde(default, deserialize_with = "lenient_reference")]
    pub reference: Option<ReferenceBook>,
    /// The source's own bid and ask, in the same units as `price` (debt per
    /// collateral). Only a pool with `feed_sides = true` quotes them (see
    /// [`crate::config::PoolConfig::feed_sides`]); every other pool ignores
    /// them. Textile's OTC feed serves both, with `price` their midpoint.
    ///
    /// Strict, unlike `reference`: [`parse_quote`] fails the fetch unless both
    /// are absent, or both are finite and positive with
    /// `bid <= price <= ask`. A half-present or crossed pair is a broken
    /// source, and a feed_sides pool would otherwise quote off it.
    #[serde(default)]
    pub bid: Option<f64>,
    #[serde(default)]
    pub ask: Option<f64>,
    /// How live the price is, on a session-bound feed asked for its off-hours
    /// fallback (`offHours=1`). Missing reads as live, which is every feed
    /// that isn't session-bound and every request without the parameter. An
    /// unknown value is kept as [`Session::Unknown`] and never quoted. See
    /// [`crate::pricing::off_hours`].
    #[serde(default, deserialize_with = "off_hours::lenient_session")]
    pub session: Session,
    /// Unix seconds of the last real market observation behind `price` (the
    /// onshore print, or the last offshore value change). On an off-hours
    /// print `timestamp` is when the API served it, so this is what the held
    /// extra spread ages on. `None` when absent or malformed.
    #[serde(
        default,
        rename = "marketAt",
        deserialize_with = "off_hours::lenient_unix_secs"
    )]
    pub market_at: Option<u64>,
    /// Where `price` came from (`icap-setfx`, `chainlink`). Informational.
    #[serde(
        default,
        rename = "marketSource",
        deserialize_with = "off_hours::lenient_string"
    )]
    pub market_source: Option<String>,
    /// An issuer's spread on a Textile feed switch (`spread` on `/price`).
    /// Only a feed read through an `[issuer_spread]` gate turns it into
    /// [`SpreadSlot::Verified`]; anything else is never applied. Read
    /// leniently, like `reference`. See [`crate::pricing::issuer_spread`].
    #[serde(default, deserialize_with = "lenient_slot")]
    pub spread: SpreadSlot,
}

fn lenient_reference<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ReferenceBook>, D::Error> {
    let value = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

/// A reference book as the feed serves it.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceBook {
    /// The raw reference midpoint, in the same units as `price`, before the
    /// operator's manual centre adjustments: those are the manual version of
    /// the curve and would otherwise count twice.
    pub mid: f64,
    /// Half the book's width, `(buy - mid) / mid`, in bps: the feed's median
    /// over recent uncrossed books. Unitless, so it survives the reciprocal
    /// that `mid` and `price` take.
    pub half_range_bps: f64,
}

impl Quote {
    /// The curve's two inputs, present only when the feed published a book
    /// and both numbers are usable. Zero or negative widths and non-finite
    /// values are refused rather than leaned on.
    pub fn reference_book(&self) -> Option<(f64, f64)> {
        let ReferenceBook {
            mid,
            half_range_bps: half,
        } = self.reference?;
        (mid.is_finite() && mid > 0.0 && half.is_finite() && half > 0.0).then_some((mid, half))
    }

    /// The feed's `(bid, ask)`, when it served a usable pair around `price`.
    /// [`parse_quote`] already refuses anything else; checking again here
    /// keeps a hand-built quote to the same rule.
    pub fn sides(&self) -> Option<(f64, f64)> {
        let (bid, ask) = self.bid.zip(self.ask)?;
        sides_bracket(bid, self.price, ask).then_some((bid, ask))
    }
}

/// Both sides finite and positive, and `bid <= price <= ask`. A pair equal to
/// the price is a zero-width book, which is valid.
fn sides_bracket(bid: f64, price: f64, ask: f64) -> bool {
    [bid, price, ask].iter().all(|v| v.is_finite() && *v > 0.0) && bid <= price && price <= ask
}

/// The rule [`Quote::bid`] documents, on the quote a basis produced.
fn check_sides(quote: &Quote) -> anyhow::Result<()> {
    match (quote.bid, quote.ask) {
        (None, None) => Ok(()),
        (Some(bid), Some(ask)) => {
            anyhow::ensure!(
                sides_bracket(bid, quote.price, ask),
                "feed bid ({bid}) and ask ({ask}) must be positive and bracket the price ({})",
                quote.price
            );
            Ok(())
        }
        _ => anyhow::bail!("feed bid and ask must both be present or both absent"),
    }
}

/// Which of a feed's prices the bot quotes around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriceBasis {
    /// `price`, as the feed serves it.
    #[default]
    Served,
    /// `unadjustedPrice` where the feed serves one: on Textile's cNGN feeds,
    /// the same centre before the admin's manual adjustments. A feed without
    /// the field has no adjustment to take out, and its `price` is used.
    Unadjusted,
}

/// The quote a feed body serves on `basis`.
///
/// On [`PriceBasis::Unadjusted`] a present `unadjustedPrice` replaces `price`
/// and the rest of the observation (timestamp, `degraded`, the reference
/// book) is shared. Anything there but a finite positive number fails the
/// fetch: falling back to `price` would put back the adjustment this basis
/// exists to leave out.
///
/// A `bid`/`ask` pair that is half-present, non-positive or doesn't bracket
/// the resulting price also fails the fetch (see [`Quote::bid`]).
pub fn parse_quote(body: &str, basis: PriceBasis) -> anyhow::Result<Quote> {
    let quote = basis_quote(body, basis)?;
    check_sides(&quote)?;
    Ok(quote)
}

/// The quote `basis` reads off the body, before the bid/ask check: that has
/// to run on the price the bot will actually quote around.
fn basis_quote(body: &str, basis: PriceBasis) -> anyhow::Result<Quote> {
    let quote: Quote = serde_json::from_str(body)?;
    if basis == PriceBasis::Served {
        return Ok(quote);
    }
    let value: serde_json::Value = serde_json::from_str(body)?;
    match value.get("unadjustedPrice") {
        None | Some(serde_json::Value::Null) => Ok(quote),
        Some(raw) => {
            let price = raw
                .as_f64()
                .filter(|p| p.is_finite() && *p > 0.0)
                .context("feed unadjustedPrice must be a positive number")?;
            Ok(Quote { price, ..quote })
        }
    }
}

/// An offshore or held print on a request that never asked for one
/// (`asked_off_hours` false) fails the fetch: the pool behind it has no
/// off-hours block to price it with, and caching it would let any reader of
/// that URL quote it. Unknown sessions pass here and are refused per pool by
/// [`crate::pricing::off_hours::assess`], which can name the reason.
pub fn check_session(quote: &Quote, asked_off_hours: bool) -> anyhow::Result<()> {
    let off_hours = matches!(quote.session, Session::Offshore | Session::Held);
    anyhow::ensure!(
        asked_off_hours || !off_hours,
        "feed served an {} price to a request that did not ask for off-hours prices",
        quote.session.label()
    );
    Ok(())
}

/// Implement this for a custom source; the bot only calls `fetch`.
#[allow(async_fn_in_trait)]
pub trait PriceFeed {
    async fn fetch(&self) -> anyhow::Result<Quote>;
}

/// Reference adapter: GET a JSON `{ "price": <f64>, "timestamp": <u64> }`,
/// with an optional `"degraded": <bool>`. The timestamp must be nonzero Unix
/// seconds at or before the local clock. Age limits and the degraded check
/// stay with each consumer (ladder/RFQ/NAV, see [`crate::pricing::tick::usable`]);
/// units are never converted.
pub struct HttpFeed {
    url: String,
    client: reqwest::Client,
    basis: PriceBasis,
    /// The URL asks for off-hours prices (`offHours=1`, see
    /// [`crate::pricing::off_hours::feed_request_url`]).
    off_hours: bool,
    spread_gate: Option<std::sync::Arc<Gate>>,
}

impl HttpFeed {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            off_hours: off_hours::requests_off_hours(&url),
            url,
            client: http_client(),
            basis: PriceBasis::Served,
            spread_gate: None,
        }
    }

    /// Check every served issuer spread with `gate`, keeping only a verified
    /// one on the quote. Without a gate a served spread stays unverified and
    /// nothing applies it.
    pub fn with_spread_gate(self, gate: Option<std::sync::Arc<Gate>>) -> Self {
        Self {
            spread_gate: gate,
            ..self
        }
    }

    /// Read `basis`'s price off every response (see [`parse_quote`]).
    pub fn with_basis(self, basis: PriceBasis) -> Self {
        Self { basis, ..self }
    }
}

impl PriceFeed for HttpFeed {
    async fn fetch(&self) -> anyhow::Result<Quote> {
        let body = self
            .client
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let quote = parse_quote(&body, self.basis)?;
        anyhow::ensure!(
            feed_age_secs(quote.timestamp, unix_now()).is_some(),
            "feed timestamp must be nonzero Unix seconds at or before the bot clock (zero future skew)"
        );
        check_session(&quote, self.off_hours)?;
        let Some(gate) = &self.spread_gate else {
            return Ok(quote);
        };
        // Every switch response counts, with or without a spread: a newer
        // one without makes an older one's spread out of date.
        let spread = gate.spread_for(&quote.spread, switch_revision(&body)).await;
        Ok(Quote { spread, ..quote })
    }
}

/// The feed switch a `/price` response answered for and the switch revision
/// it answered at (`feedSwitch` and `revision`), when it's a switch response.
fn switch_revision(body: &str) -> Option<(String, u64)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let switch = value.get("feedSwitch")?.as_str()?.to_string();
    Some((switch, value.get("revision")?.as_u64()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> serde_json::Result<Quote> {
        serde_json::from_str(body)
    }

    #[test]
    fn reads_the_switch_and_revision_a_switch_response_answered_at() {
        let body =
            r#"{"price":16500,"timestamp":1754388000,"feedSwitch":"idrx-usdt","revision":14}"#;
        assert_eq!(switch_revision(body), Some(("idrx-usdt".to_string(), 14)));
        assert_eq!(
            switch_revision(r#"{"price":1,"timestamp":1754388000,"revision":3}"#),
            None,
            "an OTC feed's revision is its own, not a switch's"
        );
        assert_eq!(
            switch_revision(r#"{"price":1,"timestamp":1754388000}"#),
            None
        );
    }

    #[test]
    fn the_unadjusted_basis_takes_the_centre_before_manual_adjustments() {
        let body = r#"{"price":0.000720,"unadjustedPrice":0.000721,"timestamp":1754388000,"degraded":true}"#;
        let served = parse_quote(body, PriceBasis::Served).unwrap();
        assert_eq!(served.price, 0.000720);
        let unadjusted = parse_quote(body, PriceBasis::Unadjusted).unwrap();
        assert_eq!(unadjusted.price, 0.000721);
        assert!(unadjusted.degraded, "the rest of the observation is shared");
        assert_eq!(unadjusted.timestamp, served.timestamp);
    }

    #[test]
    fn a_feed_with_no_adjustments_serves_its_price_on_either_basis() {
        // Market-rate corridors, the Monierate fallback, third-party feeds.
        for body in [
            r#"{"price":0.000720,"timestamp":1754388000}"#,
            r#"{"price":0.000720,"unadjustedPrice":null,"timestamp":1754388000}"#,
        ] {
            assert_eq!(
                parse_quote(body, PriceBasis::Unadjusted).unwrap().price,
                0.000720
            );
        }
    }

    #[test]
    fn a_malformed_unadjusted_price_fails_only_the_basis_that_reads_it() {
        // Quoting `price` instead would put back the adjustment a Live curve
        // leaves out, so its fetch fails; every other bot is unaffected.
        for raw in [r#""0.0007""#, "0", "-1", "true", "{}"] {
            let body =
                format!(r#"{{"price":0.000720,"unadjustedPrice":{raw},"timestamp":1754388000}}"#);
            assert!(
                parse_quote(&body, PriceBasis::Unadjusted).is_err(),
                "{raw} was accepted"
            );
            assert_eq!(
                parse_quote(&body, PriceBasis::Served).unwrap().price,
                0.000720
            );
        }
    }

    #[test]
    fn the_feeds_own_sides_are_read_when_both_bracket_the_price() {
        let body = r#"{"price":0.000650,"bid":0.000645,"ask":0.000655,"timestamp":1754388000}"#;
        let q = parse_quote(body, PriceBasis::Served).unwrap();
        assert_eq!((q.bid, q.ask), (Some(0.000645), Some(0.000655)));
        assert_eq!(q.sides(), Some((0.000645, 0.000655)));
        // A zero-width book is still a book.
        let flat = r#"{"price":0.00065,"bid":0.00065,"ask":0.00065,"timestamp":1754388000}"#;
        assert!(parse_quote(flat, PriceBasis::Served).is_ok());
        // Absent on both sides: every feed that never sends them.
        let plain = parse_quote(
            r#"{"price":0.00065,"timestamp":1754388000}"#,
            PriceBasis::Served,
        )
        .unwrap();
        assert_eq!((plain.bid, plain.ask, plain.sides()), (None, None, None));
    }

    #[test]
    fn a_half_present_crossed_or_non_positive_pair_fails_the_fetch() {
        for sides in [
            r#""bid":0.000645"#,
            r#""ask":0.000655"#,
            r#""bid":0.000655,"ask":0.000645"#,
            r#""bid":0.000651,"ask":0.000655"#,
            r#""bid":0.000645,"ask":0.000649"#,
            r#""bid":0,"ask":0.000655"#,
            r#""bid":-0.000645,"ask":0.000655"#,
            r#""bid":"0.000645","ask":0.000655"#,
            r#""bid":0.000645,"ask":null"#,
        ] {
            let body = format!(r#"{{"price":0.000650,{sides},"timestamp":1754388000}}"#);
            assert!(
                parse_quote(&body, PriceBasis::Served).is_err(),
                "{sides} was accepted"
            );
        }
    }

    #[test]
    fn the_sides_must_bracket_the_price_the_basis_quotes_around() {
        // On the unadjusted basis the bot quotes around `unadjustedPrice`, so
        // that is the price the pair has to bracket.
        let body = r#"{"price":0.000650,"unadjustedPrice":0.000660,"bid":0.000645,"ask":0.000655,"timestamp":1754388000}"#;
        assert!(parse_quote(body, PriceBasis::Served).is_ok());
        assert!(parse_quote(body, PriceBasis::Unadjusted).is_err());
    }

    #[test]
    fn the_session_is_read_leniently() {
        let base = r#""price":0.00025,"timestamp":1754388000"#;
        let session = |extra: &str| {
            parse_quote(&format!("{{{base}{extra}}}"), PriceBasis::Served)
                .unwrap()
                .session
        };
        // Missing (every feed that isn't session-bound) and null are live.
        assert_eq!(session(""), Session::Live);
        assert_eq!(session(r#","session":null"#), Session::Live);
        assert_eq!(session(r#","session":"live""#), Session::Live);
        assert_eq!(session(r#","session":"offshore""#), Session::Offshore);
        assert_eq!(session(r#","session":"held""#), Session::Held);
        // Unknown values parse, and are refused per pool rather than failing
        // the fetch.
        for raw in [r#""closed""#, r#""HELD""#, "1", "true", "{}"] {
            assert_eq!(
                session(&format!(r#","session":{raw}"#)),
                Session::Unknown,
                "{raw}"
            );
        }
    }

    #[test]
    fn market_time_and_source_are_read_when_well_formed() {
        let q = parse_quote(
            r#"{"price":0.00025,"timestamp":1754388000,"session":"held","marketAt":1754380000,"marketSource":"icap-setfx"}"#,
            PriceBasis::Served,
        )
        .unwrap();
        assert_eq!(q.market_at, Some(1_754_380_000));
        assert_eq!(q.market_source.as_deref(), Some("icap-setfx"));
        for raw in [r#""1754380000""#, "-1", "0", "1.5", "null"] {
            let body = format!(
                r#"{{"price":0.00025,"timestamp":1754388000,"marketAt":{raw},"marketSource":7}}"#
            );
            let q = parse_quote(&body, PriceBasis::Served).unwrap();
            assert_eq!((q.market_at, q.market_source), (None, None), "{raw}");
        }
    }

    #[test]
    fn an_off_hours_session_is_refused_on_a_request_that_did_not_ask() {
        let quote = |session: &str| {
            parse_quote(
                &format!(r#"{{"price":0.00025,"timestamp":1754388000,"session":"{session}"}}"#),
                PriceBasis::Served,
            )
            .unwrap()
        };
        for session in ["offshore", "held"] {
            assert!(check_session(&quote(session), false).is_err());
            assert!(check_session(&quote(session), true).is_ok());
        }
        assert!(check_session(&quote("live"), false).is_ok());
        assert!(check_session(&quote("closed"), false).is_ok());
    }

    #[test]
    fn degraded_is_read_when_present() {
        let held = parse(r#"{"price":0.00065,"timestamp":1754388000,"degraded":true}"#).unwrap();
        assert!(held.degraded);
        let live = parse(r#"{"price":0.00065,"timestamp":1754388000,"degraded":false}"#).unwrap();
        assert!(!live.degraded);
    }

    #[test]
    fn a_feed_without_the_flag_is_not_degraded() {
        // Market-rate corridors on /price, and any third-party feed.
        let plain = parse(r#"{"price":0.00065,"timestamp":1754388000}"#).unwrap();
        assert_eq!(
            plain,
            Quote {
                price: 0.00065,
                timestamp: 1_754_388_000,
                degraded: false,
                spread: Default::default(),
                reference: None,
                bid: None,
                ask: None,
                session: Default::default(),
                market_at: None,
                market_source: None,
            }
        );
        // /price adds metadata next to the quote; it is ignored, not refused.
        let textile = parse(
            r#"{"pair":"cngn-usdt","chainId":56,"price":0.00065,"timestamp":1754388000,"degraded":true}"#,
        )
        .unwrap();
        assert!(textile.degraded);
    }

    #[test]
    fn a_flag_that_is_not_a_boolean_is_refused() {
        // Reading `"yes"` or `1` as false would quote off a print the source
        // may have meant to withhold, so the whole body is rejected.
        for flag in [r#""yes""#, "1", "null", r#""false""#] {
            let body = format!(r#"{{"price":0.00065,"timestamp":1754388000,"degraded":{flag}}}"#);
            assert!(parse(&body).is_err(), "degraded: {flag} was accepted");
        }
    }

    #[test]
    fn the_reference_book_is_read_when_the_feed_publishes_it() {
        let q = parse(
            r#"{"price":0.000725,"timestamp":1754388000,"reference":{"mid":0.000726,"halfRangeBps":9.4}}"#,
        )
        .unwrap();
        assert_eq!(q.reference_book(), Some((0.000726, 9.4)));
    }

    #[test]
    fn a_feed_without_a_reference_book_simply_has_none() {
        // Every corridor except Textile's cNGN ones, and every third-party
        // feed. Their quotes must keep parsing exactly as before.
        let q = parse(r#"{"price":0.00065,"timestamp":1754388000}"#).unwrap();
        assert_eq!(q.reference, None);
        assert_eq!(q.reference_book(), None);
    }

    #[test]
    fn an_unusable_reference_book_is_refused_rather_than_leaned_on() {
        for book in [
            r#"{"mid":0,"halfRangeBps":9.4}"#,
            r#"{"mid":-0.1,"halfRangeBps":9.4}"#,
            r#"{"mid":0.000726,"halfRangeBps":-1}"#,
            r#"{"mid":0.000726,"halfRangeBps":0}"#,
        ] {
            let body = format!(r#"{{"price":0.000725,"timestamp":1,"reference":{book}}}"#);
            assert_eq!(parse(&body).unwrap().reference_book(), None, "{body}");
        }
        // Half a book, or the wrong shape, is no book; the price still parses.
        for book in [r#"{"mid":0.000726}"#, r#""9.4""#, "null"] {
            let body = format!(r#"{{"price":0.000725,"timestamp":1,"reference":{book}}}"#);
            let q = parse(&body).unwrap();
            assert_eq!((q.price, q.reference), (0.000725, None), "{body}");
        }
    }
}
