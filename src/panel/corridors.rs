// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The corridor list the panel serves, and where it comes from.
//!
//! Textile's API knows which corridors are listed; this binary only knows which
//! ones existed when it was built. So the panel asks the API
//! ([`crate::setup::remote`]) and falls back to the embedded catalog when it
//! can't — an operator on a plane, behind a firewall, or mid-outage still gets a
//! wizard that works.
//!
//! Two rules make the merge safe:
//!
//! 1. **A shipped preset wins on a market we ship.** The presets are tuned per
//!    pair — cNGN rests at 1 bp with `rfq_staleness_secs = 240` because its feed
//!    is a cron sample, WETH at 5 bps because it isn't. The API renders one
//!    conservative profile for everything, so taking its file for cNGN/USDT
//!    would quietly widen a corridor we've already tuned and let it go dark
//!    between samples. The API decides *which* corridors are offered; the preset
//!    decides *how* one we know is quoted.
//! 2. **Presets the API doesn't list stay on the end.** Testnet corridors aren't
//!    in the production table and would otherwise vanish from the picker.
//!
//! Everything is keyed on the market — chain plus the two token addresses — not
//! on ids, because the two sources name corridors differently (`cngn-usdt-celo`
//! vs a database row id).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::setup::{self, CorridorEntry};

/// How long a good answer is reused. The list changes when someone registers a
/// corridor — minutes-stale is fine, and it keeps a wizard reload from being a
/// round trip to the API.
const FRESH_FOR: Duration = Duration::from_secs(5 * 60);

/// How long a failure is remembered. Without this every corridor list, create,
/// switch and add-pool would pay the full connect timeout again while the API is
/// down, which is exactly when the panel should feel fastest.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Where a list came from, so the wizard can say so when it's the offline one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Textile's corridor registry answered.
    Api,
    /// It didn't, so this is the list compiled into the binary.
    Embedded,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Api => "api",
            Source::Embedded => "embedded",
        }
    }
}

/// A corridor list plus where it came from.
pub struct Listing {
    pub corridors: Vec<CorridorEntry>,
    pub source: Source,
    /// Why the API list is missing, in operator words. `None` when it isn't.
    pub warning: Option<String>,
}

struct Cached {
    corridors: Vec<CorridorEntry>,
    warning: Option<String>,
    /// Reuse this answer until then — for a success or a failure alike.
    until: Instant,
}

impl Cached {
    fn source(&self) -> Source {
        if self.warning.is_some() {
            Source::Embedded
        } else {
            Source::Api
        }
    }
}

/// The panel's corridor catalog: the API's list, cached, over the embedded one.
pub struct CorridorCatalog {
    /// API origin to ask. `None` disables the fetch entirely — set that way in
    /// tests, and by an operator who'd rather the panel never call home.
    api_base: Option<String>,
    cache: RwLock<Option<Cached>>,
}

impl CorridorCatalog {
    pub fn new(api_base: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            api_base,
            cache: RwLock::new(None),
        })
    }

    /// The corridors to offer right now.
    pub async fn list(&self) -> Listing {
        let Some(base) = self.api_base.as_deref() else {
            return Listing {
                corridors: embedded(),
                source: Source::Embedded,
                warning: None,
            };
        };

        if let Some(cached) = self.cached().await {
            return cached;
        }

        let (corridors, warning) = match setup::fetch_corridors(base).await {
            Ok(remote) if !remote.is_empty() => (merge(remote), None),
            // An empty list is almost certainly a misconfigured origin rather
            // than "Textile lists nothing", and an empty picker is a dead end.
            // Treat it like a failure and keep the presets.
            Ok(_) => (
                embedded(),
                Some(format!(
                    "{base} listed no corridors, so this is Stitch's built-in list."
                )),
            ),
            Err(e) => (
                embedded(),
                Some(format!(
                    "Couldn't reach Textile for the current corridor list ({e:#}), so this is \
                     Stitch's built-in one. Newly listed corridors may be missing."
                )),
            ),
        };

        let failed = warning.is_some();
        let until = Instant::now() + if failed { RETRY_AFTER } else { FRESH_FOR };
        *self.cache.write().await = Some(Cached {
            corridors: corridors.clone(),
            warning: warning.clone(),
            until,
        });
        Listing {
            corridors,
            source: if failed {
                Source::Embedded
            } else {
                Source::Api
            },
            warning,
        }
    }

    /// One corridor by id, from whichever list is current.
    ///
    /// The embedded catalog is checked first and unconditionally: a bot created
    /// from a preset carries that preset's id, and switch / add-pool have to
    /// keep resolving it even while the API is unreachable.
    pub async fn find(&self, id: &str) -> Option<CorridorEntry> {
        if let Some(preset) = setup::find_corridor(id) {
            return Some(CorridorEntry::from(preset));
        }
        self.list().await.corridors.into_iter().find(|c| c.id == id)
    }

    async fn cached(&self) -> Option<Listing> {
        let guard = self.cache.read().await;
        let cached = guard.as_ref()?;
        if cached.until <= Instant::now() {
            return None;
        }
        Some(Listing {
            corridors: cached.corridors.clone(),
            source: cached.source(),
            warning: cached.warning.clone(),
        })
    }
}

fn embedded() -> Vec<CorridorEntry> {
    setup::catalog().iter().map(CorridorEntry::from).collect()
}

/// The API's list, with each entry swapped for the shipped preset that quotes
/// the same market, then any presets the API didn't mention.
///
/// See the module docs for why the preset wins the overlap.
fn merge(remote: Vec<CorridorEntry>) -> Vec<CorridorEntry> {
    let mut used: Vec<&'static str> = Vec::new();
    let listed: Vec<CorridorEntry> = remote
        .into_iter()
        .map(|entry| match preset_for(&entry) {
            Some(preset) => {
                used.push(preset.id);
                // Keep the preset's config and its id — the id is what an
                // already-created bot is labeled with, and what enrollment
                // seats a corridor by, so a preset market must keep naming
                // itself the same way whether or not the API answered.
                CorridorEntry::from(preset)
            }
            None => entry,
        })
        .collect();

    let leftovers = setup::catalog()
        .iter()
        .filter(|c| !used.contains(&c.id))
        .map(CorridorEntry::from);

    listed.into_iter().chain(leftovers).collect()
}

/// The shipped preset quoting the same market as this corridor, if we ship one.
fn preset_for(entry: &CorridorEntry) -> Option<&'static setup::Corridor> {
    let (chain_id, collateral, debt) = setup::pair_of_config(&entry.toml_template)?;
    setup::identify_pair(chain_id, &collateral, &debt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config for a market the catalog definitely doesn't ship (made-up tokens
    /// on Celo), so `preset_for` can't match it.
    fn novel_template() -> String {
        r#"
chain_id = 42220
rpc_url = "https://rpc.example.com"
indexer_url = "https://api.example.com"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"
tick_interval_secs = 5

[feed]
url = "https://api.example.com/price?pair=aaa-bbb"
staleness_secs = 900

[[pools]]
collateral = "0x1111111111111111111111111111111111111111"
collateral_decimals = 6
debt = "0x2222222222222222222222222222222222222222"
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
        .to_string()
    }

    /// The catalog's Celo cNGN/USDT preset, as the API would render it: same
    /// market, different id, and the API's conservative spread profile.
    fn api_rendering_of_a_preset_market() -> CorridorEntry {
        let preset = setup::find_corridor("cngn-usdt-celo").unwrap();
        let (chain_id, collateral, debt) = setup::pair_of_config(preset.toml_template).unwrap();
        CorridorEntry {
            id: "cmrowid".to_string(),
            display_name: "cNGN → USDT".to_string(),
            network_label: "Celo".to_string(),
            chain_id,
            toml_template: format!(
                r#"
chain_id = {chain_id}
rpc_url = "https://forno.celo.org"
indexer_url = "https://api.textilecredit.com"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0xa9AA0a64769cBed4d3B1Ceb4Df01CdE915C235b3"
tick_interval_secs = 5

[feed]
url = "https://api.textilecredit.com/price?chainId={chain_id}&pair=cngn-usdt"
staleness_secs = 900

[[pools]]
collateral = "{collateral}"
collateral_decimals = 6
debt = "{debt}"
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
            ),
            pending_deploy: false,
        }
    }

    #[test]
    fn a_market_we_ship_keeps_its_tuned_preset() {
        // The API renders one conservative profile for every pair. Celo cNGN/USDT
        // is tuned to 1 bp with a stretched RFQ staleness because its feed is a
        // cron sample — taking the API's file would widen the spread and let the
        // corridor go dark between samples.
        let merged = merge(vec![api_rendering_of_a_preset_market()]);
        let cngn = merged
            .iter()
            .find(|c| c.chain_id == 42220 && c.toml_template.contains("cngn-usdt"))
            .expect("the corridor is still offered");
        assert_eq!(cngn.id, "cngn-usdt-celo", "it keeps the preset's id");
        let cfg = crate::config::Config::from_toml(&cngn.toml_template).unwrap();
        assert_eq!(cfg.pools[0].buy_offset_bps, Some(1), "the tuned spread");
    }

    #[test]
    fn a_market_we_do_not_ship_comes_through_as_the_api_rendered_it() {
        let entry = CorridorEntry {
            id: "cmnovel".to_string(),
            display_name: "AAA → BBB".to_string(),
            network_label: "Celo".to_string(),
            chain_id: 42220,
            toml_template: novel_template(),
            pending_deploy: false,
        };
        let merged = merge(vec![entry.clone()]);
        assert_eq!(merged[0], entry, "the API's entry, untouched");
    }

    #[test]
    fn presets_the_api_does_not_list_are_still_offered() {
        // The production table has no testnet corridor, and dropping it would
        // take the only pair an operator can rehearse on off the picker.
        let merged = merge(vec![api_rendering_of_a_preset_market()]);
        assert!(
            merged.iter().any(|c| c.id == "cngn-usdt-bsc-testnet"),
            "the testnet preset survives: {:?}",
            merged.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_merged_list_never_repeats_a_market() {
        let merged = merge(vec![api_rendering_of_a_preset_market()]);
        let mut ids: Vec<_> = merged.iter().map(|c| c.id.clone()).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "ids are unique");
        assert_eq!(
            merged.iter().filter(|c| c.id == "cngn-usdt-celo").count(),
            1,
            "the overlapping market appears once"
        );
    }

    #[tokio::test]
    async fn a_disabled_fetch_serves_the_embedded_catalog() {
        let catalog = CorridorCatalog::new(None);
        let listing = catalog.list().await;
        assert_eq!(listing.source, Source::Embedded);
        assert!(listing.warning.is_none(), "not a failure — a choice");
        assert_eq!(listing.corridors.len(), setup::catalog().len());
    }

    #[tokio::test]
    async fn an_unreachable_api_falls_back_and_says_so() {
        let catalog = CorridorCatalog::new(Some("http://127.0.0.1:1".to_string()));
        let listing = catalog.list().await;
        assert_eq!(listing.source, Source::Embedded);
        assert!(listing.warning.is_some(), "the operator is told");
        assert!(!listing.corridors.is_empty(), "the wizard still works");
    }

    #[tokio::test]
    async fn a_preset_id_resolves_even_when_the_api_is_down() {
        // Switch and add-pool send ids the panel handed out earlier. Those must
        // not stop resolving because Textile is unreachable.
        let catalog = CorridorCatalog::new(Some("http://127.0.0.1:1".to_string()));
        let found = catalog
            .find("cngn-usdt-celo")
            .await
            .expect("preset resolves");
        assert_eq!(found.display_name, "cNGN / USDT");
        assert!(catalog.find("no-such-corridor").await.is_none());
    }
}
