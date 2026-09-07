// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A JSON-RPC node for handler tests.
//!
//! Answers the four reads the panel makes — `eth_getBalance`, and `eth_call`
//! for ERC-20 `balanceOf`, `allowance` and `symbol()` — from fixed tables keyed
//! by token address, and can sit on a request to model a node that has hung.
//! Nothing here is reachable outside `cfg(test)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::U256;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// What the node reports. Token keys are lowercase `0x…` addresses.
#[derive(Default, Clone)]
pub struct MockChain {
    /// Native balance of every address (the mock doesn't look at the owner).
    pub native_wei: U256,
    pub balances: HashMap<String, U256>,
    pub allowances: HashMap<String, U256>,
    /// ERC-20 `symbol()` per token. A token not listed answers `0x` (no
    /// return data), which is what a non-contract address does.
    pub symbols: HashMap<String, String>,
    /// Sleep this long before every answer — a hung node.
    pub delay: Option<Duration>,
}

impl MockChain {
    pub fn balance(mut self, token: &str, atomic: u128) -> Self {
        self.balances
            .insert(token.to_lowercase(), U256::from(atomic));
        self
    }

    pub fn allowance(mut self, token: &str, atomic: U256) -> Self {
        self.allowances.insert(token.to_lowercase(), atomic);
        self
    }

    pub fn symbol(mut self, token: &str, symbol: &str) -> Self {
        self.symbols
            .insert(token.to_lowercase(), symbol.to_string());
        self
    }

    pub fn native(mut self, wei: u128) -> Self {
        self.native_wei = U256::from(wei);
        self
    }

    pub fn hung_for(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

pub struct MockNode {
    pub url: String,
    /// Every JSON-RPC request the node answered.
    pub hits: Arc<AtomicUsize>,
    _server: tokio::task::JoinHandle<()>,
}

/// Serve `chain` on a loopback port.
pub async fn mock_rpc(chain: MockChain) -> MockNode {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let chain = Arc::new(chain);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let chain = chain.clone();
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                if let Some(delay) = chain.delay {
                    tokio::time::sleep(delay).await;
                }
                Json(answer(&chain, &req))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    MockNode {
        url: format!("http://{addr}"),
        hits,
        _server: server,
    }
}

fn answer(chain: &MockChain, req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(json!(1));
    let method = req["method"].as_str().unwrap_or("");
    let result = match method {
        "eth_chainId" => json!("0x1"),
        "eth_getBalance" => json!(format!("0x{:x}", chain.native_wei)),
        "eth_call" => {
            let tx = &req["params"][0];
            let to = tx["to"].as_str().unwrap_or("").to_lowercase();
            let data = tx["data"].as_str().unwrap_or("0x");
            let selector = data.get(2..10).unwrap_or("");
            match selector {
                // balanceOf(address)
                "70a08231" => uint_word(chain.balances.get(&to).copied().unwrap_or(U256::ZERO)),
                // allowance(address,address)
                "dd62ed3e" => uint_word(chain.allowances.get(&to).copied().unwrap_or(U256::ZERO)),
                // symbol()
                "95d89b41" => match chain.symbols.get(&to) {
                    Some(symbol) => abi_string(symbol),
                    None => json!("0x"),
                },
                _ => json!("0x"),
            }
        }
        other => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("mock: no such method {other}") }
            })
        }
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn uint_word(v: U256) -> Value {
    json!(format!("0x{:064x}", v))
}

/// ABI-encode a `string` return: offset word, length word, padded bytes.
fn abi_string(s: &str) -> Value {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&U256::from(32u8).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(bytes.len()).to_be_bytes::<32>());
    out.extend_from_slice(bytes);
    let pad = (32 - bytes.len() % 32) % 32;
    out.extend(std::iter::repeat_n(0u8, pad));
    json!(alloy_primitives::hex::encode_prefixed(out))
}
