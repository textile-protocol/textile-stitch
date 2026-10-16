// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The price feed — a tiny interface any source can satisfy, plus one HTTP
//! reference adapter. The feed only has to return a current price and a
//! timestamp; operators point it at whatever source they trust. A feed may
//! also flag a print `degraded`, and Stitch then quotes nothing off it.

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
}

impl HttpFeed {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: http_client(),
        }
    }
}

impl PriceFeed for HttpFeed {
    async fn fetch(&self) -> anyhow::Result<Quote> {
        let quote = self
            .client
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?
            .json::<Quote>()
            .await?;
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
}
