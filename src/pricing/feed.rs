// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The price feed — a tiny interface any source can satisfy, plus one HTTP
//! reference adapter. The feed only has to return a current price and a
//! timestamp; operators point it at whatever source they trust.

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
}

/// Implement this for a custom source; the bot only calls `fetch`.
#[allow(async_fn_in_trait)]
pub trait PriceFeed {
    async fn fetch(&self) -> anyhow::Result<Quote>;
}

/// Reference adapter: GET a JSON `{ "price": <f64>, "timestamp": <u64> }`.
/// The timestamp must be nonzero Unix seconds at or before the local clock.
/// Age limits remain per consumer (ladder/RFQ/NAV); units are never converted.
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
