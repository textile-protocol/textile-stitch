// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Local operator instructions. The venue carries their ID, never their authority.
use super::{config::RebalanceMethod, *};
use alloy_primitives::Address;
use anyhow::{ensure, Result};
use std::path::{Path, PathBuf};

pub mod address {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &Address, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:#x}"))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Address, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
pub const PREFIX: &str = "module-rebalance:manual:";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sale {
    pub id: String,
    pub chain_id: u64,
    #[serde(with = "address")]
    pub vault: Address,
    #[serde(with = "address")]
    pub taker: Address,
    #[serde(with = "address")]
    pub corridor_token: Address,
    #[serde(with = "address")]
    pub settlement_token: Address,
    #[serde(with = "atomic")]
    pub corridor_amount: U256,
    #[serde(with = "atomic")]
    pub min_settlement: U256,
    /// Unix seconds. Closing never revokes an already-issued signature.
    pub expires_at: u64,
    #[serde(default)]
    pub closed: bool,
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn request_id(rfq: &str) -> Option<&str> {
    let (id, attempt) = rfq.strip_prefix(PREFIX)?.split_once(':')?;
    (valid_id(id) && !attempt.is_empty()).then_some(id)
}
fn path(dir: &Path, id: &str, suffix: &str) -> Result<PathBuf> {
    ensure!(valid_id(id), "Invalid manual sale ID");
    Ok(dir.join(format!("manual-sale-{id}{suffix}.json")))
}
pub fn read(dir: &Path, id: &str) -> Result<Sale> {
    let sale: Sale = serde_json::from_str(&std::fs::read_to_string(path(dir, id, "")?)?)?;
    ensure!(sale.id == id, "Manual sale ID mismatch");
    Ok(sale)
}
pub fn write(dir: &Path, sale: &Sale) -> Result<()> {
    crate::setup::write_toml_atomic(&path(dir, &sale.id, "")?, &serde_json::to_string(sale)?)
}
impl Sale {
    pub fn validate(&self, now: u64) -> Result<()> {
        ensure!(valid_id(&self.id), "Invalid manual sale ID");
        ensure!(
            !self.closed && self.expires_at > now,
            "Sale is closed or expired"
        );
        ensure!(
            self.expires_at <= now + 86_400,
            "Sale link may last at most 24 hours"
        );
        ensure!(
            !self.vault.is_zero() && !self.taker.is_zero() && self.vault != self.taker,
            "Choose a buyer wallet different from the vault"
        );
        ensure!(
            !self.corridor_amount.is_zero() && !self.min_settlement.is_zero(),
            "Amounts must be positive"
        );
        ensure!(
            !self.corridor_token.is_zero()
                && !self.settlement_token.is_zero()
                && self.corridor_token != self.settlement_token,
            "Invalid sale pair"
        );
        Ok(())
    }
    pub fn allows(
        &self,
        cfg: &ModulesConfig,
        ctx: &Context,
        input: U256,
        output: U256,
    ) -> Result<()> {
        self.validate(ctx.now)?;
        ensure!(
            cfg.mode == Mode::Live
                && cfg.rebalance.enabled
                && cfg.rebalance.method == RebalanceMethod::Manual,
            "Manual sales require enabled live spot rebalancing"
        );
        // Share the live module's freshness and NAV gates, without requiring a
        // volatility warmup merely to compute the manual sale limit.
        let mut policy = cfg.clone();
        policy.spreads.enabled = false;
        let decision = evaluate(&policy, ctx, &[]);
        ensure!(
            !decision.blocked,
            "Fresh vault balances and reference price required"
        );
        ensure!(
            ctx.reserved_corridor.is_zero() && ctx.reserved_settlement.is_zero(),
            "Outstanding quotes must settle or expire first"
        );
        let value = math::debt_for_collateral(
            ctx.price,
            ctx.corridor,
            ctx.settlement_decimals,
            ctx.corridor_decimals,
        );
        let nav = ctx
            .settlement
            .checked_add(value)
            .ok_or_else(|| anyhow::anyhow!("NAV overflow"))?;
        let budget = value
            .saturating_sub(fraction(nav, cfg.inventory.target_bps))
            .min(fraction(nav, cfg.rebalance.max_trade_bps));
        let cap = math::collateral_for_debt(
            ctx.price,
            budget,
            ctx.settlement_decimals,
            ctx.corridor_decimals,
        )
        .min(ctx.available_corridor)
        .min(ctx.max_sell);
        ensure!(
            input == self.corridor_amount && input <= cap,
            "Sale exceeds available excess inventory or the per-sale limit"
        );
        let fair = math::debt_for_collateral(
            ctx.price,
            input,
            ctx.settlement_decimals,
            ctx.corridor_decimals,
        );
        let floor = fair.saturating_sub(fraction(fair, cfg.rebalance.max_slippage_bps));
        ensure!(
            output >= self.min_settlement && output >= floor && !output.is_zero(),
            "Net vault proceeds are below the sale minimum or current price protection"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct Nonce {
    epoch: u64,
    #[serde(with = "atomic")]
    value: U256,
}
/// Persist once BEFORE signing. Every refreshed quote consumes the same Permit2
/// nonce, including after restart. A new trading epoch requires a new request.
pub fn nonce(dir: &Path, id: &str, epoch: u64) -> Result<U256> {
    ensure!(epoch != 0, "Vault epoch unavailable");
    let file = path(dir, id, "-nonce")?;
    if file.exists() {
        let n: Nonce = serde_json::from_str(&std::fs::read_to_string(file)?)?;
        ensure!(
            n.epoch == epoch,
            "Vault epoch changed; create a new request"
        );
        return Ok(n.value);
    }
    let value = crate::protocol::vault::trading_nonce(epoch, rand::random());
    crate::setup::write_toml_atomic(&file, &serde_json::to_string(&Nonce { epoch, value })?)?;
    Ok(value)
}
#[derive(Serialize, Deserialize)]
struct Pace {
    id: String,
    next_at: u64,
}
/// A retry of one sale keeps its nonce; a different sale must respect cooldown.
pub fn pace(dir: &Path, id: &str, now: u64, cooldown: u64) -> Result<()> {
    let file = dir.join("manual-sale-pacing.json");
    if file.exists() {
        let p: Pace = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
        ensure!(p.id == id || now >= p.next_at, "Wait for the sale cooldown");
    }
    crate::setup::write_toml_atomic(
        &file,
        &serde_json::to_string(&Pace {
            id: id.into(),
            next_at: now + cooldown,
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Sale, ModulesConfig, Context) {
        let sale = Sale {
            id: "a".repeat(64),
            chain_id: 56,
            vault: Address::repeat_byte(1),
            taker: Address::repeat_byte(2),
            corridor_token: Address::repeat_byte(3),
            settlement_token: Address::repeat_byte(4),
            corridor_amount: U256::from(20),
            min_settlement: U256::from(20),
            expires_at: 2000,
            closed: false,
        };
        let mut cfg = ModulesConfig::default();
        cfg.mode = Mode::Live;
        cfg.rebalance.enabled = true;
        let ctx = Context {
            now: 1000,
            price: 1.0,
            price_at: 1000,
            balances_at: 1000,
            staleness_secs: 10,
            settlement: U256::from(400),
            corridor: U256::from(600),
            available_settlement: U256::from(400),
            available_corridor: U256::from(600),
            reserved_settlement: U256::ZERO,
            reserved_corridor: U256::ZERO,
            settlement_decimals: 6,
            corridor_decimals: 6,
            max_sell: U256::from(100),
            base_buy_bps: Some(20),
            base_sell_bps: Some(20),
        };
        (sale, cfg, ctx)
    }
    #[test]
    fn manual_sales_are_default_even_with_a_dealer_configured() {
        let cfg: ModulesConfig = toml::from_str("[rebalance]\nenabled = true\n[rebalance.dealer]\nurl = 'https://dealer.example'\ntaker = '0x1111111111111111111111111111111111111111'").unwrap();
        assert_eq!(cfg.rebalance.method, RebalanceMethod::Manual);
    }
    #[test]
    fn amount_floor_excess_and_freshness_are_enforced() {
        let (sale, cfg, ctx) = fixture();
        assert!(sale
            .allows(&cfg, &ctx, U256::from(20), U256::from(20))
            .is_ok());
        assert!(sale
            .allows(&cfg, &ctx, U256::from(19), U256::from(20))
            .is_err());
        assert!(sale
            .allows(&cfg, &ctx, U256::from(20), U256::from(19))
            .is_err());
        for change in [
            Context {
                balances_at: 995,
                ..ctx
            },
            Context {
                price_at: 900,
                ..ctx
            },
            Context {
                max_sell: U256::from(19),
                ..ctx
            },
            Context {
                available_corridor: U256::from(19),
                ..ctx
            },
            Context {
                reserved_settlement: U256::from(1),
                ..ctx
            },
            Context {
                reserved_corridor: U256::from(1),
                ..ctx
            },
            Context {
                settlement: U256::from(900),
                corridor: U256::from(100),
                ..ctx
            },
        ] {
            assert!(sale
                .allows(&cfg, &change, U256::from(20), U256::from(20))
                .is_err());
        }
        let too_big = Sale {
            corridor_amount: U256::from(21),
            ..sale.clone()
        };
        assert!(too_big
            .allows(&cfg, &ctx, U256::from(21), U256::from(21))
            .is_err());
        let cheap = Sale {
            min_settlement: U256::from(1),
            ..sale.clone()
        };
        assert!(cheap
            .allows(&cfg, &ctx, U256::from(20), U256::from(1))
            .is_err());
        let mut preview = cfg.clone();
        preview.mode = Mode::Shadow;
        assert!(sale
            .allows(&preview, &ctx, U256::from(20), U256::from(20))
            .is_err());
    }
    #[test]
    fn restart_and_refresh_keep_nonce_epoch_change_refuses() {
        let dir = std::env::temp_dir().join(format!("manual-sale-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sale, _, _) = fixture();
        write(&dir, &sale).unwrap();
        assert_eq!(read(&dir, &sale.id).unwrap(), sale);
        let n = nonce(&dir, &sale.id, 3).unwrap();
        assert_eq!(nonce(&dir, &sale.id, 3).unwrap(), n);
        assert!(nonce(&dir, &sale.id, 4).is_err());
        assert!(nonce(&dir, "../outside", 3).is_err());
        pace(&dir, &sale.id, 1000, 300).unwrap();
        assert!(pace(&dir, &"b".repeat(64), 1100, 300).is_err());
        assert!(pace(&dir, &sale.id, 1100, 300).is_ok());
        assert!(pace(&dir, &"b".repeat(64), 1400, 300).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn closed_expired_and_vault_as_buyer_are_rejected() {
        let (sale, _, _) = fixture();
        assert!(Sale {
            closed: true,
            ..sale.clone()
        }
        .validate(1000)
        .is_err());
        assert!(sale.validate(2000).is_err());
        assert!(Sale {
            taker: sale.vault,
            ..sale.clone()
        }
        .validate(1000)
        .is_err());
        assert_eq!(
            request_id(&format!("{PREFIX}{}:rfq_1", sale.id)),
            Some(sale.id.as_str())
        );
        assert!(request_id("module-rebalance:manual:../outside:1").is_none());
    }
}
