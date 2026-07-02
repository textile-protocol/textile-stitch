// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The embedded corridor catalog: each entry is a friendly label plus the
//! `stitch.toml` we ship for that corridor, verbatim. Setup writes the template
//! as-is; the wallet key never lives in the TOML, so no substitution is needed.

/// One selectable corridor in the setup picker.
#[derive(Debug, Clone, Copy)]
pub struct Corridor {
    /// Stable machine id used for lookups (e.g. "cngn-usdt-bsc").
    pub id: &'static str,
    /// Asset pair shown in the picker (e.g. "cNGN / USDT").
    pub display_name: &'static str,
    /// Network shown next to the pair (e.g. "BNB Smart Chain").
    pub network_label: &'static str,
    /// Chain id; also used to match a written config back to a corridor.
    pub chain_id: u64,
    /// The `stitch.toml` body shipped for this corridor.
    pub toml_template: &'static str,
}

const CORRIDORS: &[Corridor] = &[
    Corridor {
        id: "cngn-usdt-bsc",
        display_name: "cNGN / USDT",
        network_label: "BNB Smart Chain",
        chain_id: 56,
        toml_template: include_str!("templates/cngn-usdt-bsc.toml"),
    },
    Corridor {
        id: "brla-usdt-celo",
        display_name: "BRLA / USDT",
        network_label: "Celo",
        chain_id: 42220,
        toml_template: include_str!("templates/brla-usdt-celo.toml"),
    },
];

/// All corridors, in display order (first is the recommended default).
pub fn catalog() -> &'static [Corridor] {
    CORRIDORS
}

/// Look a corridor up by its stable id.
pub fn find_corridor(id: &str) -> Option<&'static Corridor> {
    CORRIDORS.iter().find(|c| c.id == id)
}

/// Best-effort: match a written `stitch.toml` back to a catalog corridor by its
/// chain id, so the control panel can name an already-configured folder.
pub fn identify_corridor(toml_str: &str) -> Option<&'static Corridor> {
    let cfg = crate::config::Config::from_toml(toml_str).ok()?;
    CORRIDORS.iter().find(|c| c.chain_id == cfg.chain_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_not_empty_and_ids_are_unique() {
        let ids: Vec<_> = catalog().iter().map(|c| c.id).collect();
        assert!(!ids.is_empty());
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "corridor ids must be unique");
    }

    #[test]
    fn every_template_parses_as_a_valid_config() {
        for c in catalog() {
            crate::config::Config::from_toml(c.toml_template)
                .unwrap_or_else(|e| panic!("corridor {} has an invalid template: {e}", c.id));
        }
    }

    #[test]
    fn template_chain_id_matches_catalog_metadata() {
        for c in catalog() {
            let cfg = crate::config::Config::from_toml(c.toml_template).unwrap();
            assert_eq!(cfg.chain_id, c.chain_id, "chain_id mismatch for {}", c.id);
        }
    }

    #[test]
    fn find_and_identify_round_trip() {
        let bsc = find_corridor("cngn-usdt-bsc").expect("bsc corridor exists");
        assert_eq!(identify_corridor(bsc.toml_template).unwrap().id, bsc.id);
        assert!(find_corridor("does-not-exist").is_none());
    }

    #[test]
    fn every_corridor_is_identified_from_its_own_template() {
        // Switching corridor in the desktop app writes a corridor's template
        // verbatim; the panel then re-identifies it by chain id. Guard that round
        // trip for every corridor so a switch always yields a config the app can
        // name.
        for c in catalog() {
            let identified = identify_corridor(c.toml_template)
                .unwrap_or_else(|| panic!("corridor {} not identified from its template", c.id));
            assert_eq!(identified.id, c.id, "identify mismatch for {}", c.id);
        }
    }
}
