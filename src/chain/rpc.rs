// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Minimal Ethereum JSON-RPC client and a signing [`Wallet`] that lands
//! transactions: fill nonce, gas, and EIP-1559 fees from the node, sign with
//! [`crate::chain::tx`], broadcast via `eth_sendRawTransaction`, and (optionally) wait
//! for the receipt. Reads go through `eth_call`. Just enough RPC for the
//! blue-leg closer — no provider framework, same reqwest client the indexer and
//! subgraph already use.

use std::time::Duration;

use alloy_primitives::{hex, Address, Bytes, B256, U256};
use serde_json::{json, Value};

use crate::chain::gas_caps::GasCaps;
use crate::chain::tx::{sign_tx, Eip1559Tx};
use crate::net::http_client;
use crate::signer::DynSigner;

/// Default priority fee when the node has no `eth_maxPriorityFeePerGas`: 1 gwei.
const DEFAULT_PRIORITY_WEI: u64 = 1_000_000_000;

/// Absolute floor for the tip, 0.05 gwei — BNB Chain's minimum relay price and
/// well under a cent of gas on every chain we run. Providers do not agree on
/// what to answer for a zero-base-fee chain, and one that answers `0` would
/// otherwise have us sign a free transaction: peers refuse to relay it, so it
/// never propagates, never mines, and never errors either. It just disappears.
const MIN_PRIORITY_WEI: u64 = 50_000_000;

/// Tip and fee cap for one send, from the chain's base fee and the node's two
/// price suggestions.
///
/// `eth_gasPrice` is a *whole* price (base + tip), not a base fee — treating it
/// as one inflates the cap on chains with a real base fee and says nothing about
/// the tip. Back the tip out of it instead, take whichever of the two
/// suggestions is higher, and never bid below [`MIN_PRIORITY_WEI`]. On a
/// zero-base-fee chain the tip is the entire bid, so this is the whole price.
fn plan_fees(base_fee: U256, suggested_gas_price: U256, node_tip: U256) -> (U256, U256) {
    let implied_tip = suggested_gas_price.saturating_sub(base_fee);
    let priority = node_tip.max(implied_tip).max(U256::from(MIN_PRIORITY_WEI));
    // Double the base fee for headroom against a rise between planning and
    // inclusion; the tip rides on top.
    // Saturating: these are the node's numbers, and a huge answer has to reach
    // the `GasCaps` check in `plan_tx`, not overflow here.
    let max_fee = base_fee
        .saturating_mul(U256::from(2u8))
        .saturating_add(priority);
    (priority, max_fee)
}

/// The pair [`plan_fees`] works from — `(suggested_gas_price, node_tip)` —
/// given what the node actually answered.
///
/// A price the node *did* answer always wins. Substituting
/// [`DEFAULT_PRIORITY_WEI`] for a missing `eth_maxPriorityFeePerGas` outbids a
/// live `eth_gasPrice`, because `plan_fees` takes the higher of the two and
/// cannot tell a made-up number from a real one: on BNB Chain that turns a
/// 0.1 gwei chain into a 1 gwei bid, ten times the reserve an approval needs
/// up front. Only when neither call answers is there nothing to derive a bid
/// from, and the flat default stands in.
fn node_prices(node_tip: Option<U256>, gas_price: Option<U256>) -> (U256, U256) {
    let fallback = match (node_tip, gas_price) {
        (None, None) => U256::from(DEFAULT_PRIORITY_WEI),
        _ => U256::ZERO,
    };
    (gas_price.unwrap_or(fallback), node_tip.unwrap_or(fallback))
}

/// How long to wait for a receipt before re-sending the same nonce at a higher
/// fee. We bid what the node suggests, and on a zero-base-fee chain that is the
/// floor and nothing more: BNB Chain answers `eth_maxPriorityFeePerGas` with
/// 0.05 gwei while the median transaction pays 0.1, so the whole `max_fee`
/// headroom is dead weight — the tip *is* the bid. A floor-priced tx lands only
/// when a validator that accepts the floor builds the block, so it can sit for
/// minutes. Leaving it there is worse than slow: the next send reads the
/// *pending* nonce and queues behind it, and for a 7702-delegated account the
/// node refuses that outright (one in-flight tx per delegated account). Escalate
/// often enough to clear the median within a minute.
const BUMP_AFTER: Duration = Duration::from_secs(12);

/// Fee multiplier per re-send. geth only accepts a same-nonce replacement when
/// both fee fields rise at least 10%; 25% clears that with room for rounding.
const BUMP_NUMERATOR: u64 = 125;
const BUMP_DENOMINATOR: u64 = 100;

/// Gas an empty value transfer takes on every EVM chain: the floor for a
/// reserve, so a lying estimate can't strand dust.
const NATIVE_TRANSFER_GAS: u64 = 21_000;

/// EIP-7702 delegation designator: an EOA that has been "upgraded" carries
/// exactly `0xef0100 || delegate` as its code.
const EIP7702_DESIGNATOR: [u8; 3] = [0xef, 0x01, 0x00];

/// The contract an EOA's code delegates to under EIP-7702, if the code is a
/// delegation designator. Plain EOAs (empty code) and ordinary contracts both
/// give `None` — use the raw code to tell those two apart.
pub fn delegation_target(code: &[u8]) -> Option<Address> {
    if code.len() == 23 && code[..3] == EIP7702_DESIGNATOR {
        Some(Address::from_slice(&code[3..]))
    } else {
        None
    }
}

/// True when the node refused the send because the sender is 7702-delegated and
/// already has a transaction in the pool (geth's `ErrInflightTxLimitReached`).
pub fn is_inflight_limit_error(err: &str) -> bool {
    err.contains("in-flight transaction limit")
}

/// Marker for a send whose node may have accepted the transaction before the
/// caller lost certainty about its outcome. Retrying with the next nonce can
/// spend gas twice, so callers that coordinate one-shot work must keep their
/// claim until chain state proves what happened.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct TransactionMayStillLand {
    message: String,
    /// The account nonce every attempt was signed at. Once the chain's latest
    /// nonce passes it, this transaction has either landed or never will.
    nonce: u64,
}

fn may_still_land_error(err: anyhow::Error, nonce: u64) -> anyhow::Error {
    TransactionMayStillLand {
        message: format!("{err:#}"),
        nonce,
    }
    .into()
}

/// Whether a failed send crossed the broadcast boundary and therefore must
/// not be retried merely because no receipt was observed.
pub fn transaction_may_still_land(err: &anyhow::Error) -> bool {
    unresolved_nonce(err).is_some()
}

/// The nonce of a send that may still land, for reconciling it against the
/// chain later. `None` for a send that definitely did not land.
pub fn unresolved_nonce(err: &anyhow::Error) -> Option<u64> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<TransactionMayStillLand>())
        .map(|e| e.nonce)
}

/// The operator-facing explanation for geth's in-flight limit. `stuck_nonce` is
/// the nonce the pool is already holding (latest, i.e. the one our send skipped).
fn inflight_hint(delegate: Option<Address>, stuck_nonce: Option<u64>) -> String {
    let who = match delegate {
        Some(d) => format!("This wallet is an EIP-7702 delegated smart account (delegated to {d})"),
        None => "This wallet is a delegated account".to_string(),
    };
    let nonce = match stuck_nonce {
        Some(n) => format!(" It already has a transaction pending at nonce {n}."),
        None => String::new(),
    };
    format!(
        "{who}, and the node allows such an account only one transaction in flight at a time.{nonce}\n\
         Clear it by re-sending that same nonce from your wallet with a higher gas price (a \"speed up\" \
         or a 0-value self-transfer with the nonce set by hand), then retry.\n\
         Textile also does not accept delegated wallets as makers, so revoke the delegation before going \
         live: in MetaMask, Settings -> switch the account back to a standard account, or run Stitch from \
         a fresh EOA that was never upgraded."
    )
}

fn parse_quantity(v: &Value) -> anyhow::Result<U256> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("expected a hex quantity, got {v}"))?;
    let trimmed = s.strip_prefix("0x").unwrap_or(s);
    if trimmed.is_empty() {
        return Ok(U256::ZERO);
    }
    U256::from_str_radix(trimmed, 16).map_err(|e| anyhow::anyhow!("bad quantity {s}: {e}"))
}

/// Most fallback endpoints a bot may list after its primary `rpc_url`.
pub const MAX_RPC_FALLBACKS: usize = 3;

/// How long a client stays on a fallback before it offers the primary a call
/// again. Short enough that a primary back from a blip takes over within a
/// minute, long enough that a dead one costs at most one failed attempt a
/// minute rather than one per call.
const PRIMARY_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How long the other endpoints get to report a pending nonce, once one
/// endpoint has. See [`Rpc::transaction_count`].
const NONCE_CROSSCHECK_BUDGET: Duration = Duration::from_secs(3);

/// The JSON-RPC endpoints one bot talks to: the primary `rpc_url` first, then
/// the operator's fallbacks in the order they listed them.
///
/// Fallbacks only exist together with the chain id they must serve. A client
/// checks `eth_chainId` on a fallback before its first call there and skips
/// one that answers for another network, so a mistyped fallback can never
/// feed balances or prices from the wrong chain. The primary isn't checked:
/// it is the endpoint the bot always ran against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RpcEndpoints {
    /// Never empty. Index 0 is the primary.
    urls: Vec<String>,
    /// `Some` whenever `urls` has fallbacks.
    chain_id: Option<u64>,
}

impl RpcEndpoints {
    /// A primary with fallbacks. Blank and repeated fallbacks are dropped, so
    /// the list is the distinct endpoints in the order to try them.
    pub fn with_fallbacks(
        primary: impl Into<String>,
        fallbacks: impl IntoIterator<Item = String>,
        chain_id: u64,
    ) -> Self {
        let primary = primary.into();
        let urls = fallbacks
            .into_iter()
            .map(|url| url.trim().to_string())
            .fold(vec![primary], |mut urls, url| {
                if !url.is_empty() && !urls.contains(&url) {
                    urls.push(url);
                }
                urls
            });
        Self {
            urls,
            chain_id: Some(chain_id),
        }
    }

    pub fn primary(&self) -> &str {
        &self.urls[0]
    }

    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    fn has_fallbacks(&self) -> bool {
        self.urls.len() > 1
    }
}

impl From<String> for RpcEndpoints {
    fn from(url: String) -> Self {
        Self {
            urls: vec![url],
            chain_id: None,
        }
    }
}

impl From<&str> for RpcEndpoints {
    fn from(url: &str) -> Self {
        url.to_string().into()
    }
}

impl From<&String> for RpcEndpoints {
    fn from(url: &String) -> Self {
        url.clone().into()
    }
}

impl From<&RpcEndpoints> for RpcEndpoints {
    fn from(endpoints: &RpcEndpoints) -> Self {
        endpoints.clone()
    }
}

/// An endpoint's host for logs. RPC URLs routinely carry an API key in the
/// path or query, so neither is printed.
fn endpoint_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "an unparseable URL".to_string())
}

/// Which endpoint calls go to first, and since when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Route {
    active: usize,
    since: std::time::Instant,
}

impl Route {
    /// On a fallback long enough that the primary gets another try.
    fn primary_due(self, now: std::time::Instant) -> bool {
        self.active != 0 && now.duration_since(self.since) >= PRIMARY_RETRY_AFTER
    }
}

/// Endpoint indices in the order one call tries them: the active endpoint (or
/// the primary, once it's due another try), then every other in list order.
fn attempt_order(len: usize, route: Route, now: std::time::Instant) -> Vec<usize> {
    let first = if route.primary_due(now) {
        0
    } else {
        route.active
    };
    std::iter::once(first)
        .chain((0..len).filter(|&i| i != first))
        .collect()
}

/// The route a call plans from, and the route it leaves behind. When the
/// primary is due, the first call to plan takes the retry: it tries the
/// primary first and restarts the wait, so calls planned meanwhile stay on
/// the fallback instead of all queueing on a primary that may hang.
fn claim_retry(route: Route, now: std::time::Instant) -> Route {
    if route.primary_due(now) {
        Route {
            since: now,
            ..route
        }
    } else {
        route
    }
}

/// The route after endpoint `served` answered a call planned from `route`.
fn route_after(route: Route, served: usize, now: std::time::Instant) -> Route {
    if served == route.active {
        route
    } else {
        Route {
            active: served,
            since: now,
        }
    }
}

/// Failover state for one endpoint list, shared by every [`Rpc`] built from
/// it in this process. Wallets are built per send, so state kept on one
/// client would forget a dead primary on the next transaction.
#[derive(Debug)]
struct Failover {
    route: std::sync::Mutex<Route>,
    /// Per endpoint: `None` until its chain id is checked, then whether it
    /// matched. The primary is always `Some(true)`.
    chain_ok: std::sync::Mutex<Vec<Option<bool>>>,
}

impl Failover {
    fn new(len: usize) -> Self {
        Self {
            route: std::sync::Mutex::new(Route {
                active: 0,
                since: std::time::Instant::now(),
            }),
            chain_ok: std::sync::Mutex::new(
                std::iter::once(Some(true))
                    .chain(std::iter::repeat_n(None, len - 1))
                    .collect(),
            ),
        }
    }

    #[cfg(test)]
    fn route(&self) -> Route {
        *self.route.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The order one call tries `len` endpoints in, and the route it was
    /// planned from (to hand back to [`Self::served`]).
    fn plan(&self, len: usize) -> (Route, Vec<usize>) {
        let now = std::time::Instant::now();
        let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        let order = attempt_order(len, *route, now);
        *route = claim_retry(*route, now);
        (*route, order)
    }

    fn served(&self, planned: Route, served: usize, endpoints: &RpcEndpoints) {
        let now = std::time::Instant::now();
        let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        // Another call may have moved the route while this one was in flight;
        // the newer move wins.
        if *route != planned {
            return;
        }
        let next = route_after(planned, served, now);
        if next.active != planned.active {
            if served == 0 {
                tracing::info!(
                    endpoint = %endpoint_host(endpoints.primary()),
                    "primary RPC is answering again; switched back to it"
                );
            } else {
                tracing::warn!(
                    endpoint = %endpoint_host(&endpoints.urls()[served]),
                    fallback = served,
                    "RPC failover: calls now go to fallback {served}"
                );
            }
        }
        *route = next;
    }

    fn chain_ok(&self, idx: usize) -> Option<bool> {
        self.chain_ok.lock().unwrap_or_else(|e| e.into_inner())[idx]
    }

    fn set_chain_ok(&self, idx: usize, ok: bool) {
        self.chain_ok.lock().unwrap_or_else(|e| e.into_inner())[idx] = Some(ok);
    }
}

fn failover_for(endpoints: &RpcEndpoints) -> std::sync::Arc<Failover> {
    type Registry = std::collections::HashMap<RpcEndpoints, std::sync::Arc<Failover>>;
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Registry>> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(endpoints.clone())
        .or_insert_with(|| std::sync::Arc::new(Failover::new(endpoints.urls().len())))
        .clone()
}

/// Low-level JSON-RPC transport over one or more endpoints.
///
/// With fallbacks, a call that can't get an answer out of an endpoint (the
/// connection fails, it times out, it answers a non-2xx status or a body that
/// isn't JSON) moves on to the next. A JSON-RPC `error` is a real answer —
/// a revert, a rejected transaction — and is returned as is: asking another
/// node would only get the same answer, or worse, a different one.
#[derive(Clone)]
pub struct Rpc {
    endpoints: RpcEndpoints,
    /// `None` for a single endpoint, which never fails over.
    failover: Option<std::sync::Arc<Failover>>,
    client: reqwest::Client,
}

impl Rpc {
    pub fn new(endpoints: impl Into<RpcEndpoints>) -> Self {
        let endpoints = endpoints.into();
        let failover = endpoints.has_fallbacks().then(|| failover_for(&endpoints));
        Self {
            endpoints,
            failover,
            client: http_client(),
        }
    }

    /// Build a JSON-RPC request envelope (pure — easy to assert on).
    pub fn request(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    }

    async fn post(&self, url: &str, body: &Value) -> reqwest::Result<Value> {
        self.client
            .post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    /// Whether endpoint `idx` serves this bot's chain, asking it once.
    /// `false` covers both a wrong chain (remembered) and no answer (asked
    /// again next time).
    async fn serves_chain(&self, failover: &Failover, idx: usize) -> bool {
        if let Some(ok) = failover.chain_ok(idx) {
            return ok;
        }
        let url = &self.endpoints.urls()[idx];
        let answer = self
            .post(url, &Self::request("eth_chainId", json!([])))
            .await
            .ok()
            .and_then(|resp| resp.get("result").cloned())
            .and_then(|v| parse_quantity(&v).ok());
        let Some(reported) = answer else {
            return false;
        };
        let ok = self
            .endpoints
            .chain_id
            .is_some_and(|want| reported == U256::from(want));
        if !ok {
            tracing::error!(
                endpoint = %endpoint_host(url),
                fallback = idx,
                reported = %reported,
                expected = ?self.endpoints.chain_id,
                "fallback RPC serves a different chain; it will never be used"
            );
        }
        failover.set_chain_ok(idx, ok);
        ok
    }

    async fn response(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = Self::request(method, params);
        let Some(failover) = &self.failover else {
            return Ok(self.post(self.endpoints.primary(), &body).await?);
        };
        let (planned, order) = failover.plan(self.endpoints.urls().len());
        let mut failures = Vec::new();
        for idx in order {
            if !self.serves_chain(failover, idx).await {
                failures.push(format!("{}: not usable for this chain", self.label(idx)));
                continue;
            }
            match self.post(&self.endpoints.urls()[idx], &body).await {
                Ok(resp) => {
                    failover.served(planned, idx, &self.endpoints);
                    return Ok(resp);
                }
                Err(err) => failures.push(format!(
                    "{}: {:#}",
                    self.label(idx),
                    self.transport_error(err)
                )),
            }
        }
        anyhow::bail!(
            "rpc {method}: no RPC endpoint answered ({})",
            failures.join("; ")
        )
    }

    /// A transport failure as an error. With fallbacks the endpoint is named
    /// by [`Self::label`], so reqwest's copy of the full URL, often with an
    /// API key in it, is dropped. A single endpoint keeps it, as it always has.
    fn transport_error(&self, err: reqwest::Error) -> anyhow::Error {
        match self.failover {
            Some(_) => err.without_url().into(),
            None => err.into(),
        }
    }

    /// How logs and errors name endpoint `idx`, without its path or key.
    fn label(&self, idx: usize) -> String {
        let host = endpoint_host(&self.endpoints.urls()[idx]);
        match idx {
            0 => format!("primary {host}"),
            n => format!("fallback {n} {host}"),
        }
    }

    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let resp = self.response(method, params).await?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("rpc {method} error: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("rpc {method}: no result"))
    }

    pub async fn chain_id(&self) -> anyhow::Result<u64> {
        Ok(parse_quantity(&self.call("eth_chainId", json!([])).await?)?.to::<u64>())
    }

    /// Pending nonce for `addr` (counts queued txs, so approve→fill chains work).
    ///
    /// With fallbacks, the highest count any usable endpoint reports. A
    /// transaction broadcast through one endpoint may not be in another's
    /// pool yet, and reading the lower count there would sign a second,
    /// different transaction at a nonce already in flight. Erring high can
    /// only queue the next send behind one that is pending somewhere. The
    /// other endpoints get [`NONCE_CROSSCHECK_BUDGET`] to answer; one that
    /// doesn't is left out.
    pub async fn transaction_count(&self, addr: Address) -> anyhow::Result<u64> {
        let params = json!([addr.to_string(), "pending"]);
        let first = parse_quantity(&self.call("eth_getTransactionCount", params.clone()).await?)?
            .to::<u64>();
        let Some(failover) = &self.failover else {
            return Ok(first);
        };
        let body = Self::request("eth_getTransactionCount", params);
        let asks = (0..self.endpoints.urls().len()).map(|idx| {
            let body = &body;
            async move {
                let ask = async {
                    if !self.serves_chain(failover, idx).await {
                        return None;
                    }
                    let resp = self.post(&self.endpoints.urls()[idx], body).await.ok()?;
                    parse_quantity(resp.get("result")?)
                        .ok()
                        .map(|n| n.to::<u64>())
                };
                tokio::time::timeout(NONCE_CROSSCHECK_BUDGET, ask)
                    .await
                    .ok()
                    .flatten()
            }
        });
        Ok(futures_util::future::join_all(asks)
            .await
            .into_iter()
            .flatten()
            .fold(first, u64::max))
    }

    pub async fn gas_price(&self) -> anyhow::Result<U256> {
        parse_quantity(&self.call("eth_gasPrice", json!([])).await?)
    }

    /// Native-token balance of `addr` at the latest block, in wei.
    pub async fn get_balance(&self, addr: Address) -> anyhow::Result<U256> {
        parse_quantity(
            &self
                .call("eth_getBalance", json!([addr.to_string(), "latest"]))
                .await?,
        )
    }

    /// Code at `addr` at the latest block. Empty for a plain EOA.
    pub async fn get_code(&self, addr: Address) -> anyhow::Result<Bytes> {
        let r = self
            .call("eth_getCode", json!([addr.to_string(), "latest"]))
            .await?;
        let s = r.as_str().unwrap_or("0x");
        Ok(Bytes::from(hex::decode(s.strip_prefix("0x").unwrap_or(s))?))
    }

    /// Nonce for `addr` at the latest block — the next nonce the chain has
    /// actually consumed, ignoring anything still sitting in the pool.
    pub async fn latest_transaction_count(&self, addr: Address) -> anyhow::Result<u64> {
        let r = self
            .call(
                "eth_getTransactionCount",
                json!([addr.to_string(), "latest"]),
            )
            .await?;
        Ok(parse_quantity(&r)?.to::<u64>())
    }

    /// `baseFeePerGas` of the latest block. Zero on BNB Chain, where the tip is
    /// the whole price.
    pub async fn base_fee(&self) -> anyhow::Result<U256> {
        let b = self
            .call("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        match b.get("baseFeePerGas") {
            Some(v) => parse_quantity(v),
            None => Ok(U256::ZERO),
        }
    }

    pub async fn max_priority_fee(&self) -> anyhow::Result<U256> {
        parse_quantity(&self.call("eth_maxPriorityFeePerGas", json!([])).await?)
    }

    pub async fn estimate_gas(
        &self,
        from: Address,
        to: Address,
        data: &Bytes,
        value: U256,
    ) -> anyhow::Result<U256> {
        let tx = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "data": hex::encode_prefixed(data),
            "value": format!("0x{:x}", value),
        });
        parse_quantity(&self.call("eth_estimateGas", json!([tx])).await?)
    }

    /// `eth_call` against the latest block; returns the raw return bytes.
    pub async fn eth_call(&self, to: Address, data: &Bytes) -> anyhow::Result<Bytes> {
        let tx = json!({ "to": to.to_string(), "data": hex::encode_prefixed(data) });
        let r = self.call("eth_call", json!([tx, "latest"])).await?;
        let s = r.as_str().unwrap_or("0x");
        Ok(Bytes::from(hex::decode(s.strip_prefix("0x").unwrap_or(s))?))
    }

    pub async fn send_raw(&self, raw: &Bytes) -> anyhow::Result<B256> {
        self.send_raw_tracked(raw)
            .await
            .map_err(SendRawFailure::into_error)
    }

    /// Submit a signed transaction without erasing whether the node gave a
    /// definitive JSON-RPC rejection or the transport lost the outcome.
    ///
    /// With fallbacks, a lost outcome moves on to the next endpoint with the
    /// same signed bytes, which can only land once, and keeps going until one
    /// accepts them or every endpoint has had them. A rejection is final only
    /// from the first endpoint that got the bytes: after a lost outcome an
    /// endpoint that refuses them may be refusing a transaction that already
    /// reached the pool ("already known", "nonce too low"), so the outcome
    /// stays unknown and the caller keeps its nonce claim.
    async fn send_raw_tracked(&self, raw: &Bytes) -> Result<B256, SendRawFailure> {
        let body = Self::request("eth_sendRawTransaction", json!([hex::encode_prefixed(raw)]));
        let Some(failover) = &self.failover else {
            return self.send_raw_to(self.endpoints.primary(), &body).await;
        };
        let (planned, order) = failover.plan(self.endpoints.urls().len());
        let mut lost: Vec<String> = Vec::new();
        for idx in order {
            if !self.serves_chain(failover, idx).await {
                continue;
            }
            match self.send_raw_to(&self.endpoints.urls()[idx], &body).await {
                Ok(hash) => {
                    failover.served(planned, idx, &self.endpoints);
                    return Ok(hash);
                }
                Err(SendRawFailure::Rejected(err)) if lost.is_empty() => {
                    return Err(SendRawFailure::Rejected(err));
                }
                // After a lost outcome no single answer is final, so every
                // remaining endpoint still gets the bytes: one may accept
                // them where this one refused (a rate limit, a lagging pool).
                Err(SendRawFailure::Rejected(err) | SendRawFailure::Unknown(err)) => {
                    lost.push(format!("{}: {err:#}", self.label(idx)));
                }
            }
        }
        // The primary is always tried, so `lost` is empty only if nothing
        // was ever sent, which is safe to retry.
        Err(if lost.is_empty() {
            SendRawFailure::Rejected(anyhow::anyhow!("no RPC endpoint was usable"))
        } else {
            SendRawFailure::Unknown(anyhow::anyhow!(
                "broadcast outcome unknown: {}",
                lost.join("; ")
            ))
        })
    }

    /// One `eth_sendRawTransaction` to one endpoint.
    async fn send_raw_to(&self, url: &str, body: &Value) -> Result<B256, SendRawFailure> {
        let method = "eth_sendRawTransaction";
        let response = self
            .post(url, body)
            .await
            .map_err(|err| SendRawFailure::Unknown(self.transport_error(err)))?;
        if let Some(err) = response.get("error") {
            return Err(SendRawFailure::Rejected(anyhow::anyhow!(
                "rpc {method} error: {err}"
            )));
        }
        let result = response
            .get("result")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SendRawFailure::Unknown(anyhow::anyhow!("rpc {method}: no transaction hash"))
            })?;
        result
            .parse()
            .map_err(|err| SendRawFailure::Unknown(anyhow::Error::new(err)))
    }

    pub async fn receipt(&self, hash: B256) -> anyhow::Result<Option<Value>> {
        let r = self
            .call("eth_getTransactionReceipt", json!([hash.to_string()]))
            .await?;
        Ok(if r.is_null() { None } else { Some(r) })
    }
}

#[derive(Debug)]
enum SendRawFailure {
    /// The node returned a JSON-RPC error, so it did not accept the tx.
    Rejected(anyhow::Error),
    /// The request or response failed without proving whether the node accepted it.
    Unknown(anyhow::Error),
}

impl SendRawFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Rejected(err) | Self::Unknown(err) => err,
        }
    }
}

type NonceLock = std::sync::Arc<tokio::sync::Mutex<()>>;

/// The lock every [`Wallet`] for `address` on `chain_id` takes to pick a nonce
/// and broadcast under it, shared across the process. Without it two senders
/// on one key (the liquidity schedule, a redeem-epoch close) can both read the
/// same pending nonce, then sign different calldata at it, and one replaces
/// the other. Held until the first broadcast: once a transaction is in the
/// pool the pending count moves past it, and fee bumps reuse their own nonce.
/// A signer that waits for approval holds it while it waits, which is right:
/// no other transaction from that account can get a nonce until it signs.
fn nonce_lock(chain_id: u64, address: Address) -> NonceLock {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(u64, Address), NonceLock>>,
    > = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry((chain_id, address))
        .or_default()
        .clone()
}

/// Durable reservations for externally broadcast funding transactions. Loaded
/// before run loops start; every Wallet for the account checks the same file.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ExternalIntent {
    pub operation: String,
    pub nonce: u64,
    pub raw: String,
}
type JournalPaths = std::collections::HashMap<(u64, Address), std::path::PathBuf>;
fn external_journals() -> &'static std::sync::Mutex<JournalPaths> {
    static PATHS: std::sync::OnceLock<std::sync::Mutex<JournalPaths>> = std::sync::OnceLock::new();
    PATHS.get_or_init(Default::default)
}
pub fn register_external_journal(chain: u64, owner: Address, path: std::path::PathBuf) {
    external_journals()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((chain, owner), path);
}
fn external_path(chain: u64, owner: Address) -> Option<std::path::PathBuf> {
    external_journals()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(chain, owner))
        .cloned()
}
pub fn read_external_intent(path: &std::path::Path) -> anyhow::Result<Option<ExternalIntent>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save_external_intent(path: &std::path::Path, intent: &ExternalIntent) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // A partial file after a crash fails closed. No bytes have left this process.
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(intent)?)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// A signing wallet over an [`Rpc`]: turns calldata into landed transactions.
/// The signer is shared (`Arc`) with the green-leg poster.
pub struct Wallet {
    rpc: Rpc,
    signer: DynSigner,
    address: Address,
    chain_id: u64,
    /// What a send may bid, whatever the node suggests.
    gas_caps: GasCaps,
}

impl Wallet {
    /// A wallet with `chain_id`'s built-in gas ceilings. Use
    /// [`Self::with_gas_caps`] to apply the operator's `[gas]` overrides.
    pub fn new(rpc: impl Into<RpcEndpoints>, signer: DynSigner, chain_id: u64) -> Self {
        let address = signer.address();
        Self {
            rpc: Rpc::new(rpc),
            signer,
            address,
            chain_id,
            gas_caps: GasCaps::for_chain(chain_id),
        }
    }

    pub fn with_gas_caps(self, gas_caps: GasCaps) -> Self {
        Self { gas_caps, ..self }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn rpc(&self) -> &Rpc {
        &self.rpc
    }

    /// Read a uint256 (e.g. an ERC20 allowance/balance) from `to(data)`.
    pub async fn read_uint(&self, to: Address, data: &Bytes) -> anyhow::Result<U256> {
        let out = self.rpc.eth_call(to, data).await?;
        if out.is_empty() {
            return Ok(U256::ZERO);
        }
        // A uint256 return is the last 32 bytes, big-endian.
        let start = out.len().saturating_sub(32);
        Ok(U256::from_be_slice(&out[start..]))
    }

    /// This wallet's on-chain code. Empty for a plain EOA; a 23-byte EIP-7702
    /// designator for a wallet that has been upgraded to a smart account.
    pub async fn code(&self) -> anyhow::Result<Bytes> {
        self.rpc.get_code(self.address).await
    }

    /// What a plain value transfer to `to` bids in gas, with one fee bump of
    /// headroom: `gas_limit * max_fee` off the same plan [`Self::send_and_wait`]
    /// would build, so "all but the fee" leaves enough for the send that
    /// follows. Sized against the bid, not the price paid: the node refuses
    /// a transaction whose `value + gas_limit * max_fee` exceeds the balance.
    pub async fn native_transfer_reserve(&self, to: Address) -> anyhow::Result<U256> {
        let plan = self.plan_tx(to, &Bytes::new(), U256::ZERO).await?;
        let gas = plan.gas_limit.max(U256::from(NATIVE_TRANSFER_GAS));
        let bid = plan.bumped(self.gas_caps).unwrap_or(plan).max_fee;
        Ok(bid.saturating_mul(gas))
    }

    /// Nonce + fees + gas for one send, read from the node. Refuses a plan
    /// over [`GasCaps`] rather than clamping it: a node answering that high is
    /// broken or hostile, and a clamped bid off its numbers is still a guess.
    async fn plan_tx(&self, to: Address, data: &Bytes, value: U256) -> anyhow::Result<TxPlan> {
        if let Some(path) = external_path(self.chain_id, self.address) {
            if let Some(intent) = read_external_intent(&path)? {
                anyhow::ensure!(self.rpc.latest_transaction_count(self.address).await? > intent.nonce,
                    "A journaled inventory transfer owns the next nonce; reconcile it before sending another transaction");
                std::fs::remove_file(path)?;
            }
        }
        let nonce = self.rpc.transaction_count(self.address).await?;
        let node_tip = self.rpc.max_priority_fee().await.ok();
        let node_price = self.rpc.gas_price().await.ok();
        let base_fee = self.rpc.base_fee().await.unwrap_or(U256::ZERO);
        let (suggested, tip) = node_prices(node_tip, node_price);
        let (priority, max_fee) = plan_fees(base_fee, suggested, tip);
        if node_tip
            .into_iter()
            .chain(node_price)
            .max()
            .is_some_and(|answered| priority > answered)
        {
            tracing::warn!(
                node_tip = ?node_tip, node_gas_price = ?node_price, bidding = %priority,
                "the RPC suggested a fee below the floor; bidding the floor instead"
            );
        }
        let est = self.rpc.estimate_gas(self.address, to, data, value).await?;
        let gas_limit = est.saturating_mul(U256::from(12u8)) / U256::from(10u8); // +20%
        if let Some(why) = self.gas_caps.violation(max_fee, gas_limit) {
            tracing::warn!(
                node_tip = ?node_tip, node_gas_price = ?node_price, base_fee = %base_fee,
                gas_estimate = %est, "refusing to sign: {why}"
            );
            anyhow::bail!(
                "not sending: {why}. The RPC answered tip {node_tip:?}, gas price \
                 {node_price:?}, base fee {base_fee}, gas estimate {est}. If fees on this \
                 chain really are that high, raise the ceiling under [gas] in stitch.toml; \
                 otherwise the RPC at rpc_url is misbehaving"
            );
        }
        Ok(TxPlan {
            nonce,
            priority,
            max_fee,
            gas_limit,
        })
    }

    /// Sign + broadcast one attempt. Same nonce twice is a replacement, not a
    /// second transaction — that is what makes the bump loop safe.
    async fn send_plan_tracked(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        plan: &TxPlan,
    ) -> Result<B256, SendPlanFailure> {
        let raw = self.sign_plan(to, data, value, plan).await?;
        self.broadcast(&raw, plan.nonce).await
    }

    /// One signed attempt, as raw bytes. Every signer signs a digest and
    /// hands it back, so nothing reaches the network until [`Self::broadcast`].
    async fn sign_plan(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        plan: &TxPlan,
    ) -> Result<Bytes, SendPlanFailure> {
        let tx = Eip1559Tx {
            chain_id: self.chain_id,
            nonce: plan.nonce,
            max_priority_fee_per_gas: plan.priority,
            max_fee_per_gas: plan.max_fee,
            gas_limit: plan.gas_limit,
            to,
            value,
            data,
        };
        sign_tx(self.signer.as_ref(), &tx)
            .await
            .map(|signed| signed.raw)
            .map_err(SendPlanFailure::SafeToRetry)
    }

    async fn broadcast(&self, raw: &Bytes, nonce: u64) -> Result<B256, SendPlanFailure> {
        match self.rpc.send_raw_tracked(raw).await {
            Ok(hash) => Ok(hash),
            Err(SendRawFailure::Rejected(err)) => Err(SendPlanFailure::SafeToRetry(
                self.explain_send_error(err).await,
            )),
            // A transport failure can arrive after the node accepted the raw
            // transaction. Keep the claim until chain state reconciles it.
            Err(SendRawFailure::Unknown(err)) => Err(SendPlanFailure::BroadcastUnknown(err, nonce)),
        }
    }

    async fn send_plan(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        plan: &TxPlan,
    ) -> anyhow::Result<B256> {
        self.send_plan_tracked(to, data, value, plan)
            .await
            .map_err(SendPlanFailure::into_error)
    }

    /// Turn a bare node rejection into something an operator can act on. Only
    /// the delegated-account limit gets special treatment — it names neither
    /// the wallet nor the fix, and every operator who hits it is stuck.
    async fn explain_send_error(&self, err: anyhow::Error) -> anyhow::Error {
        if !is_inflight_limit_error(&err.to_string()) {
            return err;
        }
        let delegate = self
            .rpc
            .get_code(self.address)
            .await
            .ok()
            .and_then(|code| delegation_target(&code));
        let stuck = self.rpc.latest_transaction_count(self.address).await.ok();
        anyhow::anyhow!("{err}\n\n{}", inflight_hint(delegate, stuck))
    }

    /// Sign for a durable external broadcaster. The callback must journal the
    /// exact bytes before acknowledging; this method never broadcasts or bumps.
    pub async fn journal_transaction<F, Fut>(
        &self,
        operation: &str,
        to: Address,
        data: Bytes,
        value: U256,
        journal: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(Bytes) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        anyhow::ensure!(
            self.signer.can_sign_transactions(),
            "This signer cannot authorize an inventory transfer"
        );
        let lock = nonce_lock(self.chain_id, self.address);
        let _guard = lock.lock().await;
        let plan = self.plan_tx(to, &data, value).await?;
        let raw = self
            .sign_plan(to, data, value, &plan)
            .await
            .map_err(SendPlanFailure::into_error)?;
        let path = external_path(self.chain_id, self.address)
            .ok_or_else(|| anyhow::anyhow!("Funding nonce journal not configured"))?;
        save_external_intent(
            &path,
            &ExternalIntent {
                operation: operation.to_owned(),
                nonce: plan.nonce,
                raw: hex::encode_prefixed(&raw),
            },
        )?;
        journal(raw).await
    }

    /// Sign + broadcast a contract call; returns the transaction hash.
    pub async fn send(&self, to: Address, data: Bytes, value: U256) -> anyhow::Result<B256> {
        let lock = nonce_lock(self.chain_id, self.address);
        let _nonce = lock.lock().await;
        let plan = self.plan_tx(to, &data, value).await?;
        self.send_plan(to, data, value, &plan).await
    }

    /// Send and poll for the receipt (up to ~`timeout`), re-sending the same
    /// nonce at a higher fee every [`BUMP_AFTER`] so a tx bid at the chain's
    /// floor can't sit in the pool until the timeout. Every attempt shares one
    /// nonce, so at most one of them can ever land.
    pub async fn send_and_wait(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.send_and_wait_reserving(to, data, value, timeout, |_, _| Ok(()))
            .await
    }

    /// [`Self::send_and_wait`], calling `reserve` with the nonce and the signed
    /// transaction once it is signed and before its first broadcast: the first
    /// moment it can reach the chain. A caller that must track it across
    /// restarts saves them there, so a restart can broadcast the same bytes if
    /// this run stops first; an error from `reserve` stops the send before
    /// anything is broadcast. A restart while the signer still waits for
    /// approval has broadcast nothing, so it leaves nothing to track.
    pub async fn send_and_wait_reserving(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        timeout: Duration,
        reserve: impl FnOnce(u64, &Bytes) -> anyhow::Result<()>,
    ) -> anyhow::Result<Value> {
        let lock = nonce_lock(self.chain_id, self.address);
        let nonce_guard = lock.lock().await;
        let plan = self.plan_tx(to, &data, value).await?;
        let raw = self
            .sign_plan(to, data.clone(), value, &plan)
            .await
            .map_err(SendPlanFailure::into_error)?;
        reserve(plan.nonce, &raw)?;
        let hash = self
            .broadcast(&raw, plan.nonce)
            .await
            .map_err(SendPlanFailure::into_error)?;
        drop(nonce_guard);
        // One line that answers "why is this not landing?" without a block
        // explorer: a stuck transaction is almost always a nonce or a fee.
        tracing::info!(
            %hash, to = %to, nonce = plan.nonce, tip_wei = %plan.priority,
            max_fee_wei = %plan.max_fee, gas_limit = %plan.gas_limit, "sent"
        );
        let mut plan = plan;
        let mut hashes = vec![hash];
        let mut at_ceiling = false;
        let deadline = std::time::Instant::now() + timeout;
        let mut next_bump = std::time::Instant::now() + BUMP_AFTER;
        loop {
            for hash in &hashes {
                if let Some(r) = self
                    .rpc
                    .receipt(*hash)
                    .await
                    .map_err(|e| may_still_land_error(e, plan.nonce))?
                {
                    let status = r.get("status").and_then(Value::as_str).unwrap_or("0x1");
                    if status == "0x0" {
                        anyhow::bail!("tx {hash} reverted");
                    }
                    return Ok(r);
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                let last = hashes.last().copied().unwrap_or_default();
                return Err(may_still_land_error(
                    anyhow::anyhow!(
                        "tx {last} (nonce {}) not mined within timeout, after re-sending it at a \
                     higher fee. It may still land — re-run to check before sending anything \
                     else, and if it is still pending, replace that nonce from your wallet",
                        plan.nonce
                    ),
                    plan.nonce,
                ));
            }
            if now >= next_bump {
                match plan.bumped(self.gas_caps) {
                    Some(next) => {
                        plan = next;
                        match self.send_plan(to, data.clone(), value, &plan).await {
                            Ok(hash) => {
                                tracing::info!(
                                    %hash, nonce = plan.nonce, max_fee = %plan.max_fee,
                                    "still pending; re-sent the same nonce at a higher fee"
                                );
                                hashes.push(hash);
                            }
                            // A rejected bump (fee bump too small, pool full) is not
                            // fatal: the original attempt is still live.
                            Err(e) => {
                                tracing::warn!(error = %e, "fee bump re-send rejected; still waiting")
                            }
                        }
                    }
                    None if !at_ceiling => {
                        at_ceiling = true;
                        tracing::warn!(
                            nonce = plan.nonce, max_fee = %plan.max_fee,
                            "still pending at the [gas] fee ceiling; not bidding higher, still waiting"
                        );
                    }
                    None => {}
                }
                next_bump = now + BUMP_AFTER;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
}

enum SendPlanFailure {
    SafeToRetry(anyhow::Error),
    /// The transport failed after the node may have accepted the send, at
    /// this nonce.
    BroadcastUnknown(anyhow::Error, u64),
}

impl SendPlanFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::SafeToRetry(err) => err,
            Self::BroadcastUnknown(err, nonce) => may_still_land_error(err, nonce),
        }
    }
}

/// The node-derived parameters of one send attempt. Bumping produces a new plan
/// on the same nonce, so a re-send replaces rather than queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TxPlan {
    nonce: u64,
    priority: U256,
    max_fee: U256,
    gas_limit: U256,
}

impl TxPlan {
    /// The same nonce at a higher fee, held under `caps`. `None` once the fee
    /// cap is already at the ceiling: there is nothing higher to re-send at.
    /// The tip is kept at or under the fee cap, as EIP-1559 requires.
    fn bumped(self, caps: GasCaps) -> Option<Self> {
        // +1 wei so a rounding-down division can never produce an equal (and
        // therefore rejected) replacement fee.
        let bump = |v: U256| {
            v.saturating_mul(U256::from(BUMP_NUMERATOR)) / U256::from(BUMP_DENOMINATOR)
                + U256::from(1u8)
        };
        let max_fee = bump(self.max_fee).min(caps.fee_ceiling(self.gas_limit));
        (max_fee > self.max_fee).then(|| Self {
            priority: bump(self.priority).min(max_fee),
            max_fee,
            ..self
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{parse_private_key, LocalSigner};
    use alloy_primitives::address;
    use std::sync::Arc;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn recall_funding_journal_is_exclusive_private_and_fails_closed_after_crash() {
        let dir = std::env::temp_dir().join(format!(
            "funding-journal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("pending.json");
        assert!(read_external_intent(&path).unwrap().is_none());
        let intent = ExternalIntent {
            operation: "one".into(),
            nonce: 42,
            raw: "0x02abcdef".into(),
        };
        save_external_intent(&path, &intent).unwrap();
        assert!(save_external_intent(&path, &intent).is_err());
        let recovered = read_external_intent(&path).unwrap().unwrap();
        assert_eq!(recovered.nonce, 42);
        assert_eq!(recovered.raw, intent.raw);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let owner = Address::repeat_byte(0x57);
        register_external_journal(42220, owner, path.clone());
        assert_eq!(external_path(42220, owner), Some(path.clone()));
        assert_eq!(external_path(1, owner), None);
        std::fs::write(&path, "{partial").unwrap();
        assert!(read_external_intent(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn local_signer() -> DynSigner {
        Arc::new(LocalSigner::new(parse_private_key(TEST_KEY).unwrap()))
    }

    #[test]
    fn builds_a_jsonrpc_envelope() {
        let req = Rpc::request("eth_chainId", json!([]));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["method"], "eth_chainId");
        assert!(req["params"].is_array());
    }

    #[test]
    fn parses_hex_quantities() {
        assert_eq!(parse_quantity(&json!("0x1a")).unwrap(), U256::from(26u8));
        assert_eq!(parse_quantity(&json!("0x0")).unwrap(), U256::ZERO);
        assert_eq!(parse_quantity(&json!("0x")).unwrap(), U256::ZERO);
    }

    #[tokio::test]
    async fn wallets_on_one_account_share_a_nonce_lock() {
        let a = address!("00000000000000000000000000000000000000a1");
        let b = address!("00000000000000000000000000000000000000b2");
        let first = nonce_lock(8453, a);
        assert!(Arc::ptr_eq(&first, &nonce_lock(8453, a)));
        assert!(!Arc::ptr_eq(&first, &nonce_lock(8453, b)));
        assert!(!Arc::ptr_eq(&first, &nonce_lock(1, a)));
        let held = first.lock().await;
        assert!(
            nonce_lock(8453, a).try_lock().is_err(),
            "a second wallet waits"
        );
        drop(held);
        assert!(nonce_lock(8453, a).try_lock().is_ok());
    }

    #[test]
    fn only_post_broadcast_errors_are_marked_as_may_still_land() {
        let before = SendPlanFailure::SafeToRetry(anyhow::anyhow!("signing failed"));
        assert!(!transaction_may_still_land(&before.into_error()));

        let uncertain =
            SendPlanFailure::BroadcastUnknown(anyhow::anyhow!("send response was lost"), 4);
        let uncertain = uncertain.into_error();
        assert!(transaction_may_still_land(&uncertain));
        assert_eq!(unresolved_nonce(&uncertain), Some(4));

        let receipt_timeout = may_still_land_error(anyhow::anyhow!("receipt timed out"), 9);
        assert!(transaction_may_still_land(&receipt_timeout));
        let wrapped = receipt_timeout.context("liquidity schedule");
        assert_eq!(unresolved_nonce(&wrapped), Some(9));
    }

    #[tokio::test]
    async fn send_raw_distinguishes_node_rejection_from_transport_uncertainty() {
        let rejected = fixed_node(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"insufficient funds"}}"#,
        )
        .await;
        let rejection = Rpc::new(rejected)
            .send_raw_tracked(&Bytes::new())
            .await
            .unwrap_err();
        assert!(matches!(rejection, SendRawFailure::Rejected(_)));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let unknown = Rpc::new(unavailable)
            .send_raw_tracked(&Bytes::new())
            .await
            .unwrap_err();
        assert!(matches!(unknown, SendRawFailure::Unknown(_)));
    }

    /// A JSON-RPC node that answers every call with one fixed body.
    async fn fixed_node(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// A node that answers `eth_chainId` with `chain_id` and every other
    /// method with `status` and `body`, counting only the other methods.
    async fn chain_node(
        chain_id: u64,
        status: u16,
        body: &'static str,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let counter = counter.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let (status, body) = if req.contains("eth_chainId") {
                        (
                            200,
                            format!(r#"{{"jsonrpc":"2.0","id":1,"result":"0x{chain_id:x}"}}"#),
                        )
                    } else {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (status, body.to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    /// An address nothing listens on: every call fails to connect.
    async fn dead_node() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        url
    }

    fn hits(counter: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    const BALANCE_ONE: &str = r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#;
    const OWNER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[test]
    fn endpoints_keep_order_and_drop_blank_and_repeated_fallbacks() {
        let e = RpcEndpoints::with_fallbacks(
            "https://a.example",
            [
                " https://b.example ",
                "",
                "https://a.example",
                "https://b.example",
                "https://c.example",
            ]
            .map(String::from),
            1,
        );
        assert_eq!(
            e.urls(),
            [
                "https://a.example",
                "https://b.example",
                "https://c.example"
            ]
        );
        assert_eq!(e.primary(), "https://a.example");
        assert!(!RpcEndpoints::from("https://a.example").has_fallbacks());
    }

    #[test]
    fn calls_stay_on_a_fallback_until_the_primary_is_due_a_retry() {
        let t0 = std::time::Instant::now();
        let on_primary = Route {
            active: 0,
            since: t0,
        };
        assert_eq!(attempt_order(3, on_primary, t0), [0, 1, 2]);

        let on_second = Route {
            active: 2,
            since: t0,
        };
        assert_eq!(attempt_order(3, on_second, t0), [2, 0, 1]);
        let later = t0 + PRIMARY_RETRY_AFTER;
        assert_eq!(attempt_order(3, on_second, later), [0, 1, 2]);

        // The first call once the primary is due takes the retry; calls
        // planned after it stay on the fallback for another window.
        let claimed = claim_retry(on_second, later);
        assert_eq!(
            claimed,
            Route {
                active: 2,
                since: later
            }
        );
        assert_eq!(attempt_order(3, claimed, later), [2, 0, 1]);
        assert_eq!(claim_retry(on_second, t0), on_second);
        // Served where it planned to go: unchanged.
        assert_eq!(route_after(claimed, 2, later), claimed);
        // The primary answered: back on it.
        assert_eq!(route_after(claimed, 0, later).active, 0);
        // The primary stopped answering: onto the first fallback that did.
        assert_eq!(route_after(on_primary, 1, t0).active, 1);
    }

    #[tokio::test]
    async fn a_dead_primary_fails_over_and_later_calls_skip_it() {
        let primary = dead_node().await;
        let (fallback, served) = chain_node(8453, 200, BALANCE_ONE).await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(primary, [fallback], 8453));
        assert_eq!(rpc.get_balance(OWNER).await.unwrap(), U256::from(1u8));
        assert_eq!(rpc.failover.as_ref().unwrap().route().active, 1);
        assert_eq!(rpc.get_balance(OWNER).await.unwrap(), U256::from(1u8));
        assert_eq!(hits(&served), 2);
    }

    #[tokio::test]
    async fn a_failing_status_fails_over_and_wallets_share_the_route() {
        let (primary, primary_hits) = chain_node(8453, 503, "busy").await;
        let (fallback, _) = chain_node(8453, 200, BALANCE_ONE).await;
        let endpoints = RpcEndpoints::with_fallbacks(primary, [fallback], 8453);
        Rpc::new(&endpoints).get_balance(OWNER).await.unwrap();
        // A client built afresh, as every send builds its wallet, starts on
        // the fallback instead of paying for the dead primary again.
        Rpc::new(&endpoints).get_balance(OWNER).await.unwrap();
        assert_eq!(hits(&primary_hits), 1);
    }

    #[tokio::test]
    async fn a_fallback_on_another_chain_is_never_used() {
        let primary = dead_node().await;
        let (wrong, wrong_hits) = chain_node(1, 200, BALANCE_ONE).await;
        let (right, _) = chain_node(8453, 200, BALANCE_ONE).await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(primary, [wrong, right], 8453));
        assert_eq!(rpc.get_balance(OWNER).await.unwrap(), U256::from(1u8));
        assert_eq!(hits(&wrong_hits), 0);
        assert_eq!(rpc.failover.as_ref().unwrap().route().active, 2);
    }

    #[tokio::test]
    async fn a_jsonrpc_error_is_an_answer_not_a_reason_to_fail_over() {
        let (primary, _) = chain_node(
            8453,
            200,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}}"#,
        )
        .await;
        let (fallback, fallback_hits) = chain_node(8453, 200, BALANCE_ONE).await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(primary, [fallback], 8453));
        let err = rpc.get_balance(OWNER).await.unwrap_err();
        assert!(err.to_string().contains("execution reverted"), "{err}");
        assert_eq!(hits(&fallback_hits), 0);
    }

    #[tokio::test]
    async fn every_endpoint_down_names_each_without_its_path() {
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(
            format!("{}/v2/secret-key", dead_node().await),
            [dead_node().await],
            8453,
        ));
        let err = rpc.get_balance(OWNER).await.unwrap_err().to_string();
        assert!(err.contains("primary 127.0.0.1"), "{err}");
        assert!(err.contains("fallback 1 127.0.0.1"), "{err}");
        assert!(!err.contains("secret-key"), "{err}");
    }

    #[tokio::test]
    async fn the_pending_nonce_is_the_highest_any_endpoint_reports() {
        // The fallback carried the last broadcast while the primary was
        // away; the primary's pool never saw it.
        let (primary, _) =
            chain_node(8453, 200, r#"{"jsonrpc":"2.0","id":1,"result":"0x5"}"#).await;
        let (fallback, _) =
            chain_node(8453, 200, r#"{"jsonrpc":"2.0","id":1,"result":"0x6"}"#).await;
        let (wrong_chain, _) =
            chain_node(1, 200, r#"{"jsonrpc":"2.0","id":1,"result":"0x99"}"#).await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(
            primary,
            [wrong_chain, fallback, dead_node().await],
            8453,
        ));
        assert_eq!(rpc.transaction_count(OWNER).await.unwrap(), 6);
    }

    #[tokio::test]
    async fn a_primary_rejection_is_final_and_never_rebroadcast() {
        let (primary, _) = chain_node(
            8453,
            200,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nonce too low"}}"#,
        )
        .await;
        let (fallback, fallback_hits) = chain_node(8453, 200, BALANCE_ONE).await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(primary, [fallback], 8453));
        let failure = rpc.send_raw_tracked(&Bytes::new()).await.unwrap_err();
        assert!(matches!(failure, SendRawFailure::Rejected(_)));
        assert_eq!(hits(&fallback_hits), 0);
    }

    #[tokio::test]
    async fn a_rejection_after_a_lost_broadcast_stays_unknown() {
        // The primary may have taken the bytes before the connection died, so
        // the fallback's "already known" can't prove the nonce is free.
        let primary = dead_node().await;
        let (fallback, _) = chain_node(
            8453,
            200,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"already known"}}"#,
        )
        .await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(primary, [fallback], 8453));
        let failure = rpc.send_raw_tracked(&Bytes::new()).await.unwrap_err();
        assert!(matches!(failure, SendRawFailure::Unknown(_)));
    }

    #[tokio::test]
    async fn a_refusal_after_a_lost_broadcast_still_tries_the_next_fallback() {
        let primary = dead_node().await;
        let (limited, _) = chain_node(
            8453,
            200,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"rate limited"}}"#,
        )
        .await;
        let (healthy, healthy_hits) = chain_node(
            8453,
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":"0x1111111111111111111111111111111111111111111111111111111111111111"}"#,
        )
        .await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(
            primary,
            [limited, healthy],
            8453,
        ));
        assert!(rpc.send_raw_tracked(&Bytes::new()).await.is_ok());
        assert_eq!(hits(&healthy_hits), 1);
    }

    #[tokio::test]
    async fn a_lost_broadcast_lands_through_the_fallback() {
        let hash = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let primary = dead_node().await;
        let (fallback, _) = chain_node(
            8453,
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":"0x1111111111111111111111111111111111111111111111111111111111111111"}"#,
        )
        .await;
        let rpc = Rpc::new(RpcEndpoints::with_fallbacks(primary, [fallback], 8453));
        let landed = rpc.send_raw_tracked(&Bytes::new()).await.unwrap();
        assert_eq!(landed, hash.parse::<B256>().unwrap());
    }

    #[tokio::test]
    async fn get_balance_parses_a_hex_quantity() {
        let url = fixed_node(r#"{"jsonrpc":"2.0","id":1,"result":"0xde0b6b3a7640000"}"#).await;
        let bal = Rpc::new(url)
            .get_balance(address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"))
            .await
            .unwrap();
        assert_eq!(bal, U256::from(1_000_000_000_000_000_000u128));
    }

    #[tokio::test]
    async fn get_balance_reads_a_bare_0x_as_zero() {
        let url = fixed_node(r#"{"jsonrpc":"2.0","id":1,"result":"0x"}"#).await;
        let bal = Rpc::new(url)
            .get_balance(address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"))
            .await
            .unwrap();
        assert_eq!(bal, U256::ZERO);
    }

    #[tokio::test]
    async fn get_balance_surfaces_an_rpc_error() {
        let url = fixed_node(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"header not found"}}"#,
        )
        .await;
        let err = Rpc::new(url)
            .get_balance(address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("rpc eth_getBalance error"),
            "{err:#}"
        );
    }

    #[test]
    fn reads_an_eip7702_delegation_target() {
        let mut code = vec![0xef, 0x01, 0x00];
        code.extend_from_slice(address!("63c0c19a282a1b52B07dD5a65b58948A07DAE32B").as_slice());
        assert_eq!(
            delegation_target(&code),
            Some(address!("63c0c19a282a1b52B07dD5a65b58948A07DAE32B"))
        );
    }

    #[test]
    fn plain_eoas_and_contracts_are_not_delegations() {
        assert_eq!(delegation_target(&[]), None);
        // Real contract bytecode: right prefix byte, wrong shape.
        assert_eq!(delegation_target(&[0xef, 0x01, 0x00, 0x11]), None);
        assert_eq!(delegation_target(&[0x60, 0x80, 0x60, 0x40]), None);
    }

    #[test]
    fn recognises_geths_inflight_limit_rejection() {
        assert!(is_inflight_limit_error(
            "rpc eth_sendRawTransaction error: {\"code\":-32000,\"message\":\"in-flight transaction limit reached for delegated accounts\"}"
        ));
        assert!(!is_inflight_limit_error(
            "replacement transaction underpriced"
        ));
    }

    #[test]
    fn the_inflight_hint_names_the_delegate_and_the_stuck_nonce() {
        let hint = inflight_hint(
            Some(address!("63c0c19a282a1b52B07dD5a65b58948A07DAE32B")),
            Some(6),
        );
        assert!(hint.contains("0x63c0"), "{hint}");
        assert!(hint.contains("nonce 6"), "{hint}");
        assert!(hint.contains("revoke the delegation"), "{hint}");
    }

    #[test]
    fn a_zero_base_fee_chain_bids_the_whole_price_as_tip() {
        // BNB Chain: base fee 0, both suggestions 0.05 gwei.
        let (tip, max_fee) = plan_fees(
            U256::ZERO,
            U256::from(50_000_000u64),
            U256::from(50_000_000u64),
        );
        assert_eq!(tip, U256::from(50_000_000u64));
        assert_eq!(max_fee, tip, "with no base fee the tip is the whole bid");
    }

    #[test]
    fn a_provider_answering_zero_cannot_produce_a_free_transaction() {
        let (tip, max_fee) = plan_fees(U256::ZERO, U256::ZERO, U256::ZERO);
        assert_eq!(tip, U256::from(MIN_PRIORITY_WEI));
        assert!(max_fee >= tip);
    }

    #[test]
    fn a_real_base_fee_chain_backs_the_tip_out_of_the_gas_price() {
        // 20 gwei base, 20.5 gwei suggested → a 0.5 gwei tip, not 20.5.
        let base = U256::from(20_000_000_000u64);
        let (tip, max_fee) = plan_fees(base, U256::from(20_500_000_000u64), U256::from(1u8));
        assert_eq!(tip, U256::from(500_000_000u64));
        assert_eq!(max_fee, base * U256::from(2u8) + tip);
    }

    #[test]
    fn the_node_tip_wins_when_it_is_the_higher_suggestion() {
        let (tip, _) = plan_fees(
            U256::ZERO,
            U256::from(50_000_000u64),
            U256::from(300_000_000u64),
        );
        assert_eq!(tip, U256::from(300_000_000u64));
    }

    #[test]
    fn a_missing_tip_leaves_a_live_gas_price_to_set_the_bid() {
        // BNB Chain prices gas at 0.1 gwei and the node refuses
        // `eth_maxPriorityFeePerGas`. The old default stood in at 1 gwei and
        // won the `max`, so an approval reserved ten times what it costs.
        let price = U256::from(100_000_000u64);
        let (suggested, tip) = node_prices(None, Some(price));
        assert_eq!((suggested, tip), (price, U256::ZERO));
        let (priority, max_fee) = plan_fees(U256::ZERO, suggested, tip);
        assert_eq!(priority, price);
        assert_eq!(max_fee, price);
    }

    #[test]
    fn a_missing_gas_price_leaves_the_node_tip_to_set_the_bid() {
        let node_tip = U256::from(300_000_000u64);
        let (suggested, tip) = node_prices(Some(node_tip), None);
        assert_eq!((suggested, tip), (U256::ZERO, node_tip));
        assert_eq!(plan_fees(U256::ZERO, suggested, tip).0, node_tip);
    }

    #[test]
    fn neither_price_answering_falls_back_to_the_flat_default() {
        let default = U256::from(DEFAULT_PRIORITY_WEI);
        assert_eq!(node_prices(None, None), (default, default));
        assert_eq!(plan_fees(U256::ZERO, default, default).0, default);
    }

    #[test]
    fn both_prices_answering_are_passed_through_untouched() {
        let tip = U256::from(50_000_000u64);
        let price = U256::from(120_000_000u64);
        assert_eq!(node_prices(Some(tip), Some(price)), (price, tip));
    }

    #[test]
    fn a_bumped_plan_keeps_the_nonce_and_raises_both_fees() {
        let plan = TxPlan {
            nonce: 6,
            priority: U256::from(50_000_000u64),
            max_fee: U256::from(150_000_000u64),
            gas_limit: U256::from(56_194u64),
        };
        let bumped = plan.bumped(GasCaps::for_chain(1)).unwrap();
        assert_eq!(bumped.nonce, plan.nonce, "a bump replaces, never queues");
        assert_eq!(bumped.gas_limit, plan.gas_limit);
        // geth needs +10% on both fee fields to accept the replacement.
        assert!(bumped.priority * U256::from(100u8) >= plan.priority * U256::from(110u8));
        assert!(bumped.max_fee * U256::from(100u8) >= plan.max_fee * U256::from(110u8));
    }

    #[test]
    fn bumping_a_zero_fee_still_increases_it() {
        let plan = TxPlan {
            nonce: 0,
            priority: U256::ZERO,
            max_fee: U256::ZERO,
            gas_limit: U256::from(21_000u64),
        };
        assert!(plan.bumped(GasCaps::for_chain(1)).unwrap().priority > U256::ZERO);
    }

    #[test]
    fn derives_wallet_address_from_key() {
        let w = Wallet::new("http://localhost:8545", local_signer(), 31337);
        assert_eq!(
            w.address(),
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
    }

    #[tokio::test]
    async fn the_native_reserve_covers_what_a_send_would_bid() {
        // Every quantity the node is asked for answers 0x5208 (21000): nonce,
        // fees and the gas estimate alike. `baseFeePerGas` is missing from the
        // block answer, so the base fee reads as zero and the bid is the tip.
        let url = fixed_node(r#"{"jsonrpc":"2.0","id":1,"result":"0x5208"}"#).await;
        let wallet = Wallet::new(url, local_signer(), 1);
        let to = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
        let plan = wallet.plan_tx(to, &Bytes::new(), U256::ZERO).await.unwrap();
        let reserve = wallet.native_transfer_reserve(to).await.unwrap();
        let first_bid = plan.gas_limit * plan.max_fee;
        assert!(
            reserve > first_bid,
            "one fee bump of headroom: {reserve} vs {first_bid}"
        );
        assert_eq!(
            reserve,
            plan.gas_limit * plan.bumped(GasCaps::for_chain(1)).unwrap().max_fee
        );
    }

    /// `reserve` runs after signing and before the first broadcast: a signer
    /// that never answers (a custody approval a restart cut short) reserves
    /// nothing, and a refused reserve broadcasts nothing.
    #[tokio::test]
    async fn reserve_runs_between_signing_and_the_first_broadcast() {
        use axum::{routing::post, Json, Router};
        struct Refusing;
        #[async_trait::async_trait]
        impl crate::signer::Signer for Refusing {
            async fn sign_digest(&self, _: B256) -> anyhow::Result<[u8; 65]> {
                anyhow::bail!("approval never came")
            }
            fn address(&self) -> Address {
                address!("70997970C51812dc3A010C7d01b50e0d17dc79C8")
            }
        }
        let methods = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = methods.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(req): Json<Value>| {
                let seen = seen.clone();
                async move {
                    let method = req["method"].as_str().unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(method.clone());
                    let result = match method.as_str() {
                        "eth_getBlockByNumber" => json!({ "baseFeePerGas": "0x1" }),
                        "eth_estimateGas" => json!("0x5208"),
                        _ => json!("0x1"),
                    };
                    Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        let send = |wallet: Wallet, refuse: bool| async move {
            let reserved = std::sync::atomic::AtomicBool::new(false);
            let sent = wallet
                .send_and_wait_reserving(
                    to(),
                    Bytes::new(),
                    U256::ZERO,
                    Duration::ZERO,
                    |_, signed| {
                        // Handed the signed transaction itself, to save.
                        assert_eq!(signed.first(), Some(&0x02));
                        reserved.store(true, std::sync::atomic::Ordering::SeqCst);
                        if refuse {
                            anyhow::bail!("could not save")
                        }
                        Ok(())
                    },
                )
                .await;
            (sent.is_err(), reserved.into_inner())
        };

        assert_eq!(
            send(Wallet::new(url.clone(), Arc::new(Refusing), 1), false).await,
            (true, false),
            "nothing signed, nothing reserved"
        );
        assert_eq!(
            send(Wallet::new(url, local_signer(), 1), true).await,
            (true, true)
        );
        assert!(
            !methods
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "eth_sendRawTransaction"),
            "nothing broadcast"
        );
    }

    const GWEI: u128 = 1_000_000_000;

    /// A node that answers the fee and gas reads with the given numbers:
    /// `eth_maxPriorityFeePerGas` = tip, `baseFeePerGas` = base fee,
    /// `eth_gasPrice` = both, `eth_estimateGas` = estimate. Nonce 0.
    async fn fee_node(tip: u128, base_fee: u128, estimate: u64) -> String {
        use axum::{routing::post, Json, Router};
        let hex = |v: u128| format!("0x{v:x}");
        let app = Router::new().route(
            "/",
            post(move |Json(req): Json<Value>| async move {
                let result = match req["method"].as_str().unwrap_or_default() {
                    "eth_maxPriorityFeePerGas" => json!(hex(tip)),
                    "eth_gasPrice" => json!(hex(base_fee + tip)),
                    "eth_getBlockByNumber" => json!({ "baseFeePerGas": hex(base_fee) }),
                    "eth_estimateGas" => json!(hex(estimate.into())),
                    _ => json!("0x0"),
                };
                Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        format!("http://{addr}")
    }

    fn to() -> Address {
        address!("70997970C51812dc3A010C7d01b50e0d17dc79C8")
    }

    #[tokio::test]
    async fn a_node_tip_that_would_spend_most_of_the_balance_is_refused() {
        // The audit's replay (F-21): a ~16,665 gwei tip on a 50k-gas estimate
        // plans a 60k gas limit whose worst case is ~1 ETH, most of a 1 ETH
        // gas float, paid to the block producer.
        let url = fee_node(16_664_666_666_666, 1_000_000_000, 50_000).await;
        let wallet = Wallet::new(url, local_signer(), 1);
        let err = wallet
            .plan_tx(to(), &Bytes::new(), U256::ZERO)
            .await
            .expect_err("a hostile tip must not be signed");
        let msg = format!("{err:#}");
        assert!(msg.contains("max_fee_per_gas_gwei"), "{msg}");
        assert!(
            msg.contains("16664666666666"),
            "names the node's tip: {msg}"
        );
    }

    #[tokio::test]
    async fn a_legal_price_on_a_huge_gas_estimate_is_refused() {
        // 40 gwei is under Base's 50 gwei ceiling, but 40 gwei x 1.2M gas is
        // 0.048 ETH, over its 0.01 ETH per-transaction ceiling.
        let url = fee_node(40 * GWEI, 0, 1_000_000).await;
        let wallet = Wallet::new(url, local_signer(), 8453);
        let err = wallet
            .plan_tx(to(), &Bytes::new(), U256::ZERO)
            .await
            .expect_err("over the per-transaction ceiling");
        assert!(format!("{err:#}").contains("max_tx_fee"), "{err:#}");
    }

    #[tokio::test]
    async fn normal_fees_plan_unchanged() {
        // Base on a normal day: 0.005 gwei base fee, 0.001 gwei tip (under
        // the floor), 150k gas.
        let url = fee_node(GWEI / 1_000, 5 * GWEI / 1_000, 150_000).await;
        let wallet = Wallet::new(url, local_signer(), 8453);
        let plan = wallet
            .plan_tx(to(), &Bytes::new(), U256::ZERO)
            .await
            .unwrap();
        let (priority, max_fee) = plan_fees(
            U256::from(5 * GWEI / 1_000),
            U256::from(6 * GWEI / 1_000),
            U256::from(GWEI / 1_000),
        );
        assert_eq!((plan.priority, plan.max_fee), (priority, max_fee));
        assert_eq!(plan.gas_limit, U256::from(180_000u64));
    }

    #[tokio::test]
    async fn an_operator_ceiling_overrides_the_built_in_one() {
        let url = fee_node(80 * GWEI, 0, 21_000).await;
        let wallet = Wallet::new(&url, local_signer(), 8453);
        assert!(wallet
            .plan_tx(to(), &Bytes::new(), U256::ZERO)
            .await
            .is_err());
        let raised = GasCaps::for_chain(8453).with_overrides(Some(100.0), None);
        let wallet = Wallet::new(&url, local_signer(), 8453).with_gas_caps(raised);
        let plan = wallet
            .plan_tx(to(), &Bytes::new(), U256::ZERO)
            .await
            .unwrap();
        assert_eq!(plan.priority, U256::from(80 * GWEI));
    }

    #[test]
    fn bumps_stop_at_the_ceiling() {
        let caps = GasCaps::for_chain(8453); // 50 gwei
        let plan = TxPlan {
            nonce: 3,
            priority: U256::from(45 * GWEI),
            max_fee: U256::from(45 * GWEI),
            gas_limit: U256::from(21_000u64),
        };
        let bumped = plan.bumped(caps).expect("room for one more bump");
        assert_eq!(bumped.max_fee, U256::from(50 * GWEI));
        assert!(
            bumped.priority <= bumped.max_fee,
            "tip never over the fee cap"
        );
        assert_eq!(bumped.bumped(caps), None, "nothing higher to re-send at");
    }

    #[test]
    fn bumps_also_stop_at_the_per_transaction_ceiling() {
        // Base's 0.01 ETH over a 1M gas limit leaves 10 gwei per gas.
        let caps = GasCaps::for_chain(8453);
        let plan = TxPlan {
            nonce: 3,
            priority: U256::from(GWEI),
            max_fee: U256::from(9 * GWEI),
            gas_limit: U256::from(1_000_000u64),
        };
        let bumped = plan.bumped(caps).unwrap();
        assert_eq!(bumped.max_fee, U256::from(10 * GWEI));
        assert_eq!(bumped.bumped(caps), None);
    }

    /// End-to-end proof the EIP-1559 encode → sign → broadcast → receipt path is
    /// correct: a real node accepts the signed tx and mines it. Hardhat/Anvil
    /// reject malformed RLP, bad chain_id, or an invalid signature, so a 0x1
    /// receipt is a strong check. Run with: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a local chain at FILLER_TEST_RPC (default http://localhost:8545)"]
    async fn lands_a_value_transfer_on_a_local_chain() {
        let rpc_url =
            std::env::var("FILLER_TEST_RPC").unwrap_or_else(|_| "http://localhost:8545".into());
        let chain_id = Rpc::new(&rpc_url).chain_id().await.expect("chain_id");
        let wallet = Wallet::new(&rpc_url, local_signer(), chain_id);
        let to = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"); // hardhat #1
        let receipt = wallet
            .send_and_wait(to, Bytes::new(), U256::from(1u64), Duration::from_secs(30))
            .await
            .expect("value transfer lands");
        assert_eq!(receipt.get("status").and_then(Value::as_str), Some("0x1"));
    }
}
