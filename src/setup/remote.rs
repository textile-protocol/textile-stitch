// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The corridor list, read from Textile's API instead of this binary.
//!
//! The corridor table on the API is the source of truth for which markets exist.
//! Baking the list into Stitch meant a corridor listed on the site was invisible
//! in the panel until someone cut a release — so the wizard asks the API, and
//! writes the `stitch.toml` the API renders for whichever corridor is picked.
//!
//! The query is public (`stitchCorridors`, `@skipAuth`): the panel runs on the
//! operator's own machine and has no Textile credential at wizard time. Nothing
//! secret comes back — the rendered file carries the chain's free public RPC,
//! not Textile's metered one, and never a `[signer]` section.
//!
//! Every template is parsed with the bot's own config parser before it is
//! offered. A corridor whose file we can't load is dropped rather than shown:
//! the alternative is an operator picking it and the create failing after they
//! have typed a wallet key.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::indexer::graphql_url_from_base;
use crate::net::http_client;
use crate::setup::CorridorEntry;

/// Textile's API origin — where the corridor list lives when nothing overrides it.
pub const DEFAULT_CORRIDOR_API: &str = "https://api.textilecredit.com";

const QUERY: &str = "query StitchCorridors { stitchCorridors { id displayName networkLabel chainId tomlTemplate } }";

/// One row as the API returns it, before validation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    id: String,
    display_name: String,
    network_label: String,
    chain_id: u64,
    toml_template: String,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Option<Data>,
    #[serde(default)]
    errors: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    #[serde(default)]
    stitch_corridors: Option<Vec<Row>>,
}

/// Fetch the corridors Textile currently lists.
///
/// `base` is the API origin (`https://api.textilecredit.com`); the GraphQL path
/// is appended the same way the bot's indexer client does it, so an operator who
/// points both at a staging origin gets a consistent pair.
pub async fn fetch_corridors(base: &str) -> Result<Vec<CorridorEntry>> {
    let url = graphql_url_from_base(base);
    let body = serde_json::json!({ "query": QUERY });
    let envelope: Envelope = http_client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("asking {url} for the corridor list"))?
        .error_for_status()
        .with_context(|| format!("{url} rejected the corridor list request"))?
        .json()
        .await
        .with_context(|| format!("{url} did not answer with JSON"))?;

    if let Some(errors) = envelope.errors {
        bail!("{url} rejected the corridor list: {errors}");
    }
    let rows = envelope
        .data
        .and_then(|d| d.stitch_corridors)
        .with_context(|| format!("{url} answered without a corridor list"))?;

    Ok(rows.into_iter().filter_map(usable).collect())
}

/// A row we'd actually let an operator pick, or `None`.
///
/// The parse is the gate. A template the bot can't load is a corridor that would
/// fail at create time, after the operator has already typed a wallet key — so
/// drop it here and let the rest of the list stand. The whole fetch failing over
/// one bad row would be worse: it would take every good corridor with it.
fn usable(row: Row) -> Option<CorridorEntry> {
    if row.id.trim().is_empty() || row.chain_id == 0 {
        return None;
    }
    crate::config::Config::from_toml(&row.toml_template).ok()?;
    Some(CorridorEntry {
        id: row.id,
        display_name: row.display_name,
        network_label: row.network_label,
        chain_id: row.chain_id,
        toml_template: row.toml_template,
        // Nothing the API lists is pending: it only renders a config for a chain
        // that has a reactor, which is the whole meaning of the flag.
        pending_deploy: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    fn template(chain_id: u64) -> String {
        format!(
            r#"
chain_id = {chain_id}
rpc_url = "https://rpc.example.com"
indexer_url = "https://api.example.com"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"
tick_interval_secs = 5

[feed]
url = "https://api.example.com/price?pair=cngn-usdt"
staleness_secs = 900

[[pools]]
collateral = "0xF6829D7393dAe24509eb1E52eE8e572e2E271a4f"
collateral_decimals = 6
debt = "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e"
debt_decimals = 6
buy_offset_bps = 5
buy_total_liquidity_debt = "max"
buy_min_slice_debt = "10000000"
buy_max_orders = 40
sell_offset_bps = 5
sell_total_liquidity_collateral = "max"
sell_min_slice_debt = "10000000"
sell_max_orders = 40
ttl_secs = 60
refresh_threshold_bps = 0
"#
        )
    }

    /// A GraphQL server that answers `stitchCorridors` with whatever we hand it.
    async fn mock_api(response: Value) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/graphql",
            post(move |Json(body): Json<Value>| {
                let response = response.clone();
                async move {
                    assert!(
                        body["query"].as_str().unwrap().contains("stitchCorridors"),
                        "the panel must ask for the Stitch catalog"
                    );
                    Json(response)
                }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn a_listed_corridor_comes_back_ready_to_write() {
        let (base, _server) = mock_api(json!({
            "data": { "stitchCorridors": [{
                "id": "cmcorridor1",
                "displayName": "cNGN → USDT",
                "networkLabel": "Celo",
                "chainId": 42220,
                "tomlTemplate": template(42220),
            }]}
        }))
        .await;

        let corridors = fetch_corridors(&base).await.unwrap();
        assert_eq!(corridors.len(), 1);
        assert_eq!(corridors[0].id, "cmcorridor1");
        assert_eq!(corridors[0].chain_id, 42220);
        assert!(!corridors[0].pending_deploy);
        // The guarantee the whole feature rests on: what the API sent is a file
        // the bot can load.
        crate::config::Config::from_toml(&corridors[0].toml_template).unwrap();
    }

    #[tokio::test]
    async fn a_template_the_bot_cannot_load_is_dropped_not_offered() {
        // One bad row must not take the good ones with it, and must not reach a
        // picker where choosing it fails after the operator types a wallet key.
        let (base, _server) = mock_api(json!({
            "data": { "stitchCorridors": [
                {
                    "id": "cmbroken",
                    "displayName": "broken",
                    "networkLabel": "Celo",
                    "chainId": 42220,
                    "tomlTemplate": "chain_id = 42220\nthis is not toml",
                },
                {
                    "id": "cmgood",
                    "displayName": "cNGN → USDT",
                    "networkLabel": "Celo",
                    "chainId": 42220,
                    "tomlTemplate": template(42220),
                },
            ]}
        }))
        .await;

        let corridors = fetch_corridors(&base).await.unwrap();
        assert_eq!(corridors.len(), 1, "{corridors:?}");
        assert_eq!(corridors[0].id, "cmgood");
    }

    #[tokio::test]
    async fn graphql_errors_fail_the_fetch_rather_than_returning_an_empty_list() {
        // An empty list is a meaningful answer ("nothing is listed"); an error is
        // not. Conflating them would silently replace the catalog with nothing.
        let (base, _server) = mock_api(json!({
            "errors": [{ "message": "boom" }]
        }))
        .await;

        let err = fetch_corridors(&base).await.unwrap_err();
        assert!(
            err.to_string().contains("rejected the corridor list"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_api_is_an_error_the_caller_can_fall_back_from() {
        let err = fetch_corridors("http://127.0.0.1:1").await.unwrap_err();
        assert!(err.to_string().contains("corridor list"), "{err}");
    }
}
