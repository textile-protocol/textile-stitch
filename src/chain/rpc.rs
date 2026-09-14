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
    let max_fee = base_fee.saturating_mul(U256::from(2u8)) + priority;
    (priority, max_fee)
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

/// Low-level JSON-RPC transport.
#[derive(Clone)]
pub struct Rpc {
    url: String,
    client: reqwest::Client,
}

impl Rpc {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: http_client(),
        }
    }

    /// Build a JSON-RPC request envelope (pure — easy to assert on).
    pub fn request(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    }

    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let resp: Value = self
            .client
            .post(&self.url)
            .json(&Self::request(method, params))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
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
    pub async fn transaction_count(&self, addr: Address) -> anyhow::Result<u64> {
        let r = self
            .call(
                "eth_getTransactionCount",
                json!([addr.to_string(), "pending"]),
            )
            .await?;
        Ok(parse_quantity(&r)?.to::<u64>())
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
        let r = self
            .call("eth_sendRawTransaction", json!([hex::encode_prefixed(raw)]))
            .await?;
        let s = r.as_str().unwrap_or_default();
        Ok(s.parse()?)
    }

    pub async fn receipt(&self, hash: B256) -> anyhow::Result<Option<Value>> {
        let r = self
            .call("eth_getTransactionReceipt", json!([hash.to_string()]))
            .await?;
        Ok(if r.is_null() { None } else { Some(r) })
    }
}

/// A signing wallet over an [`Rpc`]: turns calldata into landed transactions.
/// The signer is shared (`Arc`) with the green-leg poster.
pub struct Wallet {
    rpc: Rpc,
    signer: DynSigner,
    address: Address,
    chain_id: u64,
}

impl Wallet {
    pub fn new(rpc_url: impl Into<String>, signer: DynSigner, chain_id: u64) -> Self {
        let address = signer.address();
        Self {
            rpc: Rpc::new(rpc_url),
            signer,
            address,
            chain_id,
        }
    }

    pub fn address(&self) -> Address {
        self.address
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
        Ok(plan.bumped().max_fee.saturating_mul(gas))
    }

    /// Nonce + fees + gas for one send, read from the node.
    async fn plan_tx(&self, to: Address, data: &Bytes, value: U256) -> anyhow::Result<TxPlan> {
        let nonce = self.rpc.transaction_count(self.address).await?;
        let node_tip = self
            .rpc
            .max_priority_fee()
            .await
            .unwrap_or_else(|_| U256::from(DEFAULT_PRIORITY_WEI));
        let suggested = self.rpc.gas_price().await.unwrap_or(node_tip);
        let base_fee = self.rpc.base_fee().await.unwrap_or(U256::ZERO);
        let (priority, max_fee) = plan_fees(base_fee, suggested, node_tip);
        if priority > node_tip.max(suggested) {
            tracing::warn!(
                node_tip = %node_tip, node_gas_price = %suggested, bidding = %priority,
                "the RPC suggested a fee below the floor; bidding the floor instead"
            );
        }
        let est = self.rpc.estimate_gas(self.address, to, data, value).await?;
        let gas_limit = est.saturating_mul(U256::from(12u8)) / U256::from(10u8); // +20%
        Ok(TxPlan {
            nonce,
            priority,
            max_fee,
            gas_limit,
        })
    }

    /// Sign + broadcast one attempt. Same nonce twice is a replacement, not a
    /// second transaction — that is what makes the bump loop safe.
    async fn send_plan(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        plan: &TxPlan,
    ) -> anyhow::Result<B256> {
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
        let signed = sign_tx(self.signer.as_ref(), &tx).await?;
        match self.rpc.send_raw(&signed.raw).await {
            Ok(hash) => Ok(hash),
            Err(e) => Err(self.explain_send_error(e).await),
        }
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

    /// Sign + broadcast a contract call; returns the transaction hash.
    pub async fn send(&self, to: Address, data: Bytes, value: U256) -> anyhow::Result<B256> {
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
        let plan = self.plan_tx(to, &data, value).await?;
        let hash = self.send_plan(to, data.clone(), value, &plan).await?;
        // One line that answers "why is this not landing?" without a block
        // explorer: a stuck transaction is almost always a nonce or a fee.
        tracing::info!(
            %hash, to = %to, nonce = plan.nonce, tip_wei = %plan.priority,
            max_fee_wei = %plan.max_fee, gas_limit = %plan.gas_limit, "sent"
        );
        let mut plan = plan;
        let mut hashes = vec![hash];
        let deadline = std::time::Instant::now() + timeout;
        let mut next_bump = std::time::Instant::now() + BUMP_AFTER;
        loop {
            for hash in &hashes {
                if let Some(r) = self.rpc.receipt(*hash).await? {
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
                anyhow::bail!(
                    "tx {last} (nonce {}) not mined within timeout, after re-sending it at a \
                     higher fee. It may still land — re-run to check before sending anything \
                     else, and if it is still pending, replace that nonce from your wallet",
                    plan.nonce
                );
            }
            if now >= next_bump {
                plan = plan.bumped();
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
                next_bump = now + BUMP_AFTER;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
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
    fn bumped(self) -> Self {
        // +1 wei so a rounding-down division can never produce an equal (and
        // therefore rejected) replacement fee.
        let bump = |v: U256| {
            v.saturating_mul(U256::from(BUMP_NUMERATOR)) / U256::from(BUMP_DENOMINATOR)
                + U256::from(1u8)
        };
        Self {
            priority: bump(self.priority),
            max_fee: bump(self.max_fee),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{parse_private_key, LocalSigner};
    use alloy_primitives::address;
    use std::sync::Arc;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

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
    fn a_bumped_plan_keeps_the_nonce_and_raises_both_fees() {
        let plan = TxPlan {
            nonce: 6,
            priority: U256::from(50_000_000u64),
            max_fee: U256::from(150_000_000u64),
            gas_limit: U256::from(56_194u64),
        };
        let bumped = plan.bumped();
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
        assert!(plan.bumped().priority > U256::ZERO);
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
        assert_eq!(reserve, plan.gas_limit * plan.bumped().max_fee);
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
