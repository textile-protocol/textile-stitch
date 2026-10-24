// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! One change record, as it sits in the local journal and as it ships.
//!
//! The wire shape is the journal shape: the shipper sends lines as they were
//! written, so what Textile stores is byte-for-byte what the host recorded.
//! Field names are camelCase to match the API.

use serde::{Deserialize, Serialize};

use super::params::ParamChange;

/// Who a host edit is attributed to: nobody the change log saw.
pub const HOST_EDIT_ACTOR: &str = "unrecorded (host edit)";

/// The actor on the first reading of a bot the change log has never seen.
pub const BASELINE_ACTOR: &str = "baseline";

/// Which path wrote the change. Stable strings: they are stored off-host and
/// filtered on, so renaming one splits a bot's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// First reading of a bot with no snapshot: every value as found.
    Baseline,
    /// The file changed with nothing recording it (an editor on the host,
    /// `stitch init` / `stitch connect`, a restored backup).
    HostEdit,
    PanelSettings,
    PanelRaw,
    PanelPoolAdd,
    PanelPoolRemove,
    PanelModules,
    PanelModulesEnabled,
    PanelEnroll,
    PanelVerify,
    PanelVault,
    PanelPerps,
    /// The panel recorded the bot's ERC-8004 agent id.
    PanelIdentity,
    /// A failed live restart put the old config back.
    PanelRollback,
    PanelCreate,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Baseline => "baseline",
            Source::HostEdit => "host-edit",
            Source::PanelSettings => "panel.settings",
            Source::PanelRaw => "panel.raw",
            Source::PanelPoolAdd => "panel.pools.add",
            Source::PanelPoolRemove => "panel.pools.remove",
            Source::PanelModules => "panel.modules",
            Source::PanelModulesEnabled => "panel.modules.enabled",
            Source::PanelEnroll => "panel.enroll",
            Source::PanelVerify => "panel.verify",
            Source::PanelVault => "panel.vault",
            Source::PanelPerps => "panel.perps",
            Source::PanelIdentity => "panel.identity",
            Source::PanelRollback => "panel.rollback",
            Source::PanelCreate => "panel.create",
        }
    }
}

/// Who changed the config and through what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub source: Source,
    pub actor: String,
    /// The panel's name for the bot, when a panel did it. Informational: the
    /// bot itself doesn't know what the panel calls it.
    pub bot: Option<String>,
}

impl Origin {
    pub fn host_edit(bot: Option<String>) -> Self {
        Self {
            source: Source::HostEdit,
            actor: HOST_EDIT_ACTOR.to_string(),
            bot,
        }
    }

    pub fn baseline(bot: Option<String>) -> Self {
        Self {
            source: Source::Baseline,
            actor: BASELINE_ACTOR.to_string(),
            bot,
        }
    }
}

/// When a change happened and when it was noticed, in unix milliseconds.
///
/// The two differ only for a host edit: the change is the file's mtime, and
/// the detection is whoever read it next (the bot starting, or a panel save).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub changed_at_ms: u64,
    pub detected_at_ms: Option<u64>,
}

impl Timing {
    pub fn at(ms: u64) -> Self {
        Self {
            changed_at_ms: ms,
            detected_at_ms: None,
        }
    }
}

/// One parameter that changed, with everything needed to read it alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRecord {
    /// Random UUID, generated where the change was recorded. The API dedupes
    /// on it, so a batch that ships twice is stored once.
    pub id: String,
    /// Shared by every parameter one save changed.
    pub change_set_id: String,
    /// ISO-8601 UTC, millisecond precision.
    pub changed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_at: Option<String>,
    pub source: String,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    pub chain_id: u64,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_label: Option<String>,
    pub parameter: String,
    pub unit: String,
    pub before: Option<String>,
    pub after: Option<String>,
    /// SHA-256 of the whole config before and after, so a record can be tied
    /// back to the exact file. `None` before the first file or after a delete.
    pub config_sha256_before: Option<String>,
    pub config_sha256_after: Option<String>,
}

/// The two config hashes a set of records is tied to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hashes {
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Turn a diff into records. Pure: ids come from `next_id`, so a test can pin
/// them and production passes [`random_uuid`].
pub fn to_records(
    changes: &[ParamChange],
    origin: &Origin,
    timing: Timing,
    chain_id: u64,
    hashes: &Hashes,
    mut next_id: impl FnMut() -> String,
) -> Vec<ChangeRecord> {
    if changes.is_empty() {
        return Vec::new();
    }
    let change_set_id = next_id();
    changes
        .iter()
        .map(|c| ChangeRecord {
            id: next_id(),
            change_set_id: change_set_id.clone(),
            changed_at: crate::rfq::iso8601::format_iso_ms(timing.changed_at_ms),
            detected_at: timing
                .detected_at_ms
                .map(crate::rfq::iso8601::format_iso_ms),
            source: origin.source.as_str().to_string(),
            actor: origin.actor.clone(),
            bot: origin.bot.clone(),
            chain_id,
            scope: c.key.scope.clone(),
            scope_label: c.scope_label.clone(),
            parameter: c.key.name.clone(),
            unit: c.unit.clone(),
            before: c.before.clone(),
            after: c.after.clone(),
            config_sha256_before: hashes.before.clone(),
            config_sha256_after: hashes.after.clone(),
        })
        .collect()
}

/// A version-4 UUID from the OS RNG.
pub fn random_uuid() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// SHA-256 of a config's text, hex.
pub fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changelog::params::ParamKey;

    fn change(name: &str, before: Option<&str>, after: Option<&str>) -> ParamChange {
        ParamChange {
            key: ParamKey {
                scope: "bot".into(),
                name: name.into(),
            },
            scope_label: None,
            unit: "bps".into(),
            before: before.map(str::to_string),
            after: after.map(str::to_string),
        }
    }

    #[test]
    fn one_save_is_one_change_set_with_a_record_per_parameter() {
        let mut n = 0;
        let ids = || {
            n += 1;
            format!("id-{n}")
        };
        let origin = Origin {
            source: Source::PanelSettings,
            actor: "tailnet:ops@textile.example".into(),
            bot: Some("acme-zaru".into()),
        };
        let records = to_records(
            &[
                change("max_loss_bps", Some("1500"), Some("800")),
                change("max_jump_bps", Some("1000"), Some("600")),
            ],
            &origin,
            Timing::at(1_754_388_000_000),
            8453,
            &Hashes {
                before: Some("aa".into()),
                after: Some("bb".into()),
            },
            ids,
        );
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.change_set_id == "id-1"));
        assert_eq!(records[0].id, "id-2");
        assert_eq!(records[0].changed_at, "2025-08-05T10:00:00.000Z");
        assert_eq!(records[0].detected_at, None);
        assert_eq!(records[0].source, "panel.settings");
        assert_eq!(records[0].actor, "tailnet:ops@textile.example");
        assert_eq!(records[1].before.as_deref(), Some("1000"));
        assert_eq!(records[1].after.as_deref(), Some("600"));
    }

    #[test]
    fn no_changes_mint_no_change_set() {
        let mut called = false;
        let records = to_records(
            &[],
            &Origin::baseline(None),
            Timing::at(0),
            1,
            &Hashes::default(),
            || {
                called = true;
                String::new()
            },
        );
        assert!(records.is_empty());
        assert!(!called);
    }

    #[test]
    fn uuids_are_version_four_and_distinct() {
        let a = random_uuid();
        let b = random_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "{a}");
    }

    #[test]
    fn the_wire_shape_is_camel_case_and_keeps_unset_values_explicit() {
        let record = to_records(
            &[change("buy_offset_bps", Some("40"), None)],
            &Origin::host_edit(None),
            Timing {
                changed_at_ms: 1_000,
                detected_at_ms: Some(2_000),
            },
            8453,
            &Hashes::default(),
            random_uuid,
        )
        .remove(0);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["changeSetId"].as_str().map(str::len), Some(36));
        assert_eq!(json["detectedAt"], "1970-01-01T00:00:02.000Z");
        assert_eq!(json["actor"], HOST_EDIT_ACTOR);
        assert_eq!(json["source"], "host-edit");
        assert!(json["after"].is_null(), "unset is an explicit null: {json}");
        assert!(json.get("bot").is_none());
        let back: ChangeRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);
    }
}
