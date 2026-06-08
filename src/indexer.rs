// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Client for Textile's indexer — POSTs signed orders to the filler-order
//! GraphQL mutations.

use serde_json::{json, Value};

use crate::net::http_client;
use crate::submit::SubmitOrder;

const SUBMIT_MUTATION: &str = "mutation Submit($input: SubmitFillerOrderInput!) { \
submitFillerOrder(input: $input) { id rateRay } }";
const SUBMIT_MANY_MUTATION: &str = "mutation SubmitMany($input: [SubmitFillerOrderInput!]!) { \
submitFillerOrders(input: $input) { id rateRay } }";

/// Build the GraphQL request body for one order (pure — easy to assert on).
pub fn build_submit_request(order: &SubmitOrder) -> Value {
    json!({
        "query": SUBMIT_MUTATION,
        "variables": { "input": order_input(order) }
    })
}

/// Build the GraphQL request body for an atomic order batch.
pub fn build_submit_many_request(orders: &[SubmitOrder]) -> Value {
    json!({
        "query": SUBMIT_MANY_MUTATION,
        "variables": {
            "input": orders.iter().map(order_input).collect::<Vec<_>>()
        }
    })
}

fn order_input(order: &SubmitOrder) -> Value {
    json!({
            "chainId": order.chain_id,
            "clientOrderId": order.client_order_id,
            "reactor": order.reactor,
            "maker": order.maker,
            "inputToken": order.input_token,
            "inputAmount": order.input_amount,
            "outputToken": order.output_token,
            "outputAmount": order.output_amount,
            "recipient": order.recipient,
            "nonce": order.nonce,
            "deadline": order.deadline,
            "signature": order.signature,
    })
}

fn response_errors(resp: &Value) -> Option<&Value> {
    resp.get("errors")
}

fn order_ids_at(resp: &Value, pointer: &str) -> Vec<String> {
    resp.pointer(pointer)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn first_order_id_at(resp: &Value, pointer: &str) -> String {
    resp.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub struct Indexer {
    graphql_url: String,
    client: reqwest::Client,
}

impl Indexer {
    pub fn new(graphql_url: impl Into<String>) -> Self {
        Self {
            graphql_url: graphql_url.into(),
            client: http_client(),
        }
    }

    /// POST the order; returns the new order id on success.
    pub async fn submit(&self, order: &SubmitOrder) -> anyhow::Result<String> {
        let body = build_submit_request(order);
        let resp: Value = self
            .client
            .post(&self.graphql_url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(errors) = response_errors(&resp) {
            anyhow::bail!("indexer rejected order: {errors}");
        }
        Ok(first_order_id_at(&resp, "/data/submitFillerOrder/id"))
    }

    /// POST an atomic order batch; returns the created order ids on success.
    pub async fn submit_many(&self, orders: &[SubmitOrder]) -> anyhow::Result<Vec<String>> {
        if orders.is_empty() {
            return Ok(Vec::new());
        }

        let body = build_submit_many_request(orders);
        let resp: Value = self
            .client
            .post(&self.graphql_url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(errors) = response_errors(&resp) {
            anyhow::bail!("indexer rejected order batch: {errors}");
        }
        Ok(order_ids_at(&resp, "/data/submitFillerOrders"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> SubmitOrder {
        SubmitOrder {
            chain_id: 8453,
            client_order_id: Some("bid:0".into()),
            reactor: "0xreactor".into(),
            maker: "0xmaker".into(),
            input_token: "0xusdt".into(),
            input_amount: "1000000".into(),
            output_token: "0xcngn".into(),
            output_amount: "1550000000".into(),
            recipient: "0xmaker".into(),
            nonce: "42".into(),
            deadline: "1900000000".into(),
            signature: "0xsig".into(),
        }
    }

    #[test]
    fn builds_the_graphql_mutation_with_the_order_in_variables() {
        let req = build_submit_request(&order());
        assert!(req["query"].as_str().unwrap().contains("submitFillerOrder"));
        let input = &req["variables"]["input"];
        assert_eq!(input["chainId"], 8453);
        assert_eq!(input["clientOrderId"], "bid:0");
        assert_eq!(input["maker"], "0xmaker");
        assert_eq!(input["inputAmount"], "1000000");
        assert_eq!(input["signature"], "0xsig");
    }

    #[test]
    fn builds_the_graphql_batch_mutation_with_order_array() {
        let req = build_submit_many_request(&[order(), order()]);
        assert!(req["query"]
            .as_str()
            .unwrap()
            .contains("submitFillerOrders"));
        let input = req["variables"]["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["clientOrderId"], "bid:0");
        assert_eq!(input[1]["maker"], "0xmaker");
    }
}
