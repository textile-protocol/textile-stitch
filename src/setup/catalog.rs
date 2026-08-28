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
    /// The corridor's contracts aren't on-chain yet, so its template still
    /// carries a placeholder `reactor`. The preset is listed (so operators can
    /// see what's coming and read the config it will write) but a bot can't be
    /// created for it — one would quote into a reactor that doesn't exist.
    /// Clear this in the same change that fills the address; the catalog tests
    /// fail both ways, so the flag can't outlive the placeholder.
    pub pending_deploy: bool,
}

/// A corridor the panel can offer, from either source: a shipped preset or a row
/// the Textile API lists.
///
/// Owned rather than `&'static`, because a corridor fetched at runtime has no
/// static lifetime to borrow from. Every caller that used to hold a
/// `&'static Corridor` — create, switch, add-pool — takes one of these instead,
/// so the two sources are indistinguishable downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorridorEntry {
    pub id: String,
    pub display_name: String,
    pub network_label: String,
    pub chain_id: u64,
    pub toml_template: String,
    pub pending_deploy: bool,
}

impl From<&Corridor> for CorridorEntry {
    fn from(c: &Corridor) -> Self {
        Self {
            id: c.id.to_string(),
            display_name: c.display_name.to_string(),
            network_label: c.network_label.to_string(),
            chain_id: c.chain_id,
            toml_template: c.toml_template.to_string(),
            pending_deploy: c.pending_deploy,
        }
    }
}

const CORRIDORS: &[Corridor] = &[
    Corridor {
        id: "cngn-usdt-bsc",
        display_name: "cNGN / USDT",
        network_label: "BNB Smart Chain",
        chain_id: 56,
        toml_template: include_str!("templates/cngn-usdt-bsc.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "cngn-usdt-celo",
        display_name: "cNGN / USDT",
        network_label: "Celo",
        chain_id: 42220,
        toml_template: include_str!("templates/cngn-usdt-celo.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "usdc-usdt-celo",
        display_name: "USDC / USDT",
        network_label: "Celo",
        chain_id: 42220,
        toml_template: include_str!("templates/usdc-usdt-celo.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "cngn-usdc-base",
        display_name: "cNGN / USDC",
        network_label: "Base",
        chain_id: 8453,
        toml_template: include_str!("templates/cngn-usdc-base.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "xaut-usdt-ethereum",
        display_name: "XAUt / USDT",
        network_label: "Ethereum",
        chain_id: 1,
        toml_template: include_str!("templates/xaut-usdt-ethereum.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "weth-usdt-ethereum",
        display_name: "WETH / USDT",
        network_label: "Ethereum",
        chain_id: 1,
        toml_template: include_str!("templates/weth-usdt-ethereum.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "wars-usdt-celo",
        display_name: "wARS / USDT",
        network_label: "Celo",
        chain_id: 42220,
        toml_template: include_str!("templates/wars-usdt-celo.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "wbrl-usdt-celo",
        display_name: "wBRL / USDT",
        network_label: "Celo",
        chain_id: 42220,
        toml_template: include_str!("templates/wbrl-usdt-celo.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "nvda-usdg-robinhood",
        display_name: "NVDA / USDG",
        network_label: "Robinhood Chain",
        chain_id: 4663,
        toml_template: include_str!("templates/nvda-usdg-robinhood.toml"),
        pending_deploy: false,
    },
    Corridor {
        id: "cngn-usdt-bsc-testnet",
        display_name: "cNGN / USDT",
        network_label: "BNB Smart Chain testnet",
        chain_id: 97,
        toml_template: include_str!("templates/cngn-usdt-bsc-testnet.toml"),
        pending_deploy: false,
    },
];

/// All corridors, in display order (first is the recommended default).
pub fn catalog() -> &'static [Corridor] {
    CORRIDORS
}

/// Corridors an operator can actually stand a bot up on right now — the catalog
/// minus anything still `pending_deploy`. A pending corridor's preset points at
/// a zero reactor, so a bot built from it would quote into nothing while looking
/// healthy. Any "pick a corridor to set up" surface (the CLI `init` picker, the
/// web wizard) should offer this, not the raw catalog. Guaranteed non-empty by
/// `catalog_has_at_least_one_deployable_corridor`.
pub fn deployable_catalog() -> Vec<&'static Corridor> {
    CORRIDORS.iter().filter(|c| !c.pending_deploy).collect()
}

/// Look a corridor up by its stable id.
pub fn find_corridor(id: &str) -> Option<&'static Corridor> {
    CORRIDORS.iter().find(|c| c.id == id)
}

/// Best-effort: match a written `stitch.toml` back to a catalog corridor so the
/// control panel can name an already-configured folder. Chain id alone is not
/// enough — a chain can host more than one corridor (e.g. wARS and wBRL on Celo) —
/// so disambiguate on the first pool's **pair**: both the collateral (soft) and
/// debt (stable) token. Collateral alone isn't enough either: a custom pair can
/// reuse a catalog collateral against a different debt (cNGN/USDC where the
/// catalog ships cNGN/USDT on the same chain), and matching on collateral only
/// would mislabel it as that preset.
///
/// The chain-only fallback is *only* for a config with no pool to key on (older,
/// pool-less configs). When a pool IS present but its pair matches no catalog
/// corridor on that chain, this is a pair we don't ship — a custom corridor — so
/// return `None` rather than the first catalog entry that happens to share the
/// chain, which would make Settings preselect that unrelated preset.
pub fn identify_corridor(toml_str: &str) -> Option<&'static Corridor> {
    let cfg = crate::config::Config::from_toml(toml_str).ok()?;
    let pair = cfg
        .pools
        .first()
        .map(|p| (p.collateral.to_lowercase(), p.debt.to_lowercase()));
    pair.as_ref()
        .and_then(|(collateral, debt)| identify_pair(cfg.chain_id, collateral, debt))
        .or_else(|| {
            // No pool → nothing to disambiguate on, so a chain-only match is the
            // best we can do. A present-but-unmatched pair falls through to None.
            pair.is_none()
                .then(|| CORRIDORS.iter().find(|c| c.chain_id == cfg.chain_id))
                .flatten()
        })
}

/// The catalog corridor shipping one specific pair on one chain.
///
/// The pair, not the config, is the key — so a bot whose *second* pool is the
/// catalog one can be identified too. [`identify_corridor`] answers "what preset
/// is this folder", which is a first-pool question; callers that reason per pool
/// (enrollment seats each pool separately) need this one instead, or a later
/// catalog pool is invisible to them.
pub fn identify_pair(chain_id: u64, collateral: &str, debt: &str) -> Option<&'static Corridor> {
    let want = (collateral.to_lowercase(), debt.to_lowercase());
    CORRIDORS.iter().find(|c| {
        c.chain_id == chain_id
            && corridor_pair(c).is_some_and(|have| want.0 == have.0 && want.1 == have.1)
    })
}

/// Who a pool's corridor is: the id Textile knows it by, plus how to name it.
///
/// Owned, because it comes from either the embedded catalog (`&'static`) or the
/// config file itself (runtime strings) and callers must not care which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorridorIdentity {
    pub id: String,
    pub display_name: String,
    pub network_label: String,
}

impl From<&Corridor> for CorridorIdentity {
    fn from(c: &Corridor) -> Self {
        Self {
            id: c.id.to_string(),
            display_name: c.display_name.to_string(),
            network_label: c.network_label.to_string(),
        }
    }
}

/// What one pool's corridor is — the identity the file carries, else whatever
/// the shipped catalog recognises the pair as.
///
/// The stamp wins because it is the only one that can be right for a corridor
/// listed after this release: the catalog cannot name a market it doesn't ship.
/// The catalog stays as the fallback for presets and hand-written configs,
/// which carry no stamp. A pool with neither is a custom corridor and has no
/// identity to report — that's `None`, and callers already render it from the
/// token addresses.
pub fn pool_identity(chain_id: u64, pool: &crate::config::PoolConfig) -> Option<CorridorIdentity> {
    stamped_identity(pool).or_else(|| {
        identify_pair(chain_id, &pool.collateral, &pool.debt).map(CorridorIdentity::from)
    })
}

/// The identity written into the pool, if it carries a usable one.
///
/// All three fields are required together. A file with an id but no names would
/// otherwise produce a corridor labeled with a raw database id, which is worse
/// in the UI than falling through to the address pair.
fn stamped_identity(pool: &crate::config::PoolConfig) -> Option<CorridorIdentity> {
    let field = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(CorridorIdentity {
        id: field(&pool.corridor_id)?,
        display_name: field(&pool.corridor_name)?,
        network_label: field(&pool.corridor_network)?,
    })
}

/// The bot's corridor identity: its first pool's.
///
/// The stamped counterpart to [`identify_corridor`], and what every caller that
/// asks "which corridor is this bot" should use. Same first-pool rule, same
/// `None` for a config that names no corridor we can identify.
pub fn config_identity(toml_str: &str) -> Option<CorridorIdentity> {
    let cfg = crate::config::Config::from_toml(toml_str).ok()?;
    pool_identity(cfg.chain_id, cfg.pools.first()?)
}

/// The chain and first pool's `(collateral, debt)` a `stitch.toml` quotes, all
/// lowercased. `None` when the file doesn't parse or has no pool to key on.
///
/// Used to line a remote corridor up against the shipped presets: two configs
/// describe the same market exactly when this matches.
pub fn pair_of_config(toml_str: &str) -> Option<(u64, String, String)> {
    let cfg = crate::config::Config::from_toml(toml_str).ok()?;
    let pool = cfg.pools.first()?;
    Some((
        cfg.chain_id,
        pool.collateral.to_lowercase(),
        pool.debt.to_lowercase(),
    ))
}

/// The first pool's `(collateral, debt)` token addresses, lowercased, parsed from
/// a corridor's own template. Returns `None` if the template can't be parsed (a
/// guarded invariant — see `every_template_parses_as_a_valid_config`).
fn corridor_pair(c: &Corridor) -> Option<(String, String)> {
    let cfg = crate::config::Config::from_toml(c.toml_template).ok()?;
    let pool = cfg.pools.first()?;
    Some((pool.collateral.to_lowercase(), pool.debt.to_lowercase()))
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

    /// A zero address parses fine (the config tests use one), so nothing stops
    /// a corridor shipping with a placeholder `reactor` — and a bot built from
    /// that preset would quote happily and never be fillable. `pending_deploy`
    /// is the only way to ship one, and this asserts the pairing BOTH ways:
    /// a live corridor can't carry a placeholder, and a pending one can't
    /// carry a real address. The second half is what makes the flag
    /// self-cleaning — fill the reactor without clearing it and this fails,
    /// so the placeholder state can't quietly become permanent.
    #[test]
    fn only_pending_corridors_carry_a_placeholder_reactor() {
        const ZERO: &str = "0x0000000000000000000000000000000000000000";
        for c in catalog() {
            let cfg = crate::config::Config::from_toml(c.toml_template).unwrap();
            assert_eq!(
                cfg.reactor.eq_ignore_ascii_case(ZERO),
                c.pending_deploy,
                "corridor {}: reactor is {} but pending_deploy is {}. A live corridor needs a \
                 real SETTLEMENT_V3_FILLER_REACTOR; a pending one must keep the zero placeholder \
                 until the deploy lands.",
                c.id,
                cfg.reactor,
                c.pending_deploy
            );
            // Permit2 is the same canonical address on every chain, so there is
            // never a reason for it to be a placeholder.
            assert!(
                !cfg.permit2.eq_ignore_ascii_case(ZERO),
                "corridor {} ships a zero permit2 address",
                c.id
            );
        }
    }

    /// The panel lists pending corridors but refuses to build a bot for one
    /// (see panel::http::wizard::create). At least one live corridor must
    /// always remain, or the Add Bot flow has nothing to offer.
    #[test]
    fn at_least_one_corridor_is_live() {
        assert!(catalog().iter().any(|c| !c.pending_deploy));
    }

    /// `deployable_catalog` is what the setup surfaces (CLI `init`, web wizard)
    /// offer, so it must never include a `pending_deploy` corridor and must never
    /// be empty — otherwise the picker would let an operator stand a bot up on a
    /// zero reactor, or have nothing to offer at all.
    #[test]
    fn deployable_catalog_excludes_pending_and_is_non_empty() {
        let deployable = deployable_catalog();
        assert!(
            !deployable.is_empty(),
            "deployable_catalog must offer at least one corridor"
        );
        assert!(
            deployable.iter().all(|c| !c.pending_deploy),
            "deployable_catalog must not include any pending_deploy corridor"
        );
        assert_eq!(
            deployable.len(),
            catalog().iter().filter(|c| !c.pending_deploy).count(),
            "deployable_catalog must include every live corridor"
        );
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
    fn corridors_sharing_a_chain_are_told_apart_by_collateral() {
        // Celo hosts cNGN/USDT, USDC/USDT, wARS/USDT and wBRL/USDT (chain 42220).
        // A chain-only match would collapse them; identify must key on the
        // collateral token so the control panel names (and preselects) the right one.
        let cngn = find_corridor("cngn-usdt-celo").expect("cngn corridor exists");
        let usdc = find_corridor("usdc-usdt-celo").expect("usdc corridor exists");
        let wars = find_corridor("wars-usdt-celo").expect("wars corridor exists");
        let wbrl = find_corridor("wbrl-usdt-celo").expect("wbrl corridor exists");
        assert_eq!(cngn.chain_id, usdc.chain_id, "test premise: same chain");
        assert_eq!(usdc.chain_id, wars.chain_id, "test premise: same chain");
        assert_eq!(wars.chain_id, wbrl.chain_id, "test premise: same chain");
        assert_eq!(identify_corridor(cngn.toml_template).unwrap().id, cngn.id);
        assert_eq!(identify_corridor(usdc.toml_template).unwrap().id, usdc.id);
        assert_eq!(identify_corridor(wars.toml_template).unwrap().id, wars.id);
        assert_eq!(identify_corridor(wbrl.toml_template).unwrap().id, wbrl.id);
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

    #[test]
    fn a_custom_pair_on_a_catalog_chain_is_not_mislabeled_as_a_preset() {
        // A custom corridor can land on a chain the catalog already covers (e.g.
        // some new Celo pair). Its pool's collateral matches no shipped corridor,
        // so identify must return None — the panel then shows "Custom corridor"
        // rather than the first catalog entry that shares the chain (which would
        // mislabel it and make Settings preselect an unrelated preset).
        let cngn = find_corridor("cngn-usdt-celo").expect("cngn corridor exists");
        // Same chain as the Celo presets, but a collateral token none of them use.
        let custom = cngn.toml_template.replace(
            "0xF6829D7393dAe24509eb1E52eE8e572e2E271a4f",
            "0x1111111111111111111111111111111111111111",
        );
        assert!(
            identify_corridor(&custom).is_none(),
            "an unshipped Celo pair must not be identified as a catalog corridor"
        );

        // Trickier: a custom pair that *reuses* a catalog collateral (cNGN) but
        // quotes it against a different debt token. Chain and collateral both
        // match the cNGN/USDT preset, so a collateral-only match would mislabel
        // it. Matching the full pair keeps it a custom corridor.
        let same_collateral_new_debt = cngn.toml_template.replace(
            "0x48065fbBE25f71C9282ddf5e1cD6D6A887483D5e", // USDT on Celo
            "0x2222222222222222222222222222222222222222",
        );
        assert!(
            identify_corridor(&same_collateral_new_debt).is_none(),
            "cNGN against a different debt must not be identified as cNGN/USDT"
        );

        // A pool-less config (nothing to key on) still gets the chain-only
        // fallback, so the historical behavior for those is preserved.
        let pool_less = "chain_id = 42220\n\
             rpc_url = \"https://forno.celo.org\"\n\
             indexer_url = \"https://api.textilecredit.com\"\n\
             permit2 = \"0x000000000022D473030F116dDEE9F6B43aC78BA3\"\n\
             reactor = \"0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3\"\n\
             tick_interval_secs = 5\n\
             pools = []\n\
             [feed]\n\
             url = \"https://api.textilecredit.com/price\"\n\
             staleness_secs = 900\n";
        assert_eq!(
            identify_corridor(pool_less).map(|c| c.chain_id),
            Some(42220),
            "a pool-less config still resolves to a chain-only match"
        );
    }

    /// A config for a pair the catalog will never ship, optionally carrying the
    /// corridor identity Textile's renderer stamps into it.
    fn novel_config(identity: &str) -> String {
        format!(
            r#"
chain_id = 42220
rpc_url = "https://forno.celo.org"
indexer_url = "https://api.textilecredit.com"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"
tick_interval_secs = 5

[feed]
url = "https://api.textilecredit.com/price?chainId=42220&pair=aaa-usdt"
staleness_secs = 900

[[pools]]
collateral = "0x1111111111111111111111111111111111111111"
collateral_decimals = 6
debt = "0x2222222222222222222222222222222222222222"
debt_decimals = 6
{identity}
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

    const STAMP: &str = r#"corridor_id = "cmnovelcorridor"
corridor_name = "AAA → USDT"
corridor_network = "Celo""#;

    #[test]
    fn a_corridor_the_catalog_never_shipped_still_names_itself() {
        // The reason the stamp exists: the catalog compiled into this binary
        // cannot identify a market listed after the release, so without it the
        // panel shows raw addresses and the access request names no corridor.
        let toml = novel_config(STAMP);
        assert!(
            identify_corridor(&toml).is_none(),
            "the catalog genuinely doesn't know this pair"
        );
        let identity = config_identity(&toml).expect("the file names itself");
        assert_eq!(identity.id, "cmnovelcorridor");
        assert_eq!(identity.display_name, "AAA → USDT");
        assert_eq!(identity.network_label, "Celo");
    }

    #[test]
    fn an_unstamped_config_still_falls_back_to_the_catalog() {
        // Shipped presets and hand-written configs carry no stamp, and must keep
        // resolving exactly as before.
        let preset = find_corridor("cngn-usdt-celo").unwrap();
        let identity = config_identity(preset.toml_template).expect("the preset resolves");
        assert_eq!(identity.id, "cngn-usdt-celo");
        assert_eq!(identity.display_name, preset.display_name);
        assert_eq!(identity.network_label, preset.network_label);
    }

    #[test]
    fn a_custom_corridor_has_no_identity_to_report() {
        // Neither stamped nor shipped: callers render it from the addresses.
        assert!(config_identity(&novel_config("")).is_none());
    }

    #[test]
    fn a_half_written_stamp_falls_through_rather_than_showing_a_raw_id() {
        // An id with no names would label the corridor with a database row id,
        // which reads worse than the address pair it would otherwise fall back
        // to. All three or none.
        let toml = novel_config(r#"corridor_id = "cmnovelcorridor""#);
        assert!(config_identity(&toml).is_none());

        let blank = novel_config(
            "corridor_id = \"cmnovelcorridor\"\ncorridor_name = \"  \"\ncorridor_network = \"Celo\"",
        );
        assert!(config_identity(&blank).is_none(), "a blank name is no name");
    }

    #[test]
    fn a_stamp_wins_over_the_catalog_for_the_same_pair() {
        // Textile renamed a corridor, or reused a pair under a new row. The file
        // is the more recent truth; the catalog is only ever the fallback.
        let preset = find_corridor("cngn-usdt-celo").unwrap();
        let stamp = concat!(
            "collateral_decimals = 6\n",
            "corridor_id = \"cmrowid\"\n",
            "corridor_name = \"cNGN => USDT\"\n",
            "corridor_network = \"Celo\"",
        );
        let stamped = preset
            .toml_template
            .replace("collateral_decimals = 6", stamp);
        let identity = config_identity(&stamped).expect("resolves");
        assert_eq!(identity.id, "cmrowid");
        assert_eq!(identity.display_name, "cNGN => USDT");
    }

    #[test]
    fn each_pool_carries_its_own_identity() {
        // A multi-pool bot must not stamp pool 0's corridor onto the others —
        // that is the same mistake `identify_pair` exists to avoid.
        let cfg = crate::config::Config::from_toml(&novel_config(STAMP)).unwrap();
        assert_eq!(
            pool_identity(cfg.chain_id, &cfg.pools[0]).map(|c| c.id),
            Some("cmnovelcorridor".to_string())
        );
        let unstamped = crate::config::Config::from_toml(&novel_config("")).unwrap();
        assert!(pool_identity(unstamped.chain_id, &unstamped.pools[0]).is_none());
    }
}
