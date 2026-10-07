// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Adapter between pure module decisions and RFQ's sole order/reservation owner.
use super::*;
use crate::modules::runtime::QuoteState;
use crate::modules::{self as policy, dealer, Context, Mode};
use crate::protocol::types::OrderParams;
use tokio::sync::oneshot;

/// Signing has no execution authority. Only the engine that owns the current
/// reservation ledger may release the result; disconnecting drops this receiver.
pub(super) struct PendingRebalance {
    request: dealer::QuoteRequest,
    quote: dealer::Quote,
    epoch: u64,
    order: OrderParams,
    signature: oneshot::Receiver<Option<[u8; 65]>>,
}

/// Module balances for a chain without Multicall3. Where Multicall3 exists the
/// inventory loop reads them in its own batch instead (see `inventory_loop`).
/// Here they would be two more sequential calls in front of every quote
/// refresh, and module-only reads must not delay those, including in shadow
/// mode. One process-scoped loop bounds concurrency even when its RPC hangs.
pub(super) async fn balance_loop(
    rpc: Rpc,
    vault: Address,
    modules: crate::modules::runtime::Runtime,
) {
    let calls = [
        Call::new(vault, encode_free_settlement()),
        Call::new(vault, encode_free_corridor()),
    ];
    let mut batcher = None;
    loop {
        if batcher.is_none() {
            batcher = Batcher::detect(&rpc).await.ok();
        }
        let reader = batcher.unwrap_or_else(Batcher::sequential);
        if let Ok(values) = reader.read(&rpc, &calls).await {
            if let (Some(Some(s)), Some(Some(c))) = (values.first(), values.get(1)) {
                if let Ok(mut slot) = modules.balances.write() {
                    *slot = Some(crate::modules::runtime::Balances {
                        settlement: decode_uint(s),
                        corridor: decode_uint(c),
                        at: unix_now(),
                    });
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(INVENTORY_REFRESH_SECS)).await;
    }
}

impl Engine {
    pub(super) fn module_context(
        &self,
        book: &CorridorBook,
        quote: &Quote,
        now: u64,
    ) -> Option<Context> {
        let modules = self.modules.as_ref()?;
        let balances = (*modules.balances.read().ok()?)?;
        let limits = (*self.vault_policy.read().ok()?)?;
        // Modules use the corridor as the risk currency, settlement as numeraire.
        // A reversed pool must never invert the exposure limit silently.
        if book.collateral != limits.corridor || book.debt != limits.settlement {
            return None;
        }
        let available = self.inventory.view(now);
        let (base_buy_bps, base_sell_bps) = policy::base_spreads(book, quote.price);
        Some(Context {
            now,
            price: quote.price,
            price_at: quote.timestamp,
            balances_at: balances.at,
            staleness_secs: book.staleness_secs,
            settlement: balances.settlement,
            corridor: balances.corridor,
            available_settlement: available.funded(book.debt)?,
            available_corridor: available.funded(book.collateral)?,
            reserved_settlement: self.reserved_on(book, true, now),
            reserved_corridor: self.reserved_on(book, false, now),
            settlement_decimals: book.debt_decimals,
            corridor_decimals: book.collateral_decimals,
            max_sell: limits.max_input_corridor,
            base_buy_bps,
            base_sell_bps,
        })
    }
    pub(super) fn module_book(
        &self,
        book: &CorridorBook,
        quote: &Quote,
        now: u64,
    ) -> Option<CorridorBook> {
        let Some(modules) = &self.modules else {
            return Some(book.clone());
        };
        let changes_quotes = modules.config.mode == Mode::Live
            && (modules.config.inventory.enabled || modules.config.spreads.enabled);
        let Some(context) = self.module_context(book, quote, now) else {
            modules.quote_status(
                QuoteState::WaitingForVault,
                "Waiting for vault balances, quote inventory and a matching token pair",
                now,
            );
            modules.status("Waiting for fresh vault balances and matching corridor orientation");
            // Observation and rebalancing must not gate otherwise unchanged quotes.
            return (!changes_quotes).then(|| book.clone());
        };
        modules.quote_status(QuoteState::Evaluating, "Evaluating the quote policy", now);
        let decision = modules.decide(&context);
        Some(if changes_quotes {
            policy::apply(book, &decision)
        } else {
            book.clone()
        })
    }
    /// Only starts bounded background work. Network and custody signing never
    /// block the maker websocket or its subsecond reply budget.
    pub(super) fn module_tick(&mut self, prices: &PriceCache, now: u64) {
        let Some(modules) = self.modules.clone() else {
            return;
        };
        if modules.config.mode != Mode::Live || !modules.config.rebalance.enabled {
            return;
        }
        if let Some(mut pending) = self.pending_rebalance.take() {
            match pending.signature.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.pending_rebalance = Some(pending);
                }
                Ok(Some(signature)) => self.submit_rebalance(pending, signature, prices, now),
                _ => modules.status("Rebalance signature failed or expired; reservation retained"),
            }
            return;
        }
        let Some(book) = self.books.first().cloned() else {
            return;
        };
        let Some(quote) = prices.get(&book.feed_url) else {
            return;
        };
        let Some(ctx) = self.module_context(&book, &quote, now) else {
            return;
        };
        let decision = modules.decide(&ctx);
        if decision.blocked || decision.rebalance_sell.is_zero() {
            // A pending dealer response cannot outlive the conditions that requested it.
            modules.take_quote();
            return;
        }
        let Some(dealer_config) = modules.config.rebalance.dealer.clone() else {
            modules.status("Spot sale indicated; configure a dealer to execute");
            return;
        };
        let Some(vault) = self.vault else {
            return;
        };
        let Some(limits) = self.vault_policy.read().ok().and_then(|p| *p) else {
            return;
        };
        let min_output =
            |amount| minimum_output(&ctx, amount, modules.config.rebalance.max_slippage_bps);
        if let Some((request, dealer_quote)) = modules.take_quote() {
            if dealer::validate_quote(&dealer_config, &request, &dealer_quote, now).is_err()
                || dealer_quote.sell_amount > decision.rebalance_sell
                || dealer_quote.buy_amount < min_output(dealer_quote.sell_amount)
            {
                modules.status("Dealer quote no longer fits current exposure or price");
                return;
            }
            let epoch = self.trading_epoch.read().map(|p| *p).unwrap_or(0);
            if epoch == 0 {
                return;
            }
            let order = build_order(&RfqOrderSpec {
                reactor: self.reactor,
                maker: vault,
                nonce: trading_nonce(
                    epoch,
                    vault_nonce_low(self.nonce_salt, unix_now_ms(), self.counter),
                ),
                deadline_secs: dealer_quote.expires_at,
                input_token: book.collateral,
                input_amount: dealer_quote.sell_amount,
                output_token: book.debt,
                output_amount: dealer_quote.buy_amount,
                validation_contract: self.validation_contract,
                taker: dealer_config.taker.parse().expect("validated dealer taker"),
                order_executor: self.vault_order_executor,
            });
            self.counter += 1;
            self.reservations.reserve_paying(
                &request.request_id,
                &book.slug,
                false,
                dealer_quote.sell_amount,
                dealer_quote.expires_at,
                Some(book.collateral.to_string()),
            );
            if let Err(e) = self.reservations.flush() {
                error!(error = %e, "rebalance refused: reservation persistence failed");
                modules.status("Rebalance blocked: reservation storage unavailable");
                return;
            }
            let signer = self.signer.clone();
            let permit2 = self.permit2;
            let chain_id = self.chain_id;
            let payload = permit2_payload(&order, permit2, chain_id);
            let expires_at = dealer_quote.expires_at;
            let (sender, signature) = oneshot::channel();
            self.pending_rebalance = Some(PendingRebalance {
                request,
                quote: dealer_quote,
                epoch,
                order,
                signature,
            });
            tokio::spawn(async move {
                let remaining = expires_at.saturating_sub(unix_now()).saturating_sub(3);
                let signature = tokio::time::timeout(
                    std::time::Duration::from_secs(remaining),
                    signer.sign_typed(&payload),
                )
                .await
                .ok()
                .and_then(Result::ok);
                let _ = sender.send(signature);
            });
        } else {
            let deadline = now.saturating_add(
                modules
                    .config
                    .rebalance
                    .order_lifetime_secs
                    .min(limits.max_lifetime_secs),
            );
            let minimum = min_output(decision.rebalance_sell);
            if minimum.is_zero() {
                return;
            }
            let request = dealer::QuoteRequest {
                request_id: format!(
                    "module-rebalance:{:x}:{}:{}",
                    self.nonce_salt, now, self.counter
                ),
                chain_id: self.chain_id,
                vault: vault.to_string(),
                sell_token: book.collateral.to_string(),
                buy_token: book.debt.to_string(),
                sell_amount: decision.rebalance_sell,
                min_buy_amount: minimum,
                deadline,
            };
            if let Err(e) = modules.begin_quote(request) {
                error!(error = %e, "rebalance pacing could not be persisted");
            }
        }
    }

    /// Check the signature against the engine's current ledger before handing
    /// it to network I/O. No await can admit another RFQ between these checks.
    fn submit_rebalance(
        &self,
        pending: PendingRebalance,
        signature: [u8; 65],
        prices: &PriceCache,
        now: u64,
    ) {
        let Some(modules) = self.modules.clone() else {
            return;
        };
        let Some(dealer_config) = modules.config.rebalance.dealer.clone() else {
            return;
        };
        let current = self.books.iter().find_map(|book| {
            if book.collateral != pending.order.input_token
                || book.debt != pending.order.output_token
            {
                return None;
            }
            self.module_context(book, &prices.get(&book.feed_url)?, now)
        });
        let Some(current) = current else {
            modules.status("Rebalance conditions changed while signing; order withheld");
            return;
        };
        if !self
            .reservations
            .is_only_live_claim(&pending.request.request_id, now)
        {
            modules.status("Reservations changed while signing; rebalance order withheld");
            return;
        }
        // The context reads today's ledger. Exclude only this order's already
        // persisted claim when recomputing its desired size, never a snapshot.
        let Some(reserved_corridor) = current
            .reserved_corridor
            .checked_sub(pending.quote.sell_amount)
        else {
            modules.status("Rebalance reservation missing; order withheld");
            return;
        };
        let current = Context {
            reserved_corridor,
            ..current
        };
        let decision = modules.decide(&current);
        let epoch = self.trading_epoch.read().map(|e| *e).unwrap_or(0);
        if now.saturating_add(2) >= pending.quote.expires_at
            || epoch != pending.epoch
            || self.vault != Some(pending.order.swapper)
            || decision.blocked
            || decision.rebalance_sell < pending.quote.sell_amount
            || pending.quote.buy_amount
                < minimum_output(
                    &current,
                    pending.quote.sell_amount,
                    modules.config.rebalance.max_slippage_bps,
                )
        {
            modules.status("Rebalance conditions changed while signing; order withheld");
            return;
        }
        let chain_id = self.chain_id;
        tokio::spawn(async move {
            let submit = dealer::Execute {
                request_id: &pending.request.request_id,
                chain_id,
                encoded_order: alloy_primitives::hex::encode_prefixed(encode_order_bytes(
                    &pending.order,
                )),
                strategy_signature: alloy_primitives::hex::encode_prefixed(signature),
            };
            // No retry and no early release: even a timeout may have delivered
            // an executable signature. The normal ledger survives restarts.
            match dealer::execute(&dealer_config, &submit).await {
                Ok(()) => modules.status(
                    "Sent to dealer; settlement unconfirmed. Inventory reserved through expiry.",
                ),
                Err(_) => modules
                    .status("Dealer submission uncertain; inventory reserved through expiry."),
            }
        });
    }
}
fn minimum_output(ctx: &Context, amount: U256, slippage_bps: u32) -> U256 {
    let fair = math::debt_for_collateral(
        ctx.price,
        amount,
        ctx.settlement_decimals,
        ctx.corridor_decimals,
    );
    // Round the allowed loss down, so proceeds never fall below the configured floor.
    fair.saturating_sub(policy::fraction(fair, slippage_bps))
}
