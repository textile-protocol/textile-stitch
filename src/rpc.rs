// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Minimal Ethereum JSON-RPC client and a signing [`Wallet`] that lands
//! transactions: fill nonce, gas, and EIP-1559 fees from the node, sign with
//! [`crate::tx`], broadcast via `eth_sendRawTransaction`, and (optionally) wait
//! for the receipt. Reads go through `eth_call`. Just enough RPC for the
//! blue-leg closer — no provider framework, same reqwest client the indexer and
//! subgraph already use.

use std::time::Duration;

use alloy_primitives::{hex, Address, Bytes, B256, U256};
use k256::ecdsa::SigningKey;
use serde_json::{json, Value};

use crate::net::http_client;
use crate::signer::address_from_signing_key;
use crate::tx::{sign_tx, Eip1559Tx};

/// Default priority fee when the node has no `eth_maxPriorityFeePerGas`: 1 gwei.
const DEFAULT_PRIORITY_WEI: u64 = 1_000_000_000;

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
pub struct Wallet {
    rpc: Rpc,
    key: SigningKey,
    address: Address,
    chain_id: u64,
}

impl Wallet {
    pub fn new(rpc_url: impl Into<String>, key: SigningKey, chain_id: u64) -> Self {
        let address = address_from_signing_key(&key);
        Self {
            rpc: Rpc::new(rpc_url),
            key,
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

    /// Sign + broadcast a contract call; returns the transaction hash.
    pub async fn send(&self, to: Address, data: Bytes, value: U256) -> anyhow::Result<B256> {
        let nonce = self.rpc.transaction_count(self.address).await?;
        let priority = self
            .rpc
            .max_priority_fee()
            .await
            .unwrap_or_else(|_| U256::from(DEFAULT_PRIORITY_WEI));
        let base = self.rpc.gas_price().await.unwrap_or(priority);
        // Headroom for base-fee swings between estimate and inclusion.
        let max_fee = base.saturating_mul(U256::from(2u8)) + priority;
        let est = self
            .rpc
            .estimate_gas(self.address, to, &data, value)
            .await?;
        let gas_limit = est.saturating_mul(U256::from(12u8)) / U256::from(10u8); // +20%

        let tx = Eip1559Tx {
            chain_id: self.chain_id,
            nonce,
            max_priority_fee_per_gas: priority,
            max_fee_per_gas: max_fee,
            gas_limit,
            to,
            value,
            data,
        };
        let signed = sign_tx(&self.key, &tx)?;
        self.rpc.send_raw(&signed.raw).await
    }

    /// Send and poll for the receipt (up to ~`timeout`).
    pub async fn send_and_wait(
        &self,
        to: Address,
        data: Bytes,
        value: U256,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let hash = self.send(to, data, value).await?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(r) = self.rpc.receipt(hash).await? {
                let status = r.get("status").and_then(Value::as_str).unwrap_or("0x1");
                if status == "0x0" {
                    anyhow::bail!("tx {hash} reverted");
                }
                return Ok(r);
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("tx {hash} not mined within timeout");
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

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

    #[test]
    fn derives_wallet_address_from_key() {
        let key = SigningKey::from_slice(
            &hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap(),
        )
        .unwrap();
        let w = Wallet::new("http://localhost:8545", key, 31337);
        assert_eq!(
            w.address(),
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
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
        let key = SigningKey::from_slice(
            &hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap(),
        )
        .unwrap();
        let chain_id = Rpc::new(&rpc_url).chain_id().await.expect("chain_id");
        let wallet = Wallet::new(&rpc_url, key, chain_id);
        let to = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"); // hardhat #1
        let receipt = wallet
            .send_and_wait(to, Bytes::new(), U256::from(1u64), Duration::from_secs(30))
            .await
            .expect("value transfer lands");
        assert_eq!(receipt.get("status").and_then(Value::as_str), Some("0x1"));
    }
}
