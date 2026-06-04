// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Blue-leg discovery: query the subgraph for a pool's OPEN positions — the
//! candidates the closer evaluates. Ordered by positionId ascending, which is
//! the pool's FIFO close order.

use alloy_primitives::U256;
use serde_json::{json, Value};

use crate::net::http_client;

use super::strategy::ClosePosition;

const OPEN_POSITIONS_QUERY: &str = "query OpenPositions($pool: String!, $first: Int!) { \
settlementPositions(where: {pool: $pool, status: OPEN}, first: $first, orderBy: positionId, orderDirection: asc) \
{ positionId remainingC remainingD openTime } }";

/// GraphQL request body for a pool's OPEN positions (pure — easy to assert on).
pub fn build_open_positions_request(pool: &str, first: u32) -> Value {
    json!({
        "query": OPEN_POSITIONS_QUERY,
        "variables": { "pool": pool.to_lowercase(), "first": first },
    })
}

/// Parse a subgraph response into FIFO-ordered close candidates.
pub fn parse_open_positions(resp: &Value) -> anyhow::Result<Vec<ClosePosition>> {
    let nodes = resp
        .pointer("/data/settlementPositions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing data.settlementPositions"))?;

    Ok(nodes
        .iter()
        .map(|n| {
            let s = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or("0");
            ClosePosition {
                position_id: s("positionId").parse().unwrap_or(U256::ZERO),
                c: s("remainingC").parse().unwrap_or(U256::ZERO),
                d: s("remainingD").parse().unwrap_or(U256::ZERO),
                open_time: s("openTime").parse().unwrap_or(0),
            }
        })
        .collect())
}

pub struct Discoverer {
    subgraph_url: String,
    client: reqwest::Client,
}

impl Discoverer {
    pub fn new(subgraph_url: impl Into<String>) -> Self {
        Self {
            subgraph_url: subgraph_url.into(),
            client: http_client(),
        }
    }

    pub async fn open_positions(
        &self,
        pool: &str,
        first: u32,
    ) -> anyhow::Result<Vec<ClosePosition>> {
        let body = build_open_positions_request(pool, first);
        let resp: Value = self
            .client
            .post(&self.subgraph_url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(errors) = resp.get("errors") {
            anyhow::bail!("subgraph error: {errors}");
        }
        parse_open_positions(&resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_query_with_lowercased_pool() {
        let req = build_open_positions_request("0xABCD", 100);
        assert!(req["query"]
            .as_str()
            .unwrap()
            .contains("settlementPositions"));
        assert_eq!(req["variables"]["pool"], "0xabcd");
        assert_eq!(req["variables"]["first"], 100);
    }

    #[test]
    fn parses_positions_in_fifo_order() {
        let resp = json!({ "data": { "settlementPositions": [
            { "positionId": "0", "remainingC": "1550000000", "remainingD": "1000000000", "openTime": "1700000000" },
            { "positionId": "1", "remainingC": "500", "remainingD": "400", "openTime": "1700000100" }
        ]}});
        let ps = parse_open_positions(&resp).unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].position_id, U256::ZERO);
        assert_eq!(ps[0].c, U256::from(1_550_000_000u64));
        assert_eq!(ps[1].open_time, 1_700_000_100);
    }

    #[test]
    fn errors_on_missing_data() {
        assert!(parse_open_positions(&json!({ "data": {} })).is_err());
    }
}
