// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The price feed — a tiny interface any source can satisfy, plus one HTTP
//! reference adapter. The feed only has to return a current price and a
//! timestamp; operators point it at whatever source they trust. A feed may
//! also flag a print `degraded`, and Stitch then quotes nothing off it.

use anyhow::Context as _;
use serde::Deserialize;

use crate::net::http_client;
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
pub fn parse_quote(body: &str, basis: PriceBasis) -> anyhow::Result<Quote> {
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
}

impl HttpFeed {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: http_client(),
            basis: PriceBasis::Served,
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
        Ok(quote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> serde_json::Result<Quote> {
        serde_json::from_str(body)
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
                reference: None,
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
