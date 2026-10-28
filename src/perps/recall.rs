// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cached, separately reserved GMX collateral. This never inflates the ordinary
//! wallet balance and never performs network I/O on the quote critical path.
use super::PerpsConfig;
use crate::rfq::{reserve::Reservations, responder::InventoryView, wire::RecallOffer};
use alloy_primitives::{Address, U256};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub const SNAPSHOT: &str = "perps-recall-snapshot.json";
const HOLDS: &str = "perps-recall-reservations.json";
pub struct Runtime {
    config: PerpsConfig,
    dir: PathBuf,
    holds: Reservations,
}
#[derive(Debug)]
pub struct Capacity {
    pub atoms: U256,
    pub executor: Address,
    pub max_recall_price: U256,
}
impl Runtime {
    pub fn load(config: &PerpsConfig, dir: &Path, now: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(config.enabled && config.recall_enabled, "Recall disabled");
        Ok(Self {
            config: config.clone(),
            dir: dir.to_owned(),
            holds: Reservations::load(dir.join(HOLDS), now)?,
        })
    }
    pub fn capacity(
        &self,
        chain: u64,
        maker: Address,
        token: Address,
        payment: Address,
        now_ms: u64,
    ) -> Option<Capacity> {
        if !self.holds.is_empty() {
            return None;
        }
        let bytes = std::fs::read(self.dir.join(SNAPSHOT)).ok()?;
        if bytes.len() > 2_000_000 {
            return None;
        }
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        let received = v["receivedAt"].as_u64()?;
        if received > now_ms || now_ms - received > 5000 {
            return None;
        }
        let r = &v["data"];
        if r["control"]["revision"].as_str()? != self.config.revision
            || r["effectiveMode"] != "live"
            || r["recall"]["enabled"] != true
            || r["assignment"]["id"].as_str()? != self.config.assignment_id
            || r["assignment"]["botId"].as_str()? != self.config.bot_id
        {
            return None;
        }
        let m = &r["recall"]["manifest"];
        let p = &r["recall"]["position"];
        let address = |v: &Value| v.as_str()?.parse::<Address>().ok();
        if m["version"] != 2
            || m["chainId"].as_u64()? != chain
            || address(&m["assignment"]["fundingWallet"])? != maker
            || address(&m["assets"]["collateral"]["address"])? != token
            || address(&m["assets"]["payment"]["address"])? != payment
            || !p["pauseReason"].is_null()
            || m["limits"].is_null()
            || r["recall"]["reservedCollateralAtoms"].as_str()? != "0"
        {
            return None;
        }
        let observed = crate::rfq::iso8601::parse_iso_ms(p["observedAt"].as_str()?)?;
        if observed > now_ms || now_ms - observed > 5000 {
            return None;
        }
        // An expired quote may still be waiting for GMX cancellation.
        if r["recall"]["operations"]
            .as_array()?
            .iter()
            .any(|o| o["releasedAt"].is_null())
        {
            return None;
        }
        let atoms = p["recallableAtoms"]
            .as_str()?
            .parse::<U256>()
            .ok()?
            .min(m["limits"]["quoteAtoms"].as_str()?.parse::<U256>().ok()?);
        let max_recall_price = p["recallMaxPrice30"].as_str()?.parse::<U256>().ok()?;
        if atoms.is_zero() || max_recall_price.is_zero() {
            return None;
        }
        Some(Capacity {
            atoms,
            max_recall_price,
            executor: address(&m["contracts"]["executor"]["address"])?,
        })
    }
    pub fn offer(
        &self,
        o: &RecallOffer,
        chain: u64,
        maker: Address,
        token: Address,
        payment: Address,
        now: u64,
    ) -> Option<Capacity> {
        let c = self.capacity(chain, maker, token, payment, now)?;
        let created = o.created_at.checked_mul(1000)?;
        let accept = created.checked_add(60_000)?;
        let settle = created.checked_add(90_000)?;
        if o.route != "gmx-recall-v1"
            || o.assignment_id != self.config.assignment_id
            || o.executor.parse::<Address>().ok()? != c.executor
            || o.max_recall_price.parse::<U256>().ok()? != c.max_recall_price
            || created > now
            || crate::rfq::iso8601::parse_iso_ms(&o.accept_by)? != accept
            || crate::rfq::iso8601::parse_iso_ms(&o.settle_by)? != settle
            || now >= accept
        {
            return None;
        }
        Some(Capacity {
            atoms: c.atoms.min(o.capacity_atoms.parse().ok()?),
            ..c
        })
    }
    pub fn reserve(
        &mut self,
        id: &str,
        corridor: &str,
        token: Address,
        atoms: U256,
        deadline: u64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.holds.is_empty(), "Recall already promised");
        self.holds.reserve_paying(
            id.to_owned(),
            corridor.to_owned(),
            false,
            atoms,
            deadline,
            Some(format!("{token:#x}")),
        );
        self.holds.flush()
    }
    pub fn has_reservation(&self, id: &str) -> bool {
        self.holds.corridor(id).is_some()
    }
    pub fn mark_exposed(&mut self, id: &str) {
        self.holds.mark_exposed(id);
    }
    /// Only an unselected reply can be released on a confirmed venue loss.
    /// Loaded holds remain exposed because selection may have happened offline.
    pub fn quote_result(&mut self, id: &str, result: &str) -> bool {
        match result {
            "selected" => {
                self.mark_exposed(id);
                false
            }
            "no_quote" | "lost_price" | "invalid" | "late" => self.holds.release_unexposed(id),
            _ => false,
        }
    }
    pub fn prune(&mut self, now: u64) {
        self.holds.prune(now);
    }
}
pub fn inventory(token: Address, atoms: U256) -> InventoryView {
    InventoryView::new(HashMap::from([(token, atoms)]))
}

pub fn cache(dir: &Path, data: Value, now: u64) -> anyhow::Result<()> {
    use std::io::Write;
    let temp = dir.join(format!("{SNAPSHOT}.{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(&serde_json::to_vec(
        &serde_json::json!({ "receivedAt": now, "data": data }),
    )?)?;
    file.sync_all()?;
    std::fs::rename(temp, dir.join(SNAPSHOT))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use serde_json::json;
    const NOW: u64 = 1_792_713_600_000;
    const MAKER: Address = address!("1111111111111111111111111111111111111111");
    const TOKEN: Address = address!("2222222222222222222222222222222222222222");
    const PAYMENT: Address = address!("3333333333333333333333333333333333333333");
    fn fixture() -> (Runtime, Value) {
        let dir = std::env::temp_dir().join(format!(
            "recall-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let config = PerpsConfig {
            enabled: true,
            recall_enabled: true,
            assignment_id: "celo-idrx".into(),
            bot_id: "stitch-celo-2".into(),
            revision: "one".into(),
            ..Default::default()
        };
        let data = json!({ "control": {"revision": "one"}, "effectiveMode": "live",
            "assignment": {"id": "celo-idrx", "botId": "stitch-celo-2"}, "recall": {"enabled": true,
            "reservedCollateralAtoms": "0", "operations": [], "position": {"pauseReason": null,
            "observedAt": "2026-10-23T00:00:00.000Z", "recallableAtoms": "1000", "recallMaxPrice30": "500500000000000000000000"},
            "manifest": {"version": 2, "chainId": 42220, "assignment": {"fundingWallet": format!("{MAKER:#x}")},
            "assets": {"collateral": {"address": format!("{TOKEN:#x}")}, "payment": {"address": format!("{PAYMENT:#x}")}},
            "limits": {"quoteAtoms": "800"}, "contracts": {"executor": {"address": format!("{PAYMENT:#x}")}}} }});
        cache(&dir, data.clone(), NOW).unwrap();
        (Runtime::load(&config, &dir, NOW / 1000).unwrap(), data)
    }
    fn capacity(r: &Runtime) -> Option<Capacity> {
        r.capacity(42220, MAKER, TOKEN, PAYMENT, NOW)
    }
    #[test]
    fn recall_snapshot_requires_freshness_identity_and_limits() {
        let (r, data) = fixture();
        assert_eq!(capacity(&r).unwrap().atoms, U256::from(800));
        assert!(r
            .capacity(42220, MAKER, TOKEN, PAYMENT, NOW + 5001)
            .is_none());
        assert!(r.capacity(1, MAKER, TOKEN, PAYMENT, NOW).is_none());
        for (pointer, value) in [
            ("/control/revision", json!("old")),
            ("/recall/manifest/limits", Value::Null),
            ("/recall/position/pauseReason", json!("accounting")),
            ("/recall/position/recallMaxPrice30", json!("0")),
            ("/recall/position/recallMaxPrice30", Value::Null),
            ("/recall/reservedCollateralAtoms", json!("1")),
            (
                "/recall/operations",
                json!([{"releasedAt": null, "state": "unsuccessful"}]),
            ),
        ] {
            let mut changed = data.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            cache(&r.dir, changed, NOW).unwrap();
            assert!(capacity(&r).is_none(), "{pointer}");
        }
        std::fs::remove_dir_all(r.dir).unwrap();
    }
    #[test]
    fn recall_reservations_survive_restart_and_wait_through_signed_skew() {
        let (mut r, _) = fixture();
        r.reserve("rfq-one", "idrx", TOKEN, U256::from(400), NOW / 1000 + 90)
            .unwrap();
        assert!(capacity(&r).is_none());
        assert!(r
            .reserve("rfq-two", "idrx", TOKEN, U256::from(1), NOW / 1000 + 90)
            .is_err());
        let mut restarted = Runtime::load(&r.config, &r.dir, NOW / 1000 + 91).unwrap();
        assert!(capacity(&restarted).is_none());
        restarted.prune(NOW / 1000 + 119);
        assert!(capacity(&restarted).is_none());
        restarted.prune(NOW / 1000 + 121);
        assert!(capacity(&restarted).is_some());
        std::fs::write(r.dir.join(HOLDS), b"{partial").unwrap();
        assert!(Runtime::load(&r.config, &r.dir, NOW / 1000).is_err());
        std::fs::remove_dir_all(r.dir).unwrap();
    }
    #[test]
    fn confirmed_recall_losers_release_but_exposed_and_restarted_holds_stay() {
        for result in ["no_quote", "lost_price", "invalid", "late"] {
            let (mut r, _) = fixture();
            let deadline = NOW / 1000 + 90;
            r.reserve("loser", "idrx", TOKEN, U256::from(400), deadline)
                .unwrap();
            assert!(r.quote_result("loser", result));
            assert!(!r.quote_result("loser", result));
            assert!(capacity(&r).is_some());
            let reloaded = Runtime::load(&r.config, &r.dir, NOW / 1000).unwrap();
            assert!(!reloaded.has_reservation("loser"));

            r.reserve("winner", "idrx", TOKEN, U256::from(400), deadline)
                .unwrap();
            r.quote_result("winner", "selected");
            assert!(!r.quote_result("winner", result));
            assert!(capacity(&r).is_none());
            std::fs::remove_dir_all(r.dir).unwrap();

            let (mut r, _) = fixture();
            r.reserve("unknown", "idrx", TOKEN, U256::from(400), deadline)
                .unwrap();
            let mut reloaded = Runtime::load(&r.config, &r.dir, NOW / 1000).unwrap();
            assert!(!reloaded.quote_result("unknown", result));
            assert!(capacity(&reloaded).is_none());
            r.mark_exposed("unknown");
            assert!(!r.quote_result("unknown", result));
            std::fs::remove_dir_all(r.dir).unwrap();
        }
    }
    #[test]
    fn recall_offer_never_extends_or_overflows_deadlines() {
        let (r, _) = fixture();
        let mut offer: RecallOffer = serde_json::from_value(json!({"route":"gmx-recall-v1", "assignmentId":"celo-idrx",
            "executor": format!("{PAYMENT:#x}"), "createdAt": NOW/1000, "acceptBy":"2026-10-23T00:01:00.000Z",
            "settleBy":"2026-10-23T00:01:30.000Z", "capacityAtoms":"400", "maxRecallPrice":"500500000000000000000000", "revision":0})).unwrap();
        assert_eq!(
            r.offer(&offer, 42220, MAKER, TOKEN, PAYMENT, NOW)
                .unwrap()
                .atoms,
            U256::from(400)
        );
        let bound = offer.max_recall_price.clone();
        offer.max_recall_price = "500500000000000000000001".into();
        assert!(r.offer(&offer, 42220, MAKER, TOKEN, PAYMENT, NOW).is_none());
        offer.max_recall_price = bound;
        offer.created_at = u64::MAX;
        assert!(r.offer(&offer, 42220, MAKER, TOKEN, PAYMENT, NOW).is_none());
        offer.created_at = NOW / 1000;
        offer.settle_by = "2026-10-23T00:01:31.000Z".into();
        assert!(r.offer(&offer, 42220, MAKER, TOKEN, PAYMENT, NOW).is_none());
        std::fs::remove_dir_all(r.dir).unwrap();
    }
}
