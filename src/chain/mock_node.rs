// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A JSON-RPC node for tests.
//!
//! Answers the reads the bot and the panel make — `eth_getBalance`,
//! `eth_getCode`, and `eth_call` for ERC-20 `balanceOf`, `allowance`,
//! `symbol()` and no-argument views — from fixed tables keyed by address, and
//! can sit on a request to model a node that has hung. With
//! [`MockChain::with_multicall3`] it also serves `aggregate3`, dispatching
//! each inner call through the same tables, which is what lets a test assert
//! that batching changes the request count and nothing else.
//!
//! Lives under `chain` rather than `panel` so the default build's tests can
//! reach it; `panel::http::mock_chain` re-exports it under its old name.
//! Nothing here is reachable outside `cfg(test)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{hex, keccak256, Address, Bytes, U256};
use alloy_sol_types::SolValue;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::chain::multicall::{Call3, Result3};

/// What the node reports. Token keys are lowercase `0x…` addresses.
#[derive(Default, Clone)]
pub struct MockChain {
    /// Native balance of every address (the mock doesn't look at the owner).
    pub native_wei: U256,
    pub balances: HashMap<String, U256>,
    /// Balances for one holder, keyed by `(token, owner)`, both lowercase.
    /// Falls back to `balances` for an owner with no row of its own, so a test
    /// that doesn't care who holds what doesn't have to say.
    pub owner_balances: HashMap<(String, String), U256>,
    pub allowances: HashMap<String, U256>,
    /// ERC-20 `symbol()` per token. A token not listed answers `0x` (no
    /// return data), which is what a non-contract address does.
    pub symbols: HashMap<String, String>,
    /// One word per no-argument view, keyed by `(address, 4-byte selector)`.
    /// The vault's inventory views live here; a view with no row answers `0x`,
    /// which is what a contract that doesn't have it does.
    pub views: HashMap<(String, String), U256>,
    /// Addresses with non-empty code, lowercase. `eth_getCode` answers `0x`
    /// for anything else, which is what a plain EOA does.
    pub code: HashMap<String, Vec<u8>>,
    /// Sleep this long before every answer — a hung node.
    pub delay: Option<Duration>,
}

/// Multicall3's canonical address, lowercase — where [`MockChain::with_multicall3`]
/// deploys it.
pub const MULTICALL3: &str = "0xca11bde05977b3631167028862be2a173976ca11";

// Half of these builders are only reached by the panel's handler tests, which
// compile under `--features panel`. Without the feature they are dead, and
// that is not a reason to delete them.
#[allow(dead_code)]
impl MockChain {
    pub fn balance(mut self, token: &str, atomic: u128) -> Self {
        self.balances
            .insert(token.to_lowercase(), U256::from(atomic));
        self
    }

    /// What one holder has of a token — for a bot whose capital is in a vault
    /// and whose signer wallet holds something else entirely.
    pub fn balance_of(mut self, token: &str, owner: &str, atomic: u128) -> Self {
        self.owner_balances.insert(
            (token.to_lowercase(), owner.to_lowercase()),
            U256::from(atomic),
        );
        self
    }

    pub fn symbol(mut self, token: &str, symbol: &str) -> Self {
        self.symbols
            .insert(token.to_lowercase(), symbol.to_string());
        self
    }

    /// A no-argument view answering one word: `view(vault, "quotableSettlement()", …)`.
    pub fn view(mut self, to: &str, signature: &str, value: U256) -> Self {
        let selector = alloy_primitives::hex::encode(&keccak256(signature.as_bytes())[..4]);
        self.views.insert((to.to_lowercase(), selector), value);
        self
    }

    /// The same, for a view that answers an address.
    pub fn view_address(self, to: &str, signature: &str, address: &str) -> Self {
        let address: Address = address.parse().expect("not an address");
        self.view(to, signature, U256::from_be_slice(address.as_slice()))
    }

    pub fn native(mut self, wei: u128) -> Self {
        self.native_wei = U256::from(wei);
        self
    }

    /// Give an address code, so `eth_getCode` reports it as a contract.
    pub fn with_code(mut self, address: &str, code: Vec<u8>) -> Self {
        self.code.insert(address.to_lowercase(), code);
        self
    }

    /// Deploy Multicall3 at its canonical address. Without this the node
    /// answers `eth_getCode` empty there, which is a chain that has none.
    pub fn with_multicall3(self) -> Self {
        self.with_code(MULTICALL3, vec![0x60, 0x80, 0x60, 0x40])
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
        "eth_getCode" => {
            let who = req["params"][0].as_str().unwrap_or("").to_lowercase();
            json!(hex::encode_prefixed(
                chain.code.get(&who).cloned().unwrap_or_default()
            ))
        }
        "eth_call" => {
            let tx = &req["params"][0];
            let to = tx["to"].as_str().unwrap_or("").to_lowercase();
            let data = tx["data"].as_str().unwrap_or("0x");
            json!(hex::encode_prefixed(eth_call(chain, &to, data)))
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

/// `aggregate3((address,bool,bytes)[])`.
const AGGREGATE3: &str = "82ad56cb";

/// The return bytes of one `eth_call`. Empty means no return data, which is
/// what an address without that function answers.
fn eth_call(chain: &MockChain, to: &str, data: &str) -> Vec<u8> {
    let selector = data.get(2..10).unwrap_or("");
    if to == MULTICALL3 && selector == AGGREGATE3 {
        return aggregate3(chain, data);
    }
    if let Some(word) = chain.views.get(&(to.to_string(), selector.to_string())) {
        return word.to_be_bytes::<32>().to_vec();
    }
    match selector {
        // balanceOf(address)
        "70a08231" => {
            let owner = address_arg(data, 0);
            let held = owner
                .and_then(|o| chain.owner_balances.get(&(to.to_string(), o)).copied())
                .or_else(|| chain.balances.get(to).copied())
                .unwrap_or(U256::ZERO);
            held.to_be_bytes::<32>().to_vec()
        }
        // allowance(address,address)
        "dd62ed3e" => chain
            .allowances
            .get(to)
            .copied()
            .unwrap_or(U256::ZERO)
            .to_be_bytes::<32>()
            .to_vec(),
        // symbol()
        "95d89b41" => match chain.symbols.get(to) {
            Some(symbol) => abi_string(symbol),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Run every call in a Multicall3 batch through the same tables a direct
/// `eth_call` hits, so a test cannot pass batched and fail sequential.
/// Undecodable calldata answers an empty result set, which the caller reports
/// as a length mismatch rather than reading as zeros.
fn aggregate3(chain: &MockChain, data: &str) -> Vec<u8> {
    let raw = match alloy_primitives::hex::decode(data.strip_prefix("0x").unwrap_or(data)) {
        Ok(raw) if raw.len() > 4 => raw,
        _ => return Vec::new(),
    };
    let Ok((calls,)) = <(Vec<Call3>,)>::abi_decode_params_validate(&raw[4..]) else {
        return Vec::new();
    };
    let results: Vec<Result3> = calls
        .into_iter()
        .map(|c| {
            let to = format!("{:?}", c.target).to_lowercase();
            let inner = hex::encode_prefixed(&c.callData);
            Result3 {
                success: true,
                returnData: Bytes::from(eth_call(chain, &to, &inner)),
            }
        })
        .collect();
    (results,).abi_encode_params()
}

/// The `n`th argument of a call, read as an address: the low 20 bytes of the
/// word, lowercase and `0x`-prefixed.
fn address_arg(data: &str, n: usize) -> Option<String> {
    let word = data.get(10 + n * 64..10 + (n + 1) * 64)?;
    Some(format!("0x{}", word.get(24..)?.to_lowercase()))
}

/// ABI-encode a `string` return: offset word, length word, padded bytes.
fn abi_string(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&U256::from(32u8).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(bytes.len()).to_be_bytes::<32>());
    out.extend_from_slice(bytes);
    let pad = (32 - bytes.len() % 32) % 32;
    out.extend(std::iter::repeat_n(0u8, pad));
    out
}
