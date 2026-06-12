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
    #[serde(default)]
    pub slot_deadlines: HashMap<String, u64>,
}

/// How long before a slot's order deadline its input stops counting as
/// reusable. Mirrors the indexer's `FILLER_ORDER_DEADLINE_MARGIN_SECS` (the
/// committed-input cutoff is `chain time + 30s`): once the server stops
/// counting a row, crediting its input on top of the funded balance makes the
/// replacement batch exceed `min(balance, allowance)` and every submit gets
/// rejected as not funded.
const REUSABLE_DEADLINE_MARGIN_SECS: u64 = 30;

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
    slot_deadlines: &mut HashMap<String, u64>,
    drafts: &[OrderDraft],
    spent_nonce: u64,
) {
    for draft in drafts {
        if draft.nonce == spent_nonce {
            slot_nonces.remove(&draft.slot_key);
            slot_inputs.remove(&draft.slot_key);
            slot_deadlines.remove(&draft.slot_key);
        }
    }
}

/// Input already committed by this side's live slots (`key_id` is
/// `buy:<pair>` / `sell:<pair>`) — a replacement can reuse it. Only slots whose
/// posted order is still comfortably unexpired count: once a slot's deadline
/// passes, the indexer no longer holds that input against the maker, so its
/// "credit" is gone — keeping it would size every replacement above the funded
/// balance and livelock the side on "Order batch is not funded". A slot with
/// no recorded deadline (ledger written by an older version) is treated as
/// expired; the first successful post re-records it.
pub fn reusable_slot_input(
    slot_inputs: &HashMap<String, String>,
    slot_deadlines: &HashMap<String, u64>,
    key_id: &str,
    now: u64,
) -> U256 {
    let prefix = format!("{key_id}:");
    slot_inputs
        .iter()
        .filter(|(slot_key, _)| slot_key.starts_with(&prefix))
        .filter(|(slot_key, _)| {
            slot_deadlines.get(*slot_key).is_some_and(|deadline| {
                *deadline > now.saturating_add(REUSABLE_DEADLINE_MARGIN_SECS)
            })
        })
        .fold(U256::ZERO, |sum, (_, input)| {
            sum.saturating_add(input.parse::<U256>().unwrap_or(U256::ZERO))
        })
}

/// Replace this side's recorded slot inputs (and their order deadline) with
/// the drafts just posted.
pub fn remember_slot_inputs(
    slot_inputs: &mut HashMap<String, String>,
    slot_deadlines: &mut HashMap<String, u64>,
    key_id: &str,
    drafts: &[OrderDraft],
    deadline: u64,
) {
    let prefix = format!("{key_id}:");
    slot_inputs.retain(|slot_key, _| !slot_key.starts_with(&prefix));
    slot_deadlines.retain(|slot_key, _| !slot_key.starts_with(&prefix));
    for draft in drafts {
        slot_inputs.insert(draft.slot_key.clone(), draft.input_amount.to_string());
        slot_deadlines.insert(draft.slot_key.clone(), deadline);
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

/// Load the ledger, returning `(next_nonce, slot_nonces, slot_inputs,
/// slot_deadlines)`. A missing file is a fresh start; a chain/maker mismatch
/// is an error (the file belongs to another deployment and its nonces must
/// not be reused).
#[allow(clippy::type_complexity)]
pub fn load_slot_nonce_state(
    path: &Path,
    chain_id: u64,
    maker: Address,
    initial_next_nonce: u64,
) -> anyhow::Result<(
    u64,
    HashMap<String, u64>,
    HashMap<String, String>,
    HashMap<String, u64>,
)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok((
                initial_next_nonce,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            ));
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
        state.slot_deadlines,
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
    slot_deadlines: &HashMap<String, u64>,
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
        slot_deadlines: slot_deadlines.clone(),
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

    /// Far-future deadline so slots count as reusable unless a test expires them.
    const LIVE_DEADLINE: u64 = u64::MAX;

    fn deadlines_for(slot_inputs: &HashMap<String, String>, deadline: u64) -> HashMap<String, u64> {
        slot_inputs
            .keys()
            .map(|slot_key| (slot_key.clone(), deadline))
            .collect()
    }

    #[test]
    fn forgetting_a_spent_nonce_only_rotates_that_slot() {
        let mut slot_nonces = HashMap::new();
        let mut slot_inputs = HashMap::new();
        slot_nonces.insert("buy:pair:bid:0".to_string(), 1001);
        slot_nonces.insert("buy:pair:bid:1".to_string(), 1002);
        slot_inputs.insert("buy:pair:bid:0".to_string(), "1".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "1".to_string());
        let mut slot_deadlines = deadlines_for(&slot_inputs, LIVE_DEADLINE);
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

        forget_spent_slot_nonce(
            &mut slot_nonces,
            &mut slot_inputs,
            &mut slot_deadlines,
            &drafts,
            1002,
        );

        assert_eq!(slot_nonces.get("buy:pair:bid:0"), Some(&1001));
        assert!(!slot_nonces.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_inputs.get("buy:pair:bid:0"), Some(&"1".to_string()));
        assert!(!slot_inputs.contains_key("buy:pair:bid:1"));
        assert!(slot_deadlines.contains_key("buy:pair:bid:0"));
        assert!(!slot_deadlines.contains_key("buy:pair:bid:1"));
    }

    #[test]
    fn reusable_slot_input_sums_only_the_current_side() {
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "50".to_string());
        let slot_deadlines = deadlines_for(&slot_inputs, LIVE_DEADLINE);

        assert_eq!(
            reusable_slot_input(&slot_inputs, &slot_deadlines, "buy:pair", 0),
            U256::from(125u64)
        );
    }

    #[test]
    fn reusable_slot_input_skips_expired_slots() {
        // The livelock regression: the side's posted orders expired server-side
        // (the indexer no longer counts them as committed), so their input must
        // not be credited on top of the funded balance — otherwise every
        // replacement is sized at balance + stale ladder and the indexer
        // rejects it as not funded forever.
        let now = 1_000_000u64;
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        let mut slot_deadlines = HashMap::new();
        // bid:0 expired in the past; bid:1 expires inside the 30s server margin.
        slot_deadlines.insert("buy:pair:bid:0".to_string(), now - 1);
        slot_deadlines.insert("buy:pair:bid:1".to_string(), now + 10);

        assert_eq!(
            reusable_slot_input(&slot_inputs, &slot_deadlines, "buy:pair", now),
            U256::ZERO
        );

        // Still comfortably live → fully reusable again.
        slot_deadlines.insert("buy:pair:bid:0".to_string(), now + 300);
        slot_deadlines.insert("buy:pair:bid:1".to_string(), now + 300);
        assert_eq!(
            reusable_slot_input(&slot_inputs, &slot_deadlines, "buy:pair", now),
            U256::from(125u64)
        );
    }

    #[test]
    fn reusable_slot_input_treats_missing_deadlines_as_expired() {
        // Ledgers written before slot_deadlines existed have inputs but no
        // deadlines; treating them as reusable would re-introduce the livelock
        // right after an upgrade, so they count for nothing until re-posted.
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());

        assert_eq!(
            reusable_slot_input(&slot_inputs, &HashMap::new(), "buy:pair", 0),
            U256::ZERO
        );
    }

    #[test]
    fn remembering_slot_inputs_replaces_only_the_current_side() {
        let mut slot_inputs = HashMap::new();
        slot_inputs.insert("buy:pair:bid:0".to_string(), "100".to_string());
        slot_inputs.insert("buy:pair:bid:1".to_string(), "25".to_string());
        slot_inputs.insert("sell:pair:ask:0".to_string(), "50".to_string());
        let mut slot_deadlines = deadlines_for(&slot_inputs, 500);
        let drafts = vec![draft(1001, "buy:pair:bid:0", 80)];

        remember_slot_inputs(
            &mut slot_inputs,
            &mut slot_deadlines,
            "buy:pair",
            &drafts,
            900,
        );

        assert_eq!(slot_inputs.get("buy:pair:bid:0"), Some(&"80".to_string()));
        assert!(!slot_inputs.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_inputs.get("sell:pair:ask:0"), Some(&"50".to_string()));
        assert_eq!(slot_deadlines.get("buy:pair:bid:0"), Some(&900));
        assert!(!slot_deadlines.contains_key("buy:pair:bid:1"));
        assert_eq!(slot_deadlines.get("sell:pair:ask:0"), Some(&500));
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
        let slot_deadlines = deadlines_for(&slot_inputs, 1_700_000_000);

        save_slot_nonce_state(
            &path,
            8453,
            maker,
            1002,
            &slot_nonces,
            &slot_inputs,
            &slot_deadlines,
        )
        .expect("state saves");
        let (next_nonce, loaded_nonces, loaded_inputs, loaded_deadlines) =
            load_slot_nonce_state(&path, 8453, maker, 1).expect("state loads");

        std::fs::remove_file(path).unwrap();
        assert_eq!(next_nonce, 1002);
        assert_eq!(loaded_nonces, slot_nonces);
        assert_eq!(loaded_inputs, slot_inputs);
        assert_eq!(loaded_deadlines, slot_deadlines);
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
        save_slot_nonce_state(
            &path,
            8453,
            maker,
            1002,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("state saves");

        let err = load_slot_nonce_state(&path, 8453, other, 1).expect_err("maker mismatch");

        std::fs::remove_file(path).unwrap();
        assert!(err.to_string().contains("maker"));
    }
}
