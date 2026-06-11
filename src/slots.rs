// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Slot-nonce ledger: each replacement slot (`buy:<pair>:bid:0`, …) keeps a
//! stable Permit2 nonce across re-quotes so a new order supersedes the old one
//! instead of stacking next to it. The ledger (next nonce, per-slot nonces, and
//! per-slot posted inputs) persists to a JSON file beside the config so a
//! restart can't re-issue a nonce that may already be live or spent.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256};
use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::poster::OrderDraft;

/// On-disk shape of the ledger. Serde field names are the file format — do not
/// rename them.
#[derive(Debug, Deserialize, Serialize)]
pub struct SlotNonceState {
    pub chain_id: u64,
    pub maker: String,
    pub next_nonce: u64,
    pub slot_nonces: HashMap<String, u64>,
    #[serde(default)]
    pub slot_inputs: HashMap<String, String>,
}

/// Stable nonce for a replacement slot: reuse the slot's nonce if it has one,
/// otherwise mint the next one.
pub fn slot_nonce(
    slot_nonces: &mut HashMap<String, u64>,
    next_nonce: &mut u64,
    slot_key: impl Into<String>,
) -> u64 {
    *slot_nonces.entry(slot_key.into()).or_insert_with(|| {
        *next_nonce = next_nonce.saturating_add(1);
        *next_nonce
    })
}

/// Drop the slot whose nonce the chain reports as spent, so the next quote
/// mints a fresh nonce for that slot only.
pub fn forget_spent_slot_nonce(
    slot_nonces: &mut HashMap<String, u64>,
    slot_inputs: &mut HashMap<String, String>,
    drafts: &[OrderDraft],
    spent_nonce: u64,
) {
    for draft in drafts {
        if draft.nonce == spent_nonce {
            slot_nonces.remove(&draft.slot_key);
            slot_inputs.remove(&draft.slot_key);
        }
    }
}

/// Input already committed by this side's live slots (`key_id` is
/// `buy:<pair>` / `sell:<pair>`) — a replacement can reuse it.
pub fn reusable_slot_input(slot_inputs: &HashMap<String, String>, key_id: &str) -> U256 {
    let prefix = format!("{key_id}:");
    slot_inputs
        .iter()
        .filter(|(slot_key, _)| slot_key.starts_with(&prefix))
        .fold(U256::ZERO, |sum, (_, input)| {
            sum.saturating_add(input.parse::<U256>().unwrap_or(U256::ZERO))
        })
}

/// Replace this side's recorded slot inputs with the drafts just posted.
pub fn remember_slot_inputs(
    slot_inputs: &mut HashMap<String, String>,
    key_id: &str,
    drafts: &[OrderDraft],
) {
    let prefix = format!("{key_id}:");
    slot_inputs.retain(|slot_key, _| !slot_key.starts_with(&prefix));
    for draft in drafts {
        slot_inputs.insert(draft.slot_key.clone(), draft.input_amount.to_string());
    }
}

/// Ledger file path: beside the config, scoped by chain and maker.
pub fn slot_nonce_state_path(config_path: &str, chain_id: u64, maker: Address) -> PathBuf {
    let config_path = Path::new(config_path);
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("stitch");
    dir.join(format!(
        "{stem}.{chain_id}.{}.slot-nonces.json",
        maker.to_string().to_lowercase()
    ))
}

/// Load the ledger, returning `(next_nonce, slot_nonces, slot_inputs)`. A
/// missing file is a fresh start; a chain/maker mismatch is an error (the file
/// belongs to another deployment and its nonces must not be reused).
pub fn load_slot_nonce_state(
    path: &Path,
    chain_id: u64,
    maker: Address,
    initial_next_nonce: u64,
) -> anyhow::Result<(u64, HashMap<String, u64>, HashMap<String, String>)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok((initial_next_nonce, HashMap::new(), HashMap::new()));
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", path.display()));
        }
    };
    let state: SlotNonceState =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let maker = maker.to_string().to_lowercase();
    anyhow::ensure!(
        state.chain_id == chain_id,
        "slot nonce state chain_id {} does not match {chain_id}",
        state.chain_id
    );
    anyhow::ensure!(
        state.maker.eq_ignore_ascii_case(&maker),
        "slot nonce state maker {} does not match {maker}",
        state.maker
    );
    for (slot_key, input) in &state.slot_inputs {
        input.parse::<U256>().with_context(|| {
            format!(
                "invalid slot input amount for {slot_key} in {}",
                path.display()
            )
        })?;
    }
    let max_slot_nonce = state.slot_nonces.values().copied().max().unwrap_or(0);
    Ok((
        initial_next_nonce.max(state.next_nonce).max(max_slot_nonce),
        state.slot_nonces,
        state.slot_inputs,
    ))
}

/// Persist the ledger atomically (write to a temp file, then rename).
pub fn save_slot_nonce_state(
    path: &Path,
    chain_id: u64,
    maker: Address,
    next_nonce: u64,
    slot_nonces: &HashMap<String, u64>,
    slot_inputs: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let state = SlotNonceState {
        chain_id,
        maker: maker.to_string().to_lowercase(),
        next_nonce,
        slot_nonces: slot_nonces.clone(),
        slot_inputs: slot_inputs.clone(),
    };
    let mut json = serde_json::to_string_pretty(&state)?;
    json.push('\n');
    let mut tmp = path.to_path_buf();
    tmp.set_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::unix_now;

    fn temp_state_file(label: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "stitch-test-state-{label}-{}-{}.json",
            std::process::id(),
            unix_now()
        ));
        path
    }

    fn draft(nonce: u64, slot_key: &str, input: u64) -> OrderDraft {
        OrderDraft {
            nonce,
            slot_key: slot_key.to_string(),
            input_amount: U256::from(input),
            output_amount: U256::from(1u64),
            client_order_id: None,
        }
    }

    #[test]
    fn slot_nonce_is_stable_per_replacement_slot() {
        let mut slot_nonces = HashMap::new();
        let mut next_nonce = 1_000u64;

        let bid_0 = slot_nonce(&mut slot_nonces, &mut next_nonce, "buy:pair:bid:0");
        let bid_1 = slot_nonce(&mut slot_nonces, &mut next_nonce, "buy:pair:bid:1");
        let bid_0_again = slot_nonce(&mut slot_nonces, &mut next_nonce, "buy:pair:bid:0");

        assert_eq!(bid_0, 1_001);
        assert_eq!(bid_1, 1_002);
        assert_eq!(bid_0_again, bid_0);
    }

    #[test]
    fn forgetting_a_spent_nonce_only_rotates_that_slot() {
        let mut slot_nonces = HashMap::new();
        let mut slot_inputs = HashMap::new();
        slot_nonces.insert("buy:pair:bid:0".to_string(), 1001);
        slot_nonces.insert("buy:pair:bid:1".to_string(), 1002);
        slot_inputs.insert("buy:pair:bid:0".to_string(), "1".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "1".to_string());
        let drafts = vec![
            OrderDraft {
                nonce: 1001,
                slot_key: "buy:pair:bid:0".to_string(),
                input_amount: U256::from(1u64),
                output_amount: U256::from(1u64),
                client_order_id: Some("bid:0".to_string()),
            },
            OrderDraft {
                nonce: 1002,
                slot_key: "buy:pair:bid:1".to_string(),
                input_amount: U256::from(1u64),
                output_amount: U256::from(1u64),
                client_order_id: Some("bid:1".to_string()),
            },
        ];

        forget_spent_slot_nonce(&mut slot_nonces, &mut slot_inputs, &drafts, 1002);

        assert_eq!(slot_nonces.get("buy:pair:bid:0"), Some(&1001));
        assert!(!slot_nonces.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_inputs.get("buy:pair:bid:0"), Some(&"1".to_string()));
        assert!(!slot_inputs.contains_key("buy:pair:bid:1"));
    }

    #[test]
    fn reusable_slot_input_sums_only_the_current_side() {
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "50".to_string());

        assert_eq!(
            reusable_slot_input(&slot_inputs, "buy:pair"),
            U256::from(125u64)
        );
    }

    #[test]
    fn remembering_slot_inputs_replaces_only_the_current_side() {
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "50".to_string());
        let drafts = vec![draft(1001, "buy:pair:bid:0", 80)];

        remember_slot_inputs(&mut slot_inputs, "buy:pair", &drafts);

        assert_eq!(slot_inputs.get("buy:pair:bid:0"), Some(&"80".to_string()));
        assert!(!slot_inputs.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_inputs.get("sell:pair:ask:0"), Some(&"50".to_string()));
    }

    #[test]
    fn slot_nonce_state_path_is_scoped_by_chain_and_maker() {
        let maker: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();

        let path = slot_nonce_state_path("/tmp/stitch.toml", 8453, maker);

        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/stitch.8453.0x00000000000000000000000000000000000000aa.slot-nonces.json"
            )
        );
    }

    #[test]
    fn persisted_slot_nonce_state_round_trips() {
        let maker: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let path = temp_state_file("round-trip");
        let mut slot_nonces = HashMap::new();
        let mut slot_inputs = HashMap::new();
        slot_nonces.insert("buy:pair:bid:0".to_string(), 1001);
        slot_nonces.insert("sell:pair:ask:0".to_string(), 1002);
        slot_inputs.insert("buy:pair:bid:0".to_string(), "10".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "20".to_string());

        save_slot_nonce_state(&path, 8453, maker, 1002, &slot_nonces, &slot_inputs)
            .expect("state saves");
        let (next_nonce, loaded_nonces, loaded_inputs) =
            load_slot_nonce_state(&path, 8453, maker, 1).expect("state loads");

        std::fs::remove_file(path).unwrap();
        assert_eq!(next_nonce, 1002);
        assert_eq!(loaded_nonces, slot_nonces);
        assert_eq!(loaded_inputs, slot_inputs);
    }

    #[test]
    fn persisted_slot_nonce_state_rejects_wrong_maker() {
        let maker: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let other: Address = "0x00000000000000000000000000000000000000bb"
            .parse()
            .unwrap();
        let path = temp_state_file("wrong-maker");
        save_slot_nonce_state(&path, 8453, maker, 1002, &HashMap::new(), &HashMap::new())
            .expect("state saves");

        let err = load_slot_nonce_state(&path, 8453, other, 1).expect_err("maker mismatch");

        std::fs::remove_file(path).unwrap();
        assert!(err.to_string().contains("maker"));
    }
}
