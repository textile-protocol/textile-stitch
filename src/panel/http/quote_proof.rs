// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Proof that the bot is quoting: ask Textile for the price a taker would get.
//!
//! The venue's `POST /v2/rfq/preview` is public and needs no credential, but it
//! sends no CORS headers, so the browser can't call it — the panel proxies one
//! small probe on the bot's behalf. Restricted to the bot's own wallet by
//! default, so a quote here is THIS stitch's quote and not some other maker's.
//!
//! Thin on purpose: one venue call, no chain reads, no retries. The screen
//! decides when to ask again; it can see `retryAfterMs` and the reason.
//!
//! The bot's maker credential is never attached. The venue refuses maker keys
//! on taker paths outright, and this is a taker path.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::allowances::{short_token, token_symbols};
use super::funding::{format_units, units};
use super::settings::config_path;
use super::{ApiError, AppState};
use crate::config::Config;
use crate::enroll::{venue_error_message, venue_origin_from_config};
use crate::setup;

/// The one venue whose quotes have a public swap page to link to.
pub const PUBLIC_VENUE_ORIGIN: &str = "https://api.textilecredit.com";

/// Where a taker sees the same pair.
pub const PUBLIC_SWAP_BASE: &str = "https://app.textilecredit.com/s/swap";

/// The preview can wait ~2s for a level republish and the venue's engine hop
/// is 5s, so this is generous enough to get a real answer and short enough
/// that a dead venue doesn't pin the screen.
const VENUE_TIMEOUT: Duration = Duration::from_secs(10);
const VENUE_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Which way the probe trades, from the TAKER's seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    /// The taker pays USDT and receives the soft token: exercises the bot's
    /// SELL (ask) side, the one funded with the soft token.
    UsdtToSoft,
    /// The taker sells the soft token for USDT: exercises the bot's BUY (bid)
    /// side, the one funded with USDT.
    SoftToUsdt,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteProofBody {
    /// `"usdtToSoft"` (default) or `"softToUsdt"`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Alias in the bot's own terms: `"sell"` = the bot sells the soft token
    /// (`usdtToSoft`), `"buy"` = the bot buys it (`softToUsdt`). `direction`
    /// wins when both are given.
    #[serde(default)]
    pub side: Option<String>,
    /// Restrict the preview to this bot's wallet. Default true.
    #[serde(default)]
    pub only_this_bot: Option<bool>,
    /// `[[pools]]` index. Default 0.
    #[serde(default)]
    pub pool: Option<usize>,
    /// Venue override, like the access routes take. Tests point it at a mock.
    #[serde(default)]
    pub venue_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairBody {
    pub soft_symbol: String,
    pub soft_token: String,
    pub soft_decimals: u8,
    pub stable_symbol: String,
    pub stable_token: String,
    pub stable_decimals: u8,
    /// The corridor's display name, e.g. `cNGN / USDT`. `null` for a custom pool.
    pub label: Option<String>,
    pub network_label: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeBody {
    pub sell_symbol: String,
    /// What the probe hands over (exact input) or asks for (exact output),
    /// in whole tokens.
    pub sell_text: Option<String>,
    pub buy_symbol: String,
    pub buy_text: Option<String>,
    /// Wallets the preview was restricted to: the bot's, or none.
    pub restricted_to: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteBody {
    pub sell_text: String,
    pub buy_text: String,
    /// The venue fee, in the sell token.
    pub fee_text: String,
    pub fee_symbol: String,
    /// USDT per one soft token, before the fee (from the venue's `rateRay`).
    pub usdt_per_soft: String,
    /// Soft tokens per USDT, before the fee.
    pub soft_per_usdt: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateBody {
    /// Soft tokens per USDT as actually delivered, fee included: the headline.
    pub all_in: String,
    /// Soft tokens per USDT before the venue fee, from `rateRay`.
    pub pre_fee: String,
    /// USDT per one soft token before the fee.
    pub usdt_per_soft: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteProofResponse {
    pub direction: Direction,
    pub chain_id: u64,
    pub pair: PairBody,
    pub probe: ProbeBody,
    /// `"preview"` or `"no_quote"`.
    pub status: String,
    /// `status == "preview"`.
    pub ok: bool,
    /// On `no_quote`: `no_makers_online`, `no_valid_quote`,
    /// `no_restricted_liquidity`, or whatever else the venue said.
    pub reason: Option<String>,
    pub quote: Option<QuoteBody>,
    pub rate: Option<RateBody>,
    /// Flat copies of the quote for callers that want one level.
    pub sell_symbol: String,
    pub sell_amount: Option<String>,
    pub buy_symbol: String,
    pub buy_amount: Option<String>,
    /// Depth the venue could fill right now, in whole tokens. The token it is
    /// counted in depends on the probe: an exact-input probe reports what it
    /// could sell, an exact-output one what it could buy. Never assume; read
    /// [`Self::available_symbol`].
    pub available_text: Option<String>,
    /// The ticker [`Self::available_text`] is denominated in. `null` when
    /// there is no depth to report.
    pub available_symbol: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub reserved_until: Option<String>,
    /// The quote was routed to this bot's wallet. `null` when the probe was
    /// not tied to it.
    pub from_this_bot: Option<bool>,
    /// Previews don't expire; a firm quote would. Always `null` here.
    pub expires_at: Option<String>,
    pub swap_url: Option<String>,
    /// The same page priced only by this bot.
    pub swap_url_mine: Option<String>,
    /// The venue's `data` object, verbatim.
    pub raw: Value,
    pub checked_at_unix: u64,
}

/// The venue's `data`, loosely: every field optional so a no_quote parses too.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewData {
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    sell_amount: Option<String>,
    #[serde(default)]
    buy_amount: Option<String>,
    #[serde(default)]
    fee_amount: Option<String>,
    #[serde(default)]
    rate_ray: Option<String>,
    #[serde(default)]
    available_sell_amount: Option<String>,
    #[serde(default)]
    available_buy_amount: Option<String>,
    #[serde(default)]
    retry_after_ms: Option<u64>,
    #[serde(default)]
    reserved_until: Option<String>,
    #[serde(default)]
    routing: Option<RoutingData>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoutingData {
    #[serde(default)]
    target_maker_wallets: Vec<String>,
    /// The venue honoured `restrictedLiquidityWallets` on this request.
    #[serde(default)]
    restriction_applied: bool,
}

/// `POST /api/bots/{name}/quote-proof`
pub async fn quote_proof(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Option<Json<QuoteProofBody>>,
) -> Result<Response, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let bot = state.bot(&name).await?;
    let path = config_path(&bot)?;
    let toml = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::internal(&anyhow::anyhow!(e).context(format!("reading {}", path.display())))
    })?;
    let cfg = Config::from_toml(&toml).map_err(ApiError::bad_request)?;

    let index = body.pool.unwrap_or(0);
    let pool = cfg
        .pools
        .get(index)
        .ok_or_else(|| ApiError::bad_request(format!("this bot has no pool {index}")))?;
    let direction = parse_direction(body.direction.as_deref(), body.side.as_deref())?;
    let only_this_bot = body.only_this_bot.unwrap_or(true);

    let soft = pool.collateral.trim().parse::<Address>();
    let stable = pool.debt.trim().parse::<Address>();
    let (soft, stable) = match (soft, stable) {
        (Ok(s), Ok(d)) => (s, d),
        (Err(e), _) | (_, Err(e)) => {
            return Err(ApiError::bad_request(format!(
                "the pool's token addresses are not valid: {e}"
            )))
        }
    };

    let operator = bot.config.as_ref().and_then(|c| c.operator_address.clone());
    if only_this_bot && operator.is_none() {
        return Err(ApiError::bad_request(
            "this bot has no operator address the panel can read, so the proof can't be tied \
             to it. Ask for the open-market quote instead.",
        ));
    }
    let restricted_to: Vec<String> = if only_this_bot {
        operator.iter().cloned().collect()
    } else {
        Vec::new()
    };

    let tickers = token_symbols(&cfg);
    let soft_key = format!("{soft:#x}");
    let stable_key = format!("{stable:#x}");
    let soft_symbol = tickers.get(&soft_key).cloned();
    let stable_symbol = tickers
        .get(&stable_key)
        .cloned()
        .map(|s| canonical_symbol(&s));
    let identity = setup::pool_identity(cfg.chain_id, pool);
    let pair = PairBody {
        soft_symbol: soft_symbol
            .clone()
            .unwrap_or_else(|| short_token(&soft_key)),
        soft_token: soft_key.clone(),
        soft_decimals: pool.collateral_decimals,
        stable_symbol: stable_symbol
            .clone()
            .unwrap_or_else(|| short_token(&stable_key)),
        stable_token: stable_key.clone(),
        stable_decimals: pool.debt_decimals,
        label: identity.as_ref().map(|c| c.display_name.clone()),
        network_label: identity.as_ref().map(|c| c.network_label.clone()),
    };

    // One whole USDT either way: exact input when the taker pays USDT, exact
    // output when the taker asks for USDT. Sized in USDT so no price is needed
    // to build the probe, and so it clears the venue's dust floor.
    let one_stable = U256::from(10u8).pow(U256::from(pool.debt_decimals));
    let (sell_token, buy_token, sell_symbol, buy_symbol, sell_decimals, buy_decimals) =
        match direction {
            Direction::UsdtToSoft => (
                stable,
                soft,
                pair.stable_symbol.clone(),
                pair.soft_symbol.clone(),
                pool.debt_decimals,
                pool.collateral_decimals,
            ),
            Direction::SoftToUsdt => (
                soft,
                stable,
                pair.soft_symbol.clone(),
                pair.stable_symbol.clone(),
                pool.collateral_decimals,
                pool.debt_decimals,
            ),
        };
    let mut venue_body = json!({
        "chainId": cfg.chain_id,
        "sellToken": format!("{sell_token:#x}"),
        "buyToken": format!("{buy_token:#x}"),
    });
    match direction {
        Direction::UsdtToSoft => venue_body["sellAmount"] = json!(one_stable.to_string()),
        Direction::SoftToUsdt => venue_body["buyAmount"] = json!(one_stable.to_string()),
    }
    if !restricted_to.is_empty() {
        venue_body["restrictedLiquidityWallets"] = json!(restricted_to);
    }

    let origin = venue_origin_from_config(&cfg, body.venue_url.as_deref());
    let data = preview(&origin, &venue_body).await?;

    let parsed: PreviewData = serde_json::from_value(data.clone())
        .map_err(|e| venue_down(format!("it answered with a body the panel can't read: {e}")))?;
    let is_preview = parsed.status == "preview";
    let sell_units = parsed
        .sell_amount
        .as_deref()
        .and_then(parse_atomic)
        .map(|a| (a, format_units(a, sell_decimals)));
    let buy_units = parsed
        .buy_amount
        .as_deref()
        .and_then(parse_atomic)
        .map(|a| (a, format_units(a, buy_decimals)));

    let (quote, rate) = if is_preview {
        match (&sell_units, &buy_units) {
            (Some((sell_atomic, sell_text)), Some((buy_atomic, buy_text))) => {
                let fee_text = parsed
                    .fee_amount
                    .as_deref()
                    .and_then(parse_atomic)
                    .map(|a| format_units(a, sell_decimals))
                    .unwrap_or_else(|| "0".to_string());
                let (usdt_per_soft, pre_fee) = parsed
                    .rate_ray
                    .as_deref()
                    .and_then(rates_from_ray)
                    .unwrap_or_else(|| ("".to_string(), "".to_string()));
                let sell_f = units(*sell_atomic, sell_decimals);
                let buy_f = units(*buy_atomic, buy_decimals);
                let all_in = match direction {
                    Direction::UsdtToSoft => buy_f / sell_f,
                    Direction::SoftToUsdt => sell_f / buy_f,
                };
                (
                    Some(QuoteBody {
                        sell_text: sell_text.clone(),
                        buy_text: buy_text.clone(),
                        fee_text,
                        fee_symbol: sell_symbol.clone(),
                        usdt_per_soft: usdt_per_soft.clone(),
                        soft_per_usdt: pre_fee.clone(),
                    }),
                    Some(RateBody {
                        all_in: fmt_sig(all_in, 6),
                        pre_fee,
                        usdt_per_soft,
                    }),
                )
            }
            _ => (None, None),
        }
    } else {
        (None, None)
    };

    // Depth is reported in the token the request named: sell for exact-input,
    // buy for exact-output. Either way it is the size to retry at — and it is
    // NOT always the sell token, so the ticker travels with the number.
    let (available_text, available_symbol) = match direction {
        Direction::UsdtToSoft => (
            parsed
                .available_sell_amount
                .as_deref()
                .and_then(parse_atomic)
                .map(|a| format_units(a, sell_decimals)),
            sell_symbol.clone(),
        ),
        Direction::SoftToUsdt => (
            parsed
                .available_buy_amount
                .as_deref()
                .and_then(parse_atomic)
                .map(|a| format_units(a, buy_decimals)),
            buy_symbol.clone(),
        ),
    };
    let available_symbol = available_text.as_ref().map(|_| available_symbol);

    // Was this OUR quote? The venue names the wallets it routed to when it
    // has them, but on the preview path that list comes back empty even for a
    // preview that returned a quote, so an empty list proves nothing. What it
    // does set is `restrictionApplied`: a preview that honoured our
    // `restrictedLiquidityWallets` and still answered can only have been
    // priced by this bot. `null` means the venue didn't say, which the screen
    // must not render as "someone else quoted this".
    let from_this_bot = if only_this_bot {
        let routing = parsed.routing.as_ref();
        let named: &[String] = routing
            .map(|r| r.target_maker_wallets.as_slice())
            .unwrap_or_default();
        let named_us = operator
            .as_deref()
            .is_some_and(|op| named.iter().any(|w| w.eq_ignore_ascii_case(op)));
        let restricted_answer = is_preview && routing.is_some_and(|r| r.restriction_applied);
        // A named list wins: if the venue says who priced it, believe it.
        if named_us || (named.is_empty() && restricted_answer) {
            Some(true)
        } else if named.is_empty() {
            None
        } else {
            Some(false)
        }
    } else {
        None
    };

    let public_origin = venue_origin_from_config(&cfg, None);
    let (swap_url, swap_url_mine) = match (&soft_symbol, &stable_symbol) {
        (Some(soft), Some(stable)) => {
            let (sell, buy) = match direction {
                Direction::UsdtToSoft => (stable.as_str(), soft.as_str()),
                Direction::SoftToUsdt => (soft.as_str(), stable.as_str()),
            };
            (
                swap_url(&public_origin, sell, buy, cfg.chain_id, None),
                operator
                    .as_deref()
                    .and_then(|op| swap_url(&public_origin, sell, buy, cfg.chain_id, Some(op))),
            )
        }
        _ => (None, None),
    };

    let (probe_sell_text, probe_buy_text) = match direction {
        Direction::UsdtToSoft => (Some(format_units(one_stable, pool.debt_decimals)), None),
        Direction::SoftToUsdt => (None, Some(format_units(one_stable, pool.debt_decimals))),
    };

    Ok(Json(QuoteProofResponse {
        direction,
        chain_id: cfg.chain_id,
        pair,
        probe: ProbeBody {
            sell_symbol: sell_symbol.clone(),
            sell_text: probe_sell_text,
            buy_symbol: buy_symbol.clone(),
            buy_text: probe_buy_text,
            restricted_to,
        },
        status: if is_preview {
            "preview".to_string()
        } else {
            "no_quote".to_string()
        },
        ok: is_preview,
        reason: if is_preview {
            None
        } else {
            parsed.reason.clone()
        },
        sell_symbol,
        sell_amount: quote.as_ref().map(|q| q.sell_text.clone()),
        buy_symbol,
        buy_amount: quote.as_ref().map(|q| q.buy_text.clone()),
        quote,
        rate,
        available_text,
        available_symbol,
        retry_after_ms: parsed.retry_after_ms,
        reserved_until: parsed.reserved_until,
        from_this_bot,
        expires_at: None,
        swap_url,
        swap_url_mine,
        raw: data,
        checked_at_unix: now_unix(),
    })
    .into_response())
}

fn parse_direction(direction: Option<&str>, side: Option<&str>) -> Result<Direction, ApiError> {
    if let Some(d) = direction.map(str::trim).filter(|d| !d.is_empty()) {
        return match d {
            "usdtToSoft" => Ok(Direction::UsdtToSoft),
            "softToUsdt" => Ok(Direction::SoftToUsdt),
            other => Err(ApiError::bad_request(format!(
                "direction must be \"usdtToSoft\" or \"softToUsdt\", not {other:?}"
            ))),
        };
    }
    match side.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Direction::UsdtToSoft),
        Some(s) if s.eq_ignore_ascii_case("sell") => Ok(Direction::UsdtToSoft),
        Some(s) if s.eq_ignore_ascii_case("buy") => Ok(Direction::SoftToUsdt),
        Some(other) => Err(ApiError::bad_request(format!(
            "side must be \"buy\" or \"sell\", not {other:?}"
        ))),
    }
}

/// The one venue round trip. A 4xx is the venue refusing this pair (400 with
/// its message); anything else that isn't a clean 200 is the venue being
/// unavailable (502), which the screen treats as transient.
///
/// The exceptions are the 4xx codes that mean "come back shortly" rather than
/// "not this pair": too many requests, and the two timeout codes. The panel
/// proxies every operator's polls from their own address, so a rate limit is
/// a realistic answer, and the Live screen only retries on 502/503/504, so
/// calling one of those a refusal would stop the proof poll for good and put
/// the rate-limit text on screen as if the corridor did not exist.
async fn preview(origin: &str, venue_body: &Value) -> Result<Value, ApiError> {
    let url = format!("{}/v2/rfq/preview", origin.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(VENUE_CONNECT_TIMEOUT)
        .timeout(VENUE_TIMEOUT)
        .build()
        .map_err(|e| ApiError::internal(&anyhow::anyhow!("venue HTTP client: {e}")))?;
    let response = client
        .post(&url)
        .json(venue_body)
        .send()
        .await
        .map_err(|e| venue_down(format!("could not reach {url}: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| venue_down(format!("its answer could not be read: {e}")))?;
    if status.is_client_error() {
        if is_transient_client_error(status) {
            let detail = venue_error_message(&text)
                .map(|m| format!("{status}: {m}"))
                .unwrap_or_else(|| status.to_string());
            return Err(venue_down(detail));
        }
        let message = venue_error_message(&text)
            .unwrap_or_else(|| format!("Textile's quote service refused the request ({status})"));
        return Err(ApiError::bad_request(message));
    }
    if !status.is_success() {
        let detail = venue_error_message(&text)
            .map(|m| format!("{status}: {m}"))
            .unwrap_or_else(|| status.to_string());
        return Err(venue_down(detail));
    }
    let envelope: Value = serde_json::from_str(&text)
        .map_err(|e| venue_down(format!("it answered with a body the panel can't read: {e}")))?;
    envelope
        .get("data")
        .cloned()
        .ok_or_else(|| venue_down("it answered without a data field".to_string()))
}

/// A 4xx that means "ask again in a moment": request timeout, too early, too
/// many requests. Everything else in that range is about this request.
fn is_transient_client_error(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS
    )
}

fn venue_down(detail: String) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        format!(
            "Textile's quote service didn't answer ({detail}). It is usually back within a minute."
        ),
    )
}

fn parse_atomic(s: &str) -> Option<U256> {
    U256::from_str_radix(s.trim(), 10).ok()
}

/// `rateRay` is USDT per one soft token times 1e27, whichever way the probe
/// traded. Returns (USDT per soft, soft per USDT) as text.
fn rates_from_ray(ray: &str) -> Option<(String, String)> {
    let ray: f64 = ray.trim().parse().ok()?;
    if !(ray.is_finite() && ray > 0.0) {
        return None;
    }
    let usdt_per_soft = ray / 1e27;
    Some((fmt_sig(usdt_per_soft, 9), fmt_sig(1.0 / usdt_per_soft, 6)))
}

/// A number to `sig` significant digits, trailing zeros trimmed, never in
/// scientific notation.
pub(super) fn fmt_sig(x: f64, sig: usize) -> String {
    if !x.is_finite() {
        return String::new();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    let exponent = x.abs().log10().floor() as i64;
    let decimals = (sig as i64 - 1 - exponent).max(0) as usize;
    let text = format!("{x:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// The venue's aliases for its stable tickers, so a link never says `USD₮`.
pub(super) fn canonical_symbol(symbol: &str) -> String {
    let s = symbol.trim();
    if s.eq_ignore_ascii_case("USDT0") || s == "USD₮" || s == "usd₮" {
        return "USDT".to_string();
    }
    if s == "G$" {
        return "GD".to_string();
    }
    s.to_string()
}

/// The public swap page for a pair — only when the venue is Textile's public
/// API, because a staging or mock venue has no public page. `restricted`
/// pins the page to one maker's wallet.
pub fn swap_url(
    venue_origin: &str,
    sell: &str,
    buy: &str,
    chain_id: u64,
    restricted: Option<&str>,
) -> Option<String> {
    if venue_origin.trim().trim_end_matches('/') != PUBLIC_VENUE_ORIGIN {
        return None;
    }
    let chain = chain_id.to_string();
    let mut params: Vec<(&str, &str)> =
        vec![("sell", sell), ("buy", buy), ("chainId", chain.as_str())];
    if let Some(wallet) = restricted {
        params.push(("restricted", wallet));
    }
    url::Url::parse_with_params(PUBLIC_SWAP_BASE, &params)
        .ok()
        .map(|u| u.to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::ContainerState;
    use crate::panel::http::testkit::{harness, Harness, TEST_KEY};
    use crate::setup;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::Router;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const USDT: &str = "0x48065fbbe25f71c9282ddf5e1cd6d6a887483d5e";
    const CNGN: &str = "0xf6829d7393dae24509eb1e52ee8e572e2e271a4f";
    /// TEST_KEY's wallet, in the lowercase form the inventory reports it in.
    const OPERATOR: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    /// The same wallet as the venue echoes it: checksummed.
    const OPERATOR_CHECKSUMMED: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    /// The live Celo cNGN/USDT preview of 2026-09-06, scaled to a 1 USDT probe.
    fn sample_preview(wallets: Vec<&str>) -> Value {
        json!({ "data": {
            "status": "preview",
            "sellAmount": "1000000",
            "buyAmount": "1368726683",
            "feeAmount": "100",
            "takerPays": "1000000",
            "rateRay": "730533000000217019222072",
            "routing": {
                "preferenceApplied": false, "restrictionApplied": !wallets.is_empty(),
                "fallbackUsed": false, "targetMakerWallets": wallets,
                "preferredQuotesReceived": 0, "openMarketQuotesReceived": 0
            },
            "availableSellAmount": "24337430690"
        }})
    }

    type Captured = Arc<Mutex<Option<(HeaderMap, Value)>>>;

    async fn mock_venue(
        status: u16,
        reply: Value,
    ) -> (String, Captured, tokio::task::JoinHandle<()>) {
        let seen: Captured = Arc::new(Mutex::new(None));
        let recorder = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/v2/rfq/preview",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let recorder = recorder.clone();
                let reply = reply.clone();
                async move {
                    *recorder.lock().unwrap() = Some((headers, body));
                    (StatusCode::from_u16(status).unwrap(), Json(reply))
                }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), seen, handle)
    }

    fn seed(h: &Harness, name: &str) {
        let corridor = setup::find_corridor("cngn-usdt-celo").unwrap();
        setup::write_config(h.root.join(name), corridor, TEST_KEY).unwrap();
        let mut c = crate::panel::docker::fake::container(
            &format!("stitch-{name}"),
            ContainerState::Exited,
        );
        c.image = h.state.cfg.bot_image.clone();
        c.labels.insert(
            crate::panel::naming::LABEL_BOT.to_string(),
            name.to_string(),
        );
        c.mounts =
            crate::panel::docker::fake::dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
    }

    fn captured(seen: &Captured) -> (HeaderMap, Value) {
        seen.lock().unwrap().clone().expect("the venue was called")
    }

    #[tokio::test]
    async fn quote_proof_sends_a_clean_restricted_preview() {
        let h = harness("proof-restricted");
        seed(&h, "bot-a");
        let (venue, seen, _server) = mock_venue(200, sample_preview(vec![])).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (headers, sent) = captured(&seen);
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
        assert_eq!(sent["chainId"], 42220);
        assert_eq!(sent["sellToken"], USDT);
        assert_eq!(sent["buyToken"], CNGN);
        assert_eq!(sent["sellAmount"], "1000000");
        assert!(sent.get("buyAmount").is_none(), "{sent}");
        assert_eq!(sent["restrictedLiquidityWallets"], json!([OPERATOR]));

        let v = Harness::parse(&body);
        assert_eq!(v["direction"], "usdtToSoft");
        assert_eq!(v["status"], "preview");
        assert_eq!(v["ok"], true);
        assert!(v["reason"].is_null());
        assert_eq!(v["pair"]["softSymbol"], "cNGN");
        assert_eq!(v["pair"]["stableSymbol"], "USDT");
        assert_eq!(v["pair"]["label"], "cNGN / USDT");
        assert_eq!(v["pair"]["networkLabel"], "Celo");
        assert_eq!(v["probe"]["sellSymbol"], "USDT");
        assert_eq!(v["probe"]["sellText"], "1");
        assert_eq!(v["probe"]["buySymbol"], "cNGN");
        assert_eq!(v["probe"]["restrictedTo"], json!([OPERATOR]));
        assert_eq!(v["quote"]["sellText"], "1");
        assert_eq!(v["quote"]["buyText"], "1368.726683");
        assert_eq!(v["quote"]["feeText"], "0.0001");
        assert_eq!(v["quote"]["feeSymbol"], "USDT");
        assert!(
            v["quote"]["usdtPerSoft"]
                .as_str()
                .unwrap()
                .starts_with("0.000730533"),
            "{body}"
        );
        assert!(
            v["quote"]["softPerUsdt"]
                .as_str()
                .unwrap()
                .starts_with("1368.8"),
            "{body}"
        );
        assert_eq!(v["rate"]["allIn"], "1368.73");
        assert_eq!(v["rate"]["preFee"], v["quote"]["softPerUsdt"]);
        assert_eq!(v["sellAmount"], "1");
        assert_eq!(v["buyAmount"], "1368.726683");
        assert_eq!(v["availableText"], "24337.43069");
        assert_eq!(v["availableSymbol"], "USDT", "exact-input depth is in USDT");
        assert!(
            v["fromThisBot"].is_null(),
            "the venue named no wallet and did not say it restricted: unknown, \
             not 'someone else quoted this' — {body}"
        );
        assert!(v["expiresAt"].is_null());
        assert_eq!(
            v["swapUrl"],
            "https://app.textilecredit.com/s/swap?sell=USDT&buy=cNGN&chainId=42220"
        );
        assert_eq!(
            v["swapUrlMine"],
            format!("https://app.textilecredit.com/s/swap?sell=USDT&buy=cNGN&chainId=42220&restricted={OPERATOR}")
        );
        assert_eq!(v["raw"]["status"], "preview");
    }

    #[tokio::test]
    async fn quote_proof_never_sends_the_maker_key() {
        let h = harness("proof-no-key");
        seed(&h, "bot-a");
        setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_enroll_secret").unwrap();
        let (venue, seen, _server) = mock_venue(200, sample_preview(vec![])).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (headers, _) = captured(&seen);
        assert!(headers.get("authorization").is_none(), "{headers:?}");
        assert!(headers.get("x-api-key").is_none(), "{headers:?}");
        assert!(!body.contains("tx_live_enroll_secret"));
    }

    #[tokio::test]
    async fn quote_proof_marks_from_this_bot_when_routed_to_operator() {
        let h = harness("proof-mine");
        seed(&h, "bot-a");
        // The venue returns the wallet checksummed; the panel holds it in
        // lowercase. The match must not care.
        let (venue, _seen, _server) =
            mock_venue(200, sample_preview(vec![OPERATOR_CHECKSUMMED])).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["fromThisBot"], true, "{body}");
    }

    #[tokio::test]
    async fn quote_proof_soft_to_usdt_uses_exact_output() {
        let h = harness("proof-exact-output");
        seed(&h, "bot-a");
        let reply = json!({ "data": {
            "status": "preview",
            "sellAmount": "1368900000",
            "buyAmount": "1000000",
            "feeAmount": "136890",
            "takerPays": "1368900000",
            "rateRay": "730533000000217019222072",
            "routing": { "targetMakerWallets": [] },
            "availableBuyAmount": "5000000"
        }});
        let (venue, seen, _server) = mock_venue(200, reply).await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/quote-proof",
                json!({ "venueUrl": venue, "direction": "softToUsdt" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, sent) = captured(&seen);
        assert_eq!(sent["buyAmount"], "1000000");
        assert!(sent.get("sellAmount").is_none(), "{sent}");
        assert_eq!(sent["sellToken"], CNGN);
        assert_eq!(sent["buyToken"], USDT);

        let v = Harness::parse(&body);
        assert_eq!(v["direction"], "softToUsdt");
        assert_eq!(v["probe"]["sellSymbol"], "cNGN");
        assert!(v["probe"]["sellText"].is_null());
        assert_eq!(v["probe"]["buyText"], "1");
        assert_eq!(v["quote"]["sellText"], "1368.9");
        assert_eq!(v["quote"]["buyText"], "1");
        assert_eq!(v["quote"]["feeSymbol"], "cNGN");
        assert_eq!(v["rate"]["allIn"], "1368.9");
        assert_eq!(v["availableText"], "5");
        assert_eq!(
            v["availableSymbol"], "USDT",
            "exact-output depth is in the BUY token, and says so"
        );
        assert_eq!(
            v["swapUrl"],
            "https://app.textilecredit.com/s/swap?sell=cNGN&buy=USDT&chainId=42220"
        );
    }

    #[tokio::test]
    async fn quote_proof_accepts_the_bots_own_side_words() {
        let h = harness("proof-side-alias");
        seed(&h, "bot-a");
        let (venue, seen, _server) = mock_venue(200, sample_preview(vec![])).await;

        // "buy" = the bot buys the soft token = the taker sells it.
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/quote-proof",
                json!({ "venueUrl": venue, "side": "buy" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["direction"], "softToUsdt");
        assert_eq!(captured(&seen).1["sellToken"], CNGN);

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/quote-proof",
                json!({ "venueUrl": venue, "side": "sideways" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn quote_proof_open_market_when_not_only_this_bot() {
        let h = harness("proof-open");
        seed(&h, "bot-a");
        let (venue, seen, _server) = mock_venue(200, sample_preview(vec![])).await;

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/quote-proof",
                json!({ "venueUrl": venue, "onlyThisBot": false }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, sent) = captured(&seen);
        assert!(sent.get("restrictedLiquidityWallets").is_none(), "{sent}");
        let v = Harness::parse(&body);
        assert!(v["fromThisBot"].is_null(), "{body}");
        assert_eq!(v["probe"]["restrictedTo"], json!([]));
    }

    #[tokio::test]
    async fn quote_proof_passes_no_quote_reason_and_retry_hint() {
        let h = harness("proof-no-makers");
        seed(&h, "bot-a");
        let reply = json!({ "data": {
            "status": "no_quote",
            "reason": "no_makers_online",
            "routing": { "targetMakerWallets": [] },
            "reservedUntil": "2026-09-06T12:00:00.000Z",
            "retryAfterMs": 1500
        }});
        let (venue, _seen, _server) = mock_venue(200, reply).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["status"], "no_quote");
        assert_eq!(v["ok"], false);
        assert_eq!(v["reason"], "no_makers_online");
        assert!(v["quote"].is_null());
        assert!(v["rate"].is_null());
        assert!(v["sellAmount"].is_null());
        assert_eq!(v["retryAfterMs"], 1500);
        assert_eq!(v["reservedUntil"], "2026-09-06T12:00:00.000Z");
        assert!(
            v["fromThisBot"].is_null(),
            "nobody quoted, so nobody to name"
        );
        assert!(v["availableText"].is_null());
        assert!(v["availableSymbol"].is_null());
    }

    /// The live venue leaves `targetMakerWallets` empty even on a preview that
    /// returned a quote, so the wallet match alone would call this bot's own
    /// quote somebody else's. A preview the venue says it restricted could
    /// only have been priced by the wallet we restricted it to.
    #[tokio::test]
    async fn a_restricted_preview_that_answered_is_this_bots_quote() {
        let h = harness("proof-restriction-applied");
        seed(&h, "bot-a");
        let mut reply = sample_preview(vec![]);
        reply["data"]["routing"]["restrictionApplied"] = json!(true);
        let (venue, _seen, _server) = mock_venue(200, reply).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["fromThisBot"], true, "{body}");
    }

    /// The venue named a maker, and it is not us.
    #[tokio::test]
    async fn another_makers_wallet_is_reported_as_not_ours() {
        let h = harness("proof-someone-else");
        seed(&h, "bot-a");
        let (venue, _seen, _server) = mock_venue(
            200,
            sample_preview(vec!["0x000000000000000000000000000000000000dead"]),
        )
        .await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(Harness::parse(&body)["fromThisBot"], false, "{body}");
    }

    #[tokio::test]
    async fn quote_proof_no_valid_quote_converts_available_depth() {
        let h = harness("proof-depth");
        seed(&h, "bot-a");
        let reply = json!({ "data": {
            "status": "no_quote",
            "reason": "no_valid_quote",
            "routing": { "targetMakerWallets": [] },
            "availableSellAmount": "882871"
        }});
        let (venue, _seen, _server) = mock_venue(200, reply).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["reason"], "no_valid_quote");
        assert_eq!(v["availableText"], "0.882871");
        assert_eq!(v["availableSymbol"], "USDT");
    }

    #[tokio::test]
    async fn quote_proof_venue_400_is_400_with_the_venue_message() {
        let h = harness("proof-corridor-unavailable");
        seed(&h, "bot-a");
        let reply = json!({ "error": {
            "code": "invalid_request",
            "message": "No RFQ corridor for this pair on chain 42220",
            "request_id": "req_1",
            "details": { "reason": "corridor_unavailable" }
        }});
        let (venue, _seen, _server) = mock_venue(400, reply).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            Harness::parse(&body)["error"],
            "No RFQ corridor for this pair on chain 42220"
        );
    }

    /// 429 is not a refusal of the pair. It has to arrive as a 502 so the Live
    /// screen keeps retrying instead of stopping the proof poll for good.
    #[tokio::test]
    async fn quote_proof_venue_429_is_502_not_a_refusal() {
        let h = harness("proof-venue-429");
        seed(&h, "bot-a");
        let reply = json!({ "error": { "code": "rate_limited", "message": "Too many requests" } });
        let (venue, _seen, _server) = mock_venue(429, reply).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        let error = Harness::parse(&body)["error"].as_str().unwrap().to_string();
        assert!(
            error.starts_with("Textile's quote service didn't answer"),
            "{error}"
        );
        assert!(error.contains("Too many requests"), "{error}");
    }

    #[tokio::test]
    async fn quote_proof_venue_503_is_502() {
        let h = harness("proof-venue-503");
        seed(&h, "bot-a");
        let reply = json!({ "error": { "code": "venue_unavailable", "message": "Quotes are briefly unavailable" } });
        let (venue, _seen, _server) = mock_venue(503, reply).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        let error = Harness::parse(&body)["error"].as_str().unwrap().to_string();
        assert!(
            error.starts_with("Textile's quote service didn't answer"),
            "{error}"
        );
        assert!(error.contains("Quotes are briefly unavailable"), "{error}");
    }

    #[tokio::test]
    async fn quote_proof_unreachable_venue_is_502_fast() {
        let h = harness("proof-venue-down");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap().replace(
            "indexer_url     = \"https://api.textilecredit.com\"",
            "indexer_url = \"http://127.0.0.1:1\"",
        );
        std::fs::write(&path, toml).unwrap();

        let started = Instant::now();
        let (status, body) = h.post_json("/api/bots/bot-a/quote-proof", json!({})).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(body.contains("didn't answer"), "{body}");
    }

    #[tokio::test]
    async fn quote_proof_rejects_bad_pool_index() {
        let h = harness("proof-bad-pool");
        seed(&h, "bot-a");
        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "pool": 3 }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(Harness::parse(&body)["error"], "this bot has no pool 3");
    }

    #[tokio::test]
    async fn quote_proof_custom_pool_has_no_swap_url() {
        let h = harness("proof-custom");
        seed(&h, "bot-a");
        let path = h.root.join("bot-a").join("stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap().replace(
            "0xF6829D7393dAe24509eb1E52eE8e572e2E271a4f",
            "0x1111111111111111111111111111111111111111",
        );
        std::fs::write(&path, toml).unwrap();
        let (venue, seen, _server) = mock_venue(200, sample_preview(vec![])).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["swapUrl"].is_null(), "{body}");
        assert!(v["swapUrlMine"].is_null());
        assert_eq!(v["pair"]["softSymbol"], "0x1111…1111");
        assert!(v["pair"]["label"].is_null());
        assert_eq!(
            captured(&seen).1["buyToken"],
            "0x1111111111111111111111111111111111111111"
        );
    }

    #[tokio::test]
    async fn quote_proof_needs_operator_for_only_this_bot() {
        let h = harness("proof-no-operator");
        seed(&h, "bot-a");
        std::fs::remove_file(h.root.join("bot-a").join("stitch.key")).unwrap();
        let (venue, _seen, _server) = mock_venue(200, sample_preview(vec![])).await;

        let (status, body) = h
            .post_json("/api/bots/bot-a/quote-proof", json!({ "venueUrl": venue }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("can't be tied to it"), "{body}");

        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/quote-proof",
                json!({ "venueUrl": venue, "onlyThisBot": false }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert!(v["fromThisBot"].is_null());
        assert!(v["swapUrlMine"].is_null(), "no wallet to pin the page to");
    }

    #[tokio::test]
    async fn quote_proof_unknown_bot_is_a_404() {
        let h = harness("proof-missing");
        let (status, body) = h.post_json("/api/bots/nope/quote-proof", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[test]
    fn swap_urls_only_point_at_the_public_venue() {
        assert_eq!(
            swap_url(PUBLIC_VENUE_ORIGIN, "USDT", "cNGN", 42220, None).as_deref(),
            Some("https://app.textilecredit.com/s/swap?sell=USDT&buy=cNGN&chainId=42220")
        );
        assert_eq!(
            swap_url(
                "https://api.textilecredit.com/",
                "cNGN",
                "USDT",
                56,
                Some(OPERATOR_CHECKSUMMED)
            )
            .as_deref(),
            Some(&format!(
                "https://app.textilecredit.com/s/swap?sell=cNGN&buy=USDT&chainId=56&restricted={OPERATOR_CHECKSUMMED}"
            )[..])
        );
        assert_eq!(
            swap_url("http://127.0.0.1:9", "USDT", "cNGN", 42220, None),
            None
        );
        assert_eq!(
            swap_url(
                "https://staging.textilecredit.com",
                "USDT",
                "cNGN",
                42220,
                None
            ),
            None
        );
    }

    #[test]
    fn stable_tickers_are_canonical() {
        assert_eq!(canonical_symbol("USD₮"), "USDT");
        assert_eq!(canonical_symbol("usdt0"), "USDT");
        assert_eq!(canonical_symbol("USDT"), "USDT");
        assert_eq!(canonical_symbol("USDC"), "USDC");
        assert_eq!(canonical_symbol("G$"), "GD");
    }

    #[test]
    fn rates_come_out_of_the_ray_both_ways() {
        let (usdt_per_soft, soft_per_usdt) = rates_from_ray("730533000000217019222072").unwrap();
        assert_eq!(usdt_per_soft, "0.000730533");
        assert_eq!(soft_per_usdt, "1368.86");
        assert_eq!(rates_from_ray("0"), None);
        assert_eq!(rates_from_ray("x"), None);
        assert_eq!(fmt_sig(1368.726683, 6), "1368.73");
        assert_eq!(fmt_sig(5.0, 6), "5");
        assert_eq!(fmt_sig(0.0, 6), "0");
    }

    #[test]
    fn direction_parses_both_vocabularies() {
        assert_eq!(parse_direction(None, None).unwrap(), Direction::UsdtToSoft);
        assert_eq!(
            parse_direction(Some("softToUsdt"), Some("sell")).unwrap(),
            Direction::SoftToUsdt,
            "direction wins over side"
        );
        assert_eq!(
            parse_direction(None, Some("SELL")).unwrap(),
            Direction::UsdtToSoft
        );
        assert_eq!(
            parse_direction(None, Some("buy")).unwrap(),
            Direction::SoftToUsdt
        );
        assert!(parse_direction(Some("up"), None).is_err());
    }
}
