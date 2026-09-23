// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A dollar price for the gas token, so the funding check can say "$1 of CELO".
//!
//! Textile's `/price` feed covers the corridor tokens but not the chain's own
//! gas token. Textile does publish `GET {api}/native-price?chainId=N`
//! (Binance-backed, cached a minute server-side), and CoinGecko's free simple
//! price is the second opinion. Both are asked at once under a short timeout,
//! Textile wins when it answers, and when neither does the chain's built-in
//! figure stands in — deliberately LOW, so the shortfall it reports errs on
//! asking the operator for a little more gas rather than a little less.
//!
//! One in-process cache per chain: a good answer is reused for a minute, a
//! failure is remembered for thirty seconds so a five-second funding poll
//! doesn't pay two lookups' timeouts every tick.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;

/// The public CoinGecko origin. Overridable through `STITCH_PANEL_COINGECKO_API`.
pub const DEFAULT_COINGECKO_API: &str = "https://api.coingecko.com";

/// How long a good answer is reused.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// How long a failure (both sources down) is remembered before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Each outbound lookup gets this long. Both run concurrently, so the check as
/// a whole never waits longer than this on a price.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(4);

/// Where a price came from, so the UI can mark a fallback as an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PriceSource {
    /// Textile's `/native-price`.
    Textile,
    /// CoinGecko's simple price.
    Coingecko,
    /// The built-in low figure for this chain; nothing answered.
    Fallback,
}

/// A dollar price for one chain's gas token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativePrice {
    pub usd: f64,
    pub source: PriceSource,
}

/// What a chain's gas token is called, which CoinGecko id prices it, the low
/// figure that stands in when nothing answers, and what one transaction costs
/// there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GasToken {
    pub symbol: &'static str,
    pub coingecko_id: &'static str,
    /// Deliberately below any plausible market price, never above it.
    pub fallback_usd: f64,
    /// Dollars to budget for ONE of the transactions the wizard sends right
    /// after the funding gate passes (an ERC-20 approve, ~46k gas, plus
    /// headroom). Chain-specific on purpose: one figure across chains either
    /// certifies an Ethereum wallet that cannot pay for a single approve, or
    /// asks a Celo operator for a hundred times what they need.
    pub tx_gas_usd: f64,
}

/// The gas token for a chain, when the panel knows the chain at all.
///
/// A chain outside this table still gets a Textile lookup (Textile may know
/// it), but no CoinGecko id and no fallback: the caller then treats any
/// non-zero balance as enough, because it cannot price a threshold.
pub fn gas_token(chain_id: u64) -> Option<GasToken> {
    Some(match chain_id {
        // Ethereum, Base, Arbitrum One, Robinhood Chain (an Arbitrum Orbit L2)
        // all pay gas in ETH.
        // Ethereum mainnet is the expensive one: an approve there is dollars,
        // not cents, and the L2s that share its gas token are cents.
        1 => GasToken {
            symbol: "ETH",
            coingecko_id: "ethereum",
            fallback_usd: 1000.0,
            tx_gas_usd: 5.0,
        },
        8453 | 84532 | 42161 | 4663 => GasToken {
            symbol: "ETH",
            coingecko_id: "ethereum",
            fallback_usd: 1000.0,
            tx_gas_usd: 0.50,
        },
        // BNB Smart Chain is the cheap one: gas sits at 0.05-0.1 gwei and an
        // approve is ~46k gas, so one costs well under a cent. A dollar here
        // asked an operator for a couple hundred times what they need, and the
        // wizard sat on "add $2 of BNB" with a funded wallet in front of it.
        56 | 97 => GasToken {
            symbol: "BNB",
            coingecko_id: "binancecoin",
            fallback_usd: 200.0,
            tx_gas_usd: 0.025,
        },
        42220 => GasToken {
            symbol: "CELO",
            coingecko_id: "celo",
            fallback_usd: 0.05,
            tx_gas_usd: 0.50,
        },
        137 => GasToken {
            symbol: "POL",
            coingecko_id: "polygon-ecosystem-token",
            fallback_usd: 0.10,
            tx_gas_usd: 0.50,
        },
        _ => return None,
    })
}

/// The gas token's ticker, or the plain word "gas" for a chain the panel
/// doesn't know. Copy reads "add $1 of gas" rather than inventing a ticker.
pub fn gas_symbol(chain_id: u64) -> &'static str {
    gas_token(chain_id).map(|g| g.symbol).unwrap_or("gas")
}

struct Cached {
    price: Option<NativePrice>,
    until: Instant,
}

/// Per-chain gas prices, cached in process. Built once in `AppState::new`.
pub struct NativePrices {
    /// CoinGecko origin. `None` disables that lookup entirely.
    coingecko_base: Option<String>,
    cache: RwLock<HashMap<u64, Cached>>,
}

impl NativePrices {
    pub fn new(coingecko_base: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            coingecko_base,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// A dollar price for `chain_id`'s gas token, or `None` when nothing
    /// answered and the chain has no built-in figure.
    ///
    /// `textile_origin` is the bot's own API origin (from `indexer_url`), so a
    /// bot pointed at a staging venue asks that venue.
    pub async fn get(&self, chain_id: u64, textile_origin: Option<&str>) -> Option<NativePrice> {
        if let Some(cached) = self.cached(chain_id).await {
            return cached;
        }

        let token = gas_token(chain_id);
        let textile = async {
            let origin = textile_origin?;
            tokio::time::timeout(LOOKUP_TIMEOUT, fetch_textile(origin, chain_id))
                .await
                .ok()?
                .ok()
        };
        let coingecko = async {
            let base = self.coingecko_base.as_deref()?;
            let id = token?.coingecko_id;
            tokio::time::timeout(LOOKUP_TIMEOUT, fetch_coingecko(base, id))
                .await
                .ok()?
                .ok()
        };
        let (textile, coingecko) = tokio::join!(textile, coingecko);

        let (price, failed) = match (textile, coingecko) {
            (Some(usd), _) => (
                Some(NativePrice {
                    usd,
                    source: PriceSource::Textile,
                }),
                false,
            ),
            (None, Some(usd)) => (
                Some(NativePrice {
                    usd,
                    source: PriceSource::Coingecko,
                }),
                false,
            ),
            (None, None) => (
                token.map(|t| NativePrice {
                    usd: t.fallback_usd,
                    source: PriceSource::Fallback,
                }),
                true,
            ),
        };

        let until = Instant::now() + if failed { RETRY_AFTER } else { FRESH_FOR };
        self.cache
            .write()
            .await
            .insert(chain_id, Cached { price, until });
        price
    }

    async fn cached(&self, chain_id: u64) -> Option<Option<NativePrice>> {
        let guard = self.cache.read().await;
        let cached = guard.get(&chain_id)?;
        if cached.until <= Instant::now() {
            return None;
        }
        Some(cached.price)
    }
}

/// A client for one short lookup. Never the shared 15-second client: a price
/// is a nice-to-have and must not hold the funding check hostage.
fn short_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(3))
        .build()?)
}

fn usable(usd: f64) -> anyhow::Result<f64> {
    anyhow::ensure!(usd.is_finite() && usd > 0.0, "price {usd} is not usable");
    Ok(usd)
}

/// `GET {origin}/native-price?chainId=N` → `{ "priceUsd": 0.0799, ... }`.
async fn fetch_textile(origin: &str, chain_id: u64) -> anyhow::Result<f64> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        price_usd: f64,
    }
    let url = format!(
        "{}/native-price?chainId={chain_id}",
        origin.trim_end_matches('/')
    );
    let body: Body = short_client()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    usable(body.price_usd)
}

/// `GET {base}/api/v3/simple/price?ids=celo&vs_currencies=usd` → `{ "celo": { "usd": 0.08 } }`.
async fn fetch_coingecko(base: &str, id: &str) -> anyhow::Result<f64> {
    let url = format!(
        "{}/api/v3/simple/price?ids={id}&vs_currencies=usd",
        base.trim_end_matches('/')
    );
    let body: serde_json::Value = short_client()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let usd = body
        .get(id)
        .and_then(|v| v.get("usd"))
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("no usd price for {id} in {body}"))?;
    usable(usd)
}

/// The scheme+host(+port) of a URL, with no path and no trailing slash — what
/// `/native-price` and `/v2/rfq/preview` hang off. `None` for a URL that
/// doesn't parse as http(s).
pub fn http_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    /// A price server answering both shapes, counting Textile hits.
    async fn mock_prices(
        textile_status: u16,
        coingecko_status: u16,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/native-price",
                get(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::from_u16(textile_status).unwrap(),
                            Json(json!({ "chainId": 42220, "symbol": "CELOUSDT", "priceUsd": 0.08, "timestamp": 1 })),
                        )
                    }
                }),
            )
            .route(
                "/api/v3/simple/price",
                get(move || async move {
                    (
                        axum::http::StatusCode::from_u16(coingecko_status).unwrap(),
                        Json(json!({ "celo": { "usd": 0.07 } })),
                    )
                }),
            );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), hits, handle)
    }

    #[tokio::test]
    async fn textile_wins_and_is_cached() {
        let (base, hits, _server) = mock_prices(200, 200).await;
        let prices = NativePrices::new(Some(base.clone()));
        let first = prices.get(42220, Some(&base)).await.unwrap();
        let second = prices.get(42220, Some(&base)).await.unwrap();
        assert_eq!(first.source, PriceSource::Textile);
        assert_eq!(first.usd, 0.08);
        assert_eq!(second, first);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the second call is a cache hit"
        );
    }

    #[tokio::test]
    async fn coingecko_covers_a_textile_outage() {
        let (base, _hits, _server) = mock_prices(500, 200).await;
        let prices = NativePrices::new(Some(base.clone()));
        let price = prices.get(42220, Some(&base)).await.unwrap();
        assert_eq!(price.source, PriceSource::Coingecko);
        assert_eq!(price.usd, 0.07);
    }

    #[tokio::test]
    async fn both_down_means_the_low_fallback_and_no_retry_storm() {
        let (base, hits, _server) = mock_prices(500, 500).await;
        let prices = NativePrices::new(Some(base.clone()));
        let price = prices.get(42220, Some(&base)).await.unwrap();
        assert_eq!(price.source, PriceSource::Fallback);
        assert_eq!(price.usd, 0.05);
        // The failure is remembered: a second call within RETRY_AFTER does not
        // go back to the source.
        let again = prices.get(42220, Some(&base)).await.unwrap();
        assert_eq!(again, price);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unknown_chain_with_nothing_answering_has_no_price() {
        let prices = NativePrices::new(None);
        assert!(prices.get(999, Some("http://127.0.0.1:1")).await.is_none());
        assert_eq!(gas_symbol(999), "gas");
    }

    #[tokio::test]
    async fn a_closed_port_fails_fast_not_slow() {
        let prices = NativePrices::new(Some("http://127.0.0.1:1".into()));
        let started = Instant::now();
        let price = prices.get(56, Some("http://127.0.0.1:1")).await.unwrap();
        assert_eq!(price.source, PriceSource::Fallback);
        assert_eq!(price.usd, 200.0);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn origins_drop_paths_and_keep_ports() {
        assert_eq!(
            http_origin("https://api.textilecredit.com/price?chainId=56").as_deref(),
            Some("https://api.textilecredit.com")
        );
        assert_eq!(
            http_origin("http://127.0.0.1:8420/v2/maker/stream").as_deref(),
            Some("http://127.0.0.1:8420")
        );
        assert_eq!(
            http_origin("wss://api.textilecredit.com/v2/maker/stream"),
            None
        );
        assert_eq!(http_origin("nonsense"), None);
    }

    #[test]
    fn known_chains_name_their_gas_token() {
        assert_eq!(gas_symbol(42220), "CELO");
        assert_eq!(gas_symbol(56), "BNB");
        assert_eq!(gas_symbol(8453), "ETH");
        assert_eq!(gas_symbol(4663), "ETH");
        assert_eq!(gas_symbol(137), "POL");
    }
}
