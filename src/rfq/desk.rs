// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Desk feeds: pricing an OTC desk's wallets apart from everyone else.
//!
//! A desk (`[[rfq.desk_feeds]]`) trades with this bot's wallet through a
//! virtual account the API keeps on the desk's feed. A request whose taker is
//! one of the desk's wallets, on the book the desk trades, is handled here:
//!
//! - **Caps.** The bot never pays the desk more than its account holds on
//!   that side, less what the bot has already promised it: signed quotes
//!   still live, and quotes that filled in the last [`FILL_HOLD_SECS`]
//!   (the account trails a fill by up to a minute). The debt leg is also
//!   held to the feed's `maxOrderDebt` and the TOML `max_order_debt`. A
//!   request over the room is quoted for what fits, the way inventory sizes
//!   a quote down, so the venue can fill the rest from other makers. Only a
//!   side with no room left is refused, with `taker_limit`.
//! - **Price.** While the desk's feed is live and its midpoint sits within
//!   `max_deviation_bps` of the pool's own feed price, the bot builds a flat
//!   plan at the desk's bid and ask, next to the public plan, and sends
//!   whichever is better for the taker. Better than public is never worse
//!   than the published levels, so the venue can't strike it.
//! - **Feed problems.** Unreachable, unreadable, or a 200 without a desk: the
//!   last good desk stands for [`DESK_GRACE_SECS`], then the desk's wallets
//!   get `Busy` until it's back. Only a 404 makes the taker an ordinary
//!   customer.
//!
//! Every taker the desk feeds don't name is untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use alloy_primitives::{Address, U256, U512};
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::config::DeskFeed;
use crate::pricing::desk::{DeskInfo, DeskReading, DeskStatus};
use crate::pricing::feed::Quote;
use crate::pricing::tick::usable;

use super::reserve::RELEASE_SKEW_SECS;
use super::responder::{CorridorBook, QuotePlan};

/// How long the last good desk stands while its feed can't be read.
pub const DESK_GRACE_SECS: u64 = 300;

/// How long after the bot sees a desk quote fill a desk reading has to be
/// before it shows that fill: the API books a fill onto the account up to a
/// minute after it lands, and two minutes covers that with room. Until a
/// reading that new arrives the claim keeps counting, however long the feed
/// stays down; past [`DESK_GRACE_SECS`] the desk is refused anyway.
pub const FILL_HOLD_SECS: u64 = 120;

/// Sibling of `rfq-reservations.json`.
pub const DESK_CLAIMS_FILE: &str = "rfq-desk-claims.json";

/// One desk feed as the responder runs it.
#[derive(Debug, Clone)]
pub struct DeskFeedRuntime {
    pub spec: DeskFeed,
    /// The RFQ freshness window for the desk's print
    /// ([`crate::config::desk_feed_staleness_secs`]).
    pub staleness_secs: u64,
}

/// The latest reading of each desk feed and when it was taken, shared between
/// the poll loop and the quote path.
#[derive(Debug, Clone, Default)]
pub struct DeskCache(Arc<RwLock<HashMap<String, (DeskReading, u64)>>>);

impl DeskCache {
    /// Record an answer. Returns the reading it replaced.
    pub fn record(&self, url: &str, reading: DeskReading, at: u64) -> Option<DeskReading> {
        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        map.insert(url.to_string(), (reading, at))
            .map(|(before, _)| before)
    }

    pub fn get(&self, url: &str) -> Option<(DeskReading, u64)> {
        let map = self.0.read().unwrap_or_else(|e| e.into_inner());
        map.get(url).cloned()
    }
}

/// Everything the engine holds for desk feeds. Empty when none are set.
#[derive(Debug, Clone, Default)]
pub struct Desks {
    pub feeds: Vec<DeskFeedRuntime>,
    pub cache: DeskCache,
    /// Process-scoped like the poll loops, so a reconnect can't forget a
    /// promise made in the last session.
    pub claims: Arc<Mutex<DeskClaims>>,
}

impl Desks {
    pub fn claims(&self) -> std::sync::MutexGuard<'_, DeskClaims> {
        self.claims.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a request's taker is to the desk feeds.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// Not a desk's wallet on this book: quote as usual.
    Customer,
    /// A desk's wallet whose desk can't be read: refuse.
    Busy(&'static str),
    /// A desk's wallet on the desk's book. `read_at` is when `info` was
    /// read: the account in it shows the fills seen before then.
    Desk {
        feed: usize,
        info: DeskInfo,
        read_at: u64,
    },
}

/// Which desk `taker` trades through on `book`, judged on what each feed last
/// said. A taker must be in the TOML `takers` and in the feed's own
/// `desk.takers`, and the book must be the desk's pair on this chain (by token
/// address), or it's an ordinary customer.
///
/// A desk's wallet with no reading yet, or none in [`DESK_GRACE_SECS`], is
/// refused whatever book it asks on: the bot can't tell which pair the desk
/// trades without a reading, and guessing open would pay it uncapped.
pub fn gate(
    feeds: &[DeskFeedRuntime],
    cache: &DeskCache,
    taker: Address,
    chain_id: u64,
    book: &CorridorBook,
    now: u64,
) -> Gate {
    let Some(feed) = feeds.iter().position(|f| f.spec.takers.contains(&taker)) else {
        return Gate::Customer;
    };
    let Some((reading, at)) = cache.get(&feeds[feed].spec.feed_url) else {
        return Gate::Busy("desk feed not read yet");
    };
    if now.saturating_sub(at) > DESK_GRACE_SECS {
        return Gate::Busy("desk feed unreadable for longer than the grace period");
    }
    match reading {
        DeskReading::Gone => Gate::Customer,
        DeskReading::Shared(info) => {
            let ours = info.takers.contains(&taker)
                && info.chain_id == chain_id
                && info.collateral == book.collateral
                && info.debt == book.debt;
            if ours {
                Gate::Desk {
                    feed,
                    info,
                    read_at: at,
                }
            } else {
                Gate::Customer
            }
        }
    }
}

/// Why the desk's own price can't be quoted right now, or `Ok` when it can.
/// `quote` is the desk feed's cached print; `pool_price` the matching book's
/// own feed price, which the desk midpoint must sit within `max_deviation_bps`
/// of. The midpoint is the middle of the desk's bid and ask.
pub fn desk_price_refusal(
    info: &DeskInfo,
    quote: Option<&Quote>,
    pool_price: f64,
    feed: &DeskFeedRuntime,
    now: u64,
) -> Result<(), &'static str> {
    if info.status != DeskStatus::Live {
        return Err("desk status is not live");
    }
    let quote = quote.ok_or("no desk print")?;
    usable(quote, now, feed.staleness_secs).map_err(|_| "desk print is stale or degraded")?;
    let (bid, ask) = quote.sides().ok_or("desk print has no bid and ask")?;
    let mid = (bid + ask) / 2.0;
    let deviation_bps = (mid / pool_price - 1.0).abs() * 10_000.0;
    if !deviation_bps.is_finite() || deviation_bps > f64::from(feed.spec.max_deviation_bps) {
        return Err("desk midpoint is too far from the pool's feed price");
    }
    Ok(())
}

/// The book a desk plan prices from: the matching pool's book with its sides,
/// tokens and capacity, quoting the desk feed's own bid and ask
/// ([`super::responder::feed_sides_book`] does the spreads), flat. No curve,
/// off-hours widening or issuer spread: the desk sets its own price.
pub fn desk_book(book: &CorridorBook, feed: &DeskFeedRuntime) -> CorridorBook {
    CorridorBook {
        feed_url: feed.spec.feed_url.clone(),
        staleness_secs: feed.staleness_secs,
        curve: None,
        feed_sides: true,
        off_hours: None,
        issuer_spread: None,
        ..book.clone()
    }
}

/// Whether `desk` gives the taker strictly more than `public` for what it
/// pays. Both answer the same request, so the taker pays `sell_amount` and
/// receives `buy_amount` on each: compare `buy / sell` cross-multiplied, in
/// 512 bits so no product overflows. A tie keeps the public plan.
pub fn taker_prefers(desk: &QuotePlan, public: &QuotePlan) -> bool {
    let wide = |v: U256| U512::from(v);
    wide(desk.buy_amount) * wide(public.sell_amount)
        > wide(public.buy_amount) * wide(desk.sell_amount)
}

/// Which cap a plan breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapRefusal {
    /// The side's account, less what's in flight, is smaller than the payout.
    Account,
    /// The debt leg is over `maxOrderDebt` or the TOML `max_order_debt`.
    MaxOrderDebt,
}

/// Whether `plan` fits the desk's caps. On a bid the bot pays debt, on an
/// ask collateral; `held` is what's already in flight on that side
/// ([`DeskClaims::held`]). The debt leg is what the bot pays on a bid and what
/// it receives in the signed order on an ask.
pub fn cap_refusal(
    plan: &QuotePlan,
    info: &DeskInfo,
    held: U256,
    toml_max: Option<U256>,
) -> Option<CapRefusal> {
    let account = if plan.bid {
        info.account_debt
    } else {
        info.account_collateral
    };
    if plan.input.saturating_add(held) > account {
        return Some(CapRefusal::Account);
    }
    let debt_leg = if plan.bid { plan.input } else { plan.output };
    let max = toml_max.map_or(info.max_order_debt, |m| m.min(info.max_order_debt));
    (debt_leg > max).then_some(CapRefusal::MaxOrderDebt)
}

/// What the bot may still pay the desk on one side, in one quote: the
/// payout ceiling (the side's account less what's in flight) and the debt-leg
/// ceiling (the lower of `maxOrderDebt` and the TOML `max_order_debt`). Both
/// atomic. The two ceilings [`cap_refusal`] checks, as numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskRoom {
    pub bid: bool,
    /// Most the bot may pay: debt on a bid, collateral on an ask.
    pub payout: U256,
    /// Most the debt leg may be: what the bot pays on a bid, what it
    /// receives on an ask.
    pub debt_leg: U256,
}

/// [`DeskRoom`] for one side. A side whose account is already used up (in
/// flight past it, or a negative account read as zero) has no payout room.
pub fn room(info: &DeskInfo, bid: bool, held: U256, toml_max: Option<U256>) -> DeskRoom {
    let account = if bid {
        info.account_debt
    } else {
        info.account_collateral
    };
    DeskRoom {
        bid,
        payout: account.saturating_sub(held),
        debt_leg: toml_max.map_or(info.max_order_debt, |m| m.min(info.max_order_debt)),
    }
}

/// `plan` sized down to fit `room`, at the plan's own price, or `None` when
/// nothing fits (no room, or what fits is below the smallest feeable
/// output). A plan that already fits comes back unchanged.
///
/// Rounding favours the bot: the input (what it pays) is floored to the
/// room, and the output (what it receives) is the plan's output scaled by the
/// same share, rounded up, so the clamped quote is never priced better for
/// the taker than the plan. Rounding up can't push the ask's debt leg over
/// its ceiling: the input is floored so that `output * input' <= ceiling *
/// input`. The taker's gross is rebuilt from the new output, as the
/// inventory clamp in `quote_at_price` does.
pub fn fit_to_room(plan: &QuotePlan, room: &DeskRoom, fee_bps: u32) -> Option<QuotePlan> {
    debug_assert_eq!(plan.bid, room.bid);
    if plan.input.is_zero() || plan.output.is_zero() {
        return None;
    }
    let by_debt_leg = if plan.bid {
        room.debt_leg
    } else {
        // input' with output * input' / input <= debt_leg.
        let wide = U512::from(room.debt_leg) * U512::from(plan.input) / U512::from(plan.output);
        wide.saturating_to::<U256>()
    };
    let input = plan.input.min(room.payout).min(by_debt_leg);
    if input == plan.input {
        return Some(plan.clone());
    }
    if input.is_zero() {
        return None;
    }
    let scaled = U512::from(plan.output) * U512::from(input);
    let divisor = U512::from(plan.input);
    let output = (scaled + divisor - U512::from(1u8)) / divisor;
    // At most `plan.output` (input' < input), so it fits 256 bits.
    let output = output.to::<U256>();
    let fee = super::math::fee_on(output, fee_bps);
    let gross = super::math::gross_of(output, fee_bps)?;
    if output < super::math::min_feeable_output(fee_bps) || fee.is_zero() {
        return None;
    }
    Some(QuotePlan {
        input,
        output,
        fee,
        sell_amount: gross,
        buy_amount: input,
        ..plan.clone()
    })
}

/// One quote promised to a desk's wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeskClaim {
    /// The desk it's against, by [`desk_key`], not by URL spelling.
    feed: String,
    /// True: the bot pays debt. False: it pays collateral.
    bid: bool,
    /// What the bot pays if it fills, atomic.
    paid: U256,
    /// Unix seconds after which an unfilled claim no longer counts: the order
    /// deadline plus [`RELEASE_SKEW_SECS`], as for a reservation.
    release_at: u64,
    /// Selected or window-closed: a stale losing result can't release it.
    exposed: bool,
    /// Set when the bot saw the fill: the claim counts until then, whatever
    /// its deadline, so the account has time to show it.
    filled_until: Option<u64>,
}

impl DeskClaim {
    /// When a desk reading shows everything this claim could have paid.
    /// Filled: [`FILL_HOLD_SECS`] after the bot saw the fill. Unfilled: the
    /// same after its order's deadline plus skew, since it may have filled
    /// at the deadline without the bot seeing it (a recall fill is tracked
    /// in the recall ledger, not here).
    fn hold_until(&self) -> u64 {
        self.filled_until
            .unwrap_or_else(|| self.release_at.saturating_add(FILL_HOLD_SECS))
    }

    /// Whether the claim still has to be added to an account read at
    /// `read_at`. Judged on the reading, not the clock: an account read
    /// before the fill could show doesn't show it, however old the reading
    /// gets. An unfilled claim whose order is still live always counts, since
    /// no reading is newer than now.
    fn counts_against(&self, read_at: u64) -> bool {
        read_at < self.hold_until()
    }

    /// Whether to keep the claim at `now`. Past its hold plus the grace
    /// period, either a reading newer than the hold has arrived or the desk
    /// is refused for an old reading ([`gate`]), so it can't count again.
    fn kept_at(&self, now: u64) -> bool {
        now < self.hold_until().saturating_add(DESK_GRACE_SECS)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredClaims {
    version: u32,
    entries: Vec<StoredClaim>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredClaim {
    rfq_id: String,
    feed: String,
    bid: bool,
    /// Decimal string: a U256 doesn't fit a JSON number.
    paid: String,
    release_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filled_until: Option<u64>,
}

/// The desk in-flight ledger: every quote signed for a desk's wallet, public
/// priced or desk priced, until it can no longer be paid or the account has
/// had time to show it. Persisted next to the reservations, so a restart
/// doesn't forget a promise. Owned by the responder; no locking of its own.
/// Which desk a claim is against: [`crate::config::desk_feed_identity`], the
/// endpoint and `otc` key. The ledger outlives a restart and a config edit,
/// so it keys on the desk itself, never on how its URL happens to be spelled:
/// a claim saved under `?otc=k&pair=x` still counts after the operator adds
/// `&offHours=1`. Every way in (reserve, held, load) goes through this.
fn desk_key(feed: &str) -> String {
    crate::config::desk_feed_identity(feed)
}

#[derive(Debug, Default)]
pub struct DeskClaims {
    by_rfq: HashMap<String, DeskClaim>,
    persist_path: Option<PathBuf>,
    /// The last write failed: what's on disk is behind memory, and a restart
    /// would reload claims that count for less than they should. Desk quotes
    /// stop until a write succeeds ([`Self::prune`] retries every tick).
    unsaved: bool,
}

impl DeskClaims {
    /// Memory-only, for tests: the responder always loads one from disk.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the ledger, or start empty at `path` when there is none. An
    /// unreadable file is an error: starting empty would forget promises.
    /// Loaded claims count as exposed.
    pub fn load(path: impl Into<PathBuf>, now: u64) -> anyhow::Result<Self> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                persist_path: Some(path),
                ..Self::default()
            });
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stored: StoredClaims =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            stored.version == 1,
            "unsupported {DESK_CLAIMS_FILE} version {}",
            stored.version
        );
        let by_rfq = stored
            .entries
            .into_iter()
            .map(|e| {
                let paid = U256::from_str_radix(&e.paid, 10)
                    .with_context(|| format!("invalid desk claim amount {}", e.paid))?;
                let claim = DeskClaim {
                    // Rows saved under any spelling of the desk's URL count
                    // against the same desk after a restart.
                    feed: desk_key(&e.feed),
                    bid: e.bid,
                    paid,
                    release_at: e.release_at,
                    exposed: true,
                    filled_until: e.filled_until,
                };
                Ok((e.rfq_id, claim))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?
            .into_iter()
            .filter(|(_, c)| c.kept_at(now))
            .collect();
        Ok(Self {
            by_rfq,
            persist_path: Some(path),
            unsaved: false,
        })
    }

    /// Record a quote signed for a desk's wallet, on disk before it goes out.
    /// An error means the claim isn't kept and the quote must be withheld: a
    /// restart would otherwise forget it and let the desk past its account.
    pub fn reserve(
        &mut self,
        rfq_id: &str,
        feed: &str,
        bid: bool,
        paid: U256,
        deadline: u64,
    ) -> anyhow::Result<()> {
        self.by_rfq.insert(
            rfq_id.to_string(),
            DeskClaim {
                feed: desk_key(feed),
                bid,
                paid,
                release_at: deadline.saturating_add(RELEASE_SKEW_SECS),
                exposed: false,
                filled_until: None,
            },
        );
        let written = match &self.persist_path {
            Some(path) => self.write(path),
            None => Ok(()),
        };
        match &written {
            Ok(()) => self.unsaved = false,
            Err(_) => {
                self.by_rfq.remove(rfq_id);
            }
        }
        written
    }

    /// Whether desk quotes have to stop: the ledger on disk is behind.
    pub fn unsaved(&self) -> bool {
        self.unsaved
    }

    /// What's in flight to `feed`'s desk on one side, on top of an account
    /// read at `read_at`.
    pub fn held(&self, feed: &str, bid: bool, read_at: u64) -> U256 {
        let desk = desk_key(feed);
        self.by_rfq
            .values()
            .filter(|c| c.feed == desk && c.bid == bid && c.counts_against(read_at))
            .fold(U256::ZERO, |sum, c| sum.saturating_add(c.paid))
    }

    pub fn mark_exposed(&mut self, rfq_id: &str) {
        if let Some(claim) = self.by_rfq.get_mut(rfq_id) {
            claim.exposed = true;
        }
    }

    /// Drop a never-exposed loser, as [`super::reserve::Reservations::release_unexposed`]
    /// does: its taker-bound order can't be filled.
    pub fn release_unexposed(&mut self, rfq_id: &str) -> bool {
        let gone = self
            .by_rfq
            .get(rfq_id)
            .is_some_and(|c| !c.exposed && c.filled_until.is_none())
            && self.by_rfq.remove(rfq_id).is_some();
        if gone {
            self.persist();
        }
        gone
    }

    /// The bot saw these quotes fill at `now`: each counts until a desk
    /// reading [`FILL_HOLD_SECS`] newer than that. Ids that aren't desk
    /// claims are ignored.
    pub fn mark_filled(&mut self, rfq_ids: &[String], now: u64) {
        let hold = now.saturating_add(FILL_HOLD_SECS);
        let mut changed = false;
        for id in rfq_ids {
            if let Some(claim) = self.by_rfq.get_mut(id) {
                if claim.filled_until.is_none() {
                    claim.filled_until = Some(hold);
                    changed = true;
                }
            }
        }
        if changed {
            self.persist();
        }
    }

    /// Forget claims that can't count again, and retry a failed write.
    pub fn prune(&mut self, now: u64) {
        let before = self.by_rfq.len();
        self.by_rfq.retain(|_, c| c.kept_at(now));
        if self.by_rfq.len() != before || self.unsaved {
            self.persist();
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_rfq.is_empty()
    }

    /// For changes after a quote went out (filled, released, pruned). A
    /// failed write marks the ledger [`Self::unsaved`], which stops desk
    /// quotes until a write goes through: a restart would otherwise reload,
    /// say, a fill without its hold.
    fn persist(&mut self) {
        let Some(path) = self.persist_path.clone() else {
            return;
        };
        match self.write(&path) {
            Ok(()) => self.unsaved = false,
            Err(e) => {
                if !self.unsaved {
                    tracing::error!(
                        error = %format!("{e:#}"),
                        path = %path.display(),
                        "desk claims not saved; desk quotes stop until they are"
                    );
                }
                self.unsaved = true;
            }
        }
    }

    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let stored = StoredClaims {
            version: 1,
            entries: self
                .by_rfq
                .iter()
                .map(|(id, c)| StoredClaim {
                    rfq_id: id.clone(),
                    feed: c.feed.clone(),
                    bid: c.bid,
                    paid: c.paid.to_string(),
                    release_at: c.release_at,
                    filled_until: c.filled_until,
                })
                .collect(),
        };
        // The reservations ledger's writer: a temp file renamed over the old
        // one, retried on Windows, where a plain rename won't overwrite.
        crate::setup::write_toml_atomic(path, &serde_json::to_string(&stored)?)
    }
}

impl super::Engine {
    /// [`gate`] for this request's taker on `book`.
    pub(super) fn desk_gate(&self, taker: Address, book: &CorridorBook, now: u64) -> Gate {
        let gated = gate(
            &self.desks.feeds,
            &self.desks.cache,
            taker,
            self.chain_id,
            book,
            now,
        );
        match gated {
            Gate::Desk { .. } if self.desks.claims().unsaved() => {
                Gate::Busy("desk claims can't be saved")
            }
            other => other,
        }
    }

    /// The flat desk plan for `req`, the book it was priced on and the desk
    /// print, or why there is none. `pool_quote` is the matching book's own
    /// print. The plan answers to the same wallet as the public one, so it
    /// takes the same reservations and inventory.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn desk_plan(
        &self,
        feed: usize,
        info: &DeskInfo,
        book: &CorridorBook,
        req: &super::wire::QuoteRequestFrame,
        pool_quote: &Quote,
        prices: &super::PriceCache,
        now: u64,
    ) -> Result<(QuotePlan, CorridorBook, Quote), &'static str> {
        let feed = &self.desks.feeds[feed];
        let desk_quote = prices.get(&feed.spec.feed_url);
        desk_price_refusal(info, desk_quote.as_ref(), pool_quote.price, feed, now)?;
        let desk_quote = desk_quote.ok_or("no desk print")?;
        let unsided = desk_book(book, feed);
        let sided = super::responder::feed_sides_book(&unsided, &desk_quote)
            .ok_or("desk print has no bid and ask")?;
        let plan = super::responder::decide_quote(
            &sided,
            req,
            desk_quote.price,
            self.reservations.reserved(&book.slug, true, now),
            self.reservations.reserved(&book.slug, false, now),
            self.reserved_on(book, true, now),
            self.reserved_on(book, false, now),
            &self.inventory.view(now),
        )
        .map_err(|_| "the desk plan does not fit the wallet or the pool's size")?;
        Ok((plan, unsided, desk_quote))
    }

    /// [`room`] on one side, against what's in flight on top of the account
    /// in `info`, read at `read_at`.
    pub(super) fn desk_room(
        &self,
        feed: usize,
        info: &DeskInfo,
        bid: bool,
        read_at: u64,
    ) -> DeskRoom {
        let feed = &self.desks.feeds[feed];
        let held = self.desks.claims().held(&feed.spec.feed_url, bid, read_at);
        room(info, bid, held, feed.spec.max_order_debt)
    }

    /// [`cap_refusal`] against what's in flight on top of the account in
    /// `info`, read at `read_at`.
    pub(super) fn desk_cap_refusal(
        &self,
        feed: usize,
        info: &DeskInfo,
        plan: &QuotePlan,
        read_at: u64,
    ) -> Option<CapRefusal> {
        let feed = &self.desks.feeds[feed];
        let held = self
            .desks
            .claims()
            .held(&feed.spec.feed_url, plan.bid, read_at);
        cap_refusal(plan, info, held, feed.spec.max_order_debt)
    }

    /// Whether a signed quote to a desk's wallet may still go out. A remote
    /// signer can take seconds, and in that time the desk feed can move.
    ///
    /// - A desk-priced order gets the recheck a `feed_sides` order gets
    ///   ([`super::guard::recheck_after_signing`]) against the desk feed's
    ///   current print: still fresh, and its side not moved against the bot.
    ///   Then everything that let the desk price be used at all
    ///   ([`desk_price_refusal`]) must still hold on the current prints: the
    ///   desk live, its print fresh and sided, and its midpoint within
    ///   `max_deviation_bps` of the pool's price now.
    /// - Every desk quote must still fit the caps on the desk's latest
    ///   reading, and a desk that went unreadable refuses (`Busy`). One that
    ///   turned into a 404 has no caps left to break.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn desk_holds_after_signing(
        &self,
        priced_on: &Gate,
        desk_priced: Option<&(CorridorBook, Quote)>,
        taker: Address,
        book: &CorridorBook,
        order: &crate::protocol::types::OrderParams,
        plan: &QuotePlan,
        prices: &super::PriceCache,
        priced_at: u64,
    ) -> Result<(), super::wire::RejectReason> {
        use super::wire::RejectReason;
        let now = crate::time::unix_now();
        let current = self.desk_gate(taker, book, now);
        // What the taker is can change while signing: a feed lists it, drops
        // it, or goes unreadable. A quote priced and capped as one thing never
        // goes out as another; a customer that just became a desk wallet would
        // skip the desk's caps and leave no claim.
        match (priced_on, &current) {
            (Gate::Customer, Gate::Customer) => return Ok(()),
            (Gate::Desk { feed: was, .. }, Gate::Desk { feed: is, .. }) if was == is => {}
            _ => return Err(RejectReason::Busy),
        }
        if let Some((desk_book, desk_quote)) = desk_priced {
            super::guard::recheck_after_signing(
                prices,
                order,
                plan.bid,
                desk_book,
                self.price_guard.max_loss_bps,
                Some(desk_quote),
                priced_at,
            )?;
        }
        match current {
            Gate::Desk {
                feed,
                info,
                read_at,
            } => {
                if desk_priced.is_some() {
                    let spec = &self.desks.feeds[feed];
                    let pool = prices.get(&book.feed_url).ok_or(RejectReason::StaleFeed)?;
                    let desk_now = prices.get(&spec.spec.feed_url);
                    if desk_price_refusal(&info, desk_now.as_ref(), pool.price, spec, now).is_err()
                    {
                        return Err(RejectReason::Busy);
                    }
                }
                match self.desk_cap_refusal(feed, &info, plan, read_at) {
                    Some(_) => Err(RejectReason::TakerLimit),
                    None => Ok(()),
                }
            }
            Gate::Busy(_) => Err(RejectReason::Busy),
            Gate::Customer => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_DESK_MAX_DEVIATION_BPS;
    use crate::pricing::quote::Spread;
    use std::collections::BTreeSet;

    const URL: &str = "https://api.x/price?otc=desk&pair=cngn-usdt";
    const NOW: u64 = 1_800_000_000;

    fn taker() -> Address {
        Address::repeat_byte(0xab)
    }

    fn feed() -> DeskFeedRuntime {
        DeskFeedRuntime {
            spec: DeskFeed {
                feed_url: URL.into(),
                takers: [taker()].into_iter().collect(),
                max_deviation_bps: DEFAULT_DESK_MAX_DEVIATION_BPS,
                max_order_debt: None,
            },
            staleness_secs: 240,
        }
    }

    fn book() -> CorridorBook {
        CorridorBook {
            clamped_extra_bps: crate::pricing::clamped::DEFAULT_EXTRA_BPS,
            slug: "cngn-usdt".into(),
            collateral: Address::repeat_byte(0xc1),
            debt: Address::repeat_byte(0xd1),
            collateral_decimals: 6,
            debt_decimals: 6,
            buy_spread: Some(Spread::Bps(100)),
            sell_spread: Some(Spread::Bps(100)),
            buy_capacity_debt: None,
            sell_capacity_collateral: None,
            feed_url: "https://pool".into(),
            staleness_secs: 240,
            curve: None,
            feed_sides: false,
            off_hours: None,
            issuer_spread: None,
        }
    }

    fn info(status: DeskStatus) -> DeskInfo {
        DeskInfo {
            status,
            chain_id: 56,
            collateral: Address::repeat_byte(0xc1),
            debt: Address::repeat_byte(0xd1),
            takers: [taker()].into_iter().collect(),
            account_collateral: U256::from(1_000u64),
            account_debt: U256::from(500u64),
            max_order_debt: U256::from(400u64),
        }
    }

    fn cached(reading: DeskReading, at: u64) -> DeskCache {
        let cache = DeskCache::default();
        cache.record(URL, reading, at);
        cache
    }

    #[test]
    fn only_a_listed_wallet_on_the_desks_book_is_the_desks() {
        let feeds = [feed()];
        let live = cached(DeskReading::Shared(info(DeskStatus::Live)), NOW);
        assert!(matches!(
            gate(&feeds, &live, taker(), 56, &book(), NOW),
            Gate::Desk { feed: 0, .. }
        ));
        let other = Address::repeat_byte(0x01);
        assert_eq!(gate(&feeds, &live, other, 56, &book(), NOW), Gate::Customer);
        assert_eq!(
            gate(&feeds, &live, taker(), 8453, &book(), NOW),
            Gate::Customer,
            "another chain"
        );
        let other_pair = CorridorBook {
            debt: Address::repeat_byte(0xd2),
            ..book()
        };
        assert_eq!(
            gate(&feeds, &live, taker(), 56, &other_pair, NOW),
            Gate::Customer
        );
        // The feed no longer lists the wallet: it's a customer again.
        let unlisted = DeskInfo {
            takers: BTreeSet::new(),
            ..info(DeskStatus::Live)
        };
        let cache = cached(DeskReading::Shared(unlisted), NOW);
        assert_eq!(
            gate(&feeds, &cache, taker(), 56, &book(), NOW),
            Gate::Customer
        );
    }

    #[test]
    fn a_stale_reading_holds_for_the_grace_period_then_refuses() {
        let feeds = [feed()];
        let at = NOW - DESK_GRACE_SECS;
        let cache = cached(DeskReading::Shared(info(DeskStatus::Stale)), at);
        assert!(matches!(
            gate(&feeds, &cache, taker(), 56, &book(), NOW),
            Gate::Desk { .. }
        ));
        assert!(matches!(
            gate(&feeds, &cache, taker(), 56, &book(), NOW + 1),
            Gate::Busy(_)
        ));
        assert!(matches!(
            gate(&feeds, &DeskCache::default(), taker(), 56, &book(), NOW),
            Gate::Busy(_)
        ));
    }

    #[test]
    fn a_removed_feed_makes_the_wallet_a_customer() {
        let feeds = [feed()];
        let cache = cached(DeskReading::Gone, NOW);
        assert_eq!(
            gate(&feeds, &cache, taker(), 56, &book(), NOW),
            Gate::Customer
        );
    }

    fn print(bid: f64, ask: f64) -> Quote {
        Quote {
            price: (bid + ask) / 2.0,
            timestamp: NOW - 1,
            degraded: false,
            reference: None,
            bid: Some(bid),
            ask: Some(ask),
            session: Default::default(),
            market_at: None,
            market_source: None,
            degraded_reason: Default::default(),
            clamp_gap_bps: None,
            spread: Default::default(),
        }
    }

    #[test]
    fn the_desk_price_needs_a_live_fresh_sided_print_near_the_pool() {
        let live = info(DeskStatus::Live);
        let f = feed();
        assert_eq!(
            desk_price_refusal(&live, Some(&print(0.99, 1.01)), 1.0, &f, NOW),
            Ok(())
        );
        // Either side of the 200 bps guard, both ways.
        assert!(desk_price_refusal(&live, Some(&print(1.0189, 1.0209)), 1.0, &f, NOW).is_ok());
        assert!(desk_price_refusal(&live, Some(&print(0.9811, 0.9831)), 1.0, &f, NOW).is_ok());
        assert!(desk_price_refusal(&live, Some(&print(1.0191, 1.0211)), 1.0, &f, NOW).is_err());
        assert!(desk_price_refusal(&live, Some(&print(0.9789, 0.9809)), 1.0, &f, NOW).is_err());
        assert!(desk_price_refusal(
            &info(DeskStatus::Stale),
            Some(&print(0.99, 1.01)),
            1.0,
            &f,
            NOW
        )
        .is_err());
        assert!(desk_price_refusal(&live, None, 1.0, &f, NOW).is_err());
        let old = Quote {
            timestamp: NOW - 241,
            ..print(0.99, 1.01)
        };
        assert!(desk_price_refusal(&live, Some(&old), 1.0, &f, NOW).is_err());
        let unsided = Quote {
            bid: None,
            ask: None,
            ..print(0.99, 1.01)
        };
        assert!(desk_price_refusal(&live, Some(&unsided), 1.0, &f, NOW).is_err());
    }

    fn plan(bid: bool, input: u64, output: u64) -> QuotePlan {
        QuotePlan {
            bid,
            input_token: Address::ZERO,
            input: U256::from(input),
            output_token: Address::ZERO,
            output: U256::from(output),
            fee: U256::ZERO,
            sell_amount: U256::from(output),
            buy_amount: U256::from(input),
        }
    }

    #[test]
    fn the_taker_gets_whichever_plan_pays_it_more() {
        assert!(taker_prefers(&plan(true, 101, 100), &plan(true, 100, 100)));
        assert!(
            !taker_prefers(&plan(true, 100, 100), &plan(true, 100, 100)),
            "tie: public"
        );
        assert!(!taker_prefers(&plan(true, 99, 100), &plan(true, 100, 100)));
        // Exact output: same buy, the cheaper sell wins.
        assert!(taker_prefers(&plan(false, 100, 99), &plan(false, 100, 100)));
        let max = QuotePlan {
            buy_amount: U256::MAX,
            sell_amount: U256::MAX,
            ..plan(true, 0, 0)
        };
        assert!(!taker_prefers(&max, &max), "no overflow at the top");
    }

    #[test]
    fn each_side_is_capped_by_its_account_less_what_is_held() {
        let i = info(DeskStatus::Live);
        // Bid pays debt: account.debt is 500.
        assert_eq!(
            cap_refusal(&plan(true, 300, 300), &i, U256::from(200u64), None),
            None
        );
        assert_eq!(
            cap_refusal(&plan(true, 300, 300), &i, U256::from(201u64), None),
            Some(CapRefusal::Account)
        );
        // Ask pays collateral: account.collateral is 1000, debt leg is the output.
        assert_eq!(
            cap_refusal(&plan(false, 1_000, 400), &i, U256::ZERO, None),
            None
        );
        assert_eq!(
            cap_refusal(&plan(false, 1_001, 100), &i, U256::ZERO, None),
            Some(CapRefusal::Account)
        );
    }

    fn room_of(bid: bool, payout: u64, debt_leg: u64) -> DeskRoom {
        DeskRoom {
            bid,
            payout: U256::from(payout),
            debt_leg: U256::from(debt_leg),
        }
    }

    #[test]
    fn room_is_the_account_less_what_is_held_and_the_lower_order_cap() {
        let i = info(DeskStatus::Live);
        // account.debt 500, account.collateral 1000 (see `info`).
        let r = room(&i, true, U256::from(200u64), None);
        assert_eq!(r.payout, U256::from(300u64));
        assert_eq!(r.debt_leg, i.max_order_debt);
        let r = room(&i, false, U256::from(2_000u64), Some(U256::from(7u64)));
        assert_eq!(r.payout, U256::ZERO, "held past the account: no room");
        assert_eq!(r.debt_leg, U256::from(7u64).min(i.max_order_debt));
    }

    #[test]
    fn a_plan_that_fits_comes_back_unchanged() {
        let p = plan(true, 300, 600);
        assert_eq!(
            fit_to_room(&p, &room_of(true, 300, 300), 1),
            Some(p.clone())
        );
        let p = plan(false, 600, 300);
        assert_eq!(fit_to_room(&p, &room_of(false, 600, 300), 1), Some(p));
    }

    #[test]
    fn a_bid_is_cut_to_the_lower_of_payout_and_debt_leg() {
        let fit = |payout, debt_leg| {
            fit_to_room(
                &plan(true, 1_000_000, 2_000_000),
                &room_of(true, payout, debt_leg),
                1,
            )
            .unwrap()
        };
        assert_eq!(fit(999_999, 2_000_000).input, U256::from(999_999u64));
        assert_eq!(fit(2_000_000, 400_000).input, U256::from(400_000u64));
        // Output scales with it, rounded up: never better for the taker.
        // 2_000_001 * 333_333 / 1_000_000 = 666_666.33…
        let p = fit_to_room(
            &plan(true, 1_000_000, 2_000_001),
            &room_of(true, 333_333, u64::MAX),
            1,
        )
        .unwrap();
        assert_eq!(p.output, U256::from(666_667u64));
        assert_eq!(p.buy_amount, p.input);
        assert_eq!(
            p.sell_amount,
            super::super::math::gross_of(p.output, 1).unwrap()
        );
    }

    #[test]
    fn an_ask_keeps_its_debt_leg_under_the_cap_after_rounding_up() {
        // Pays ~3 collateral per debt received, with an odd output so the
        // scaled output never divides evenly. Every debt cap must hold after
        // the output is rounded up.
        let p = plan(false, 3_000_000_000, 1_000_000_001);
        for cap in [
            10_000u64,
            10_001,
            99_999,
            333_333_333,
            999_999_999,
            1_000_000_000,
        ] {
            let fit = fit_to_room(&p, &room_of(false, u64::MAX, cap), 1)
                .unwrap_or_else(|| panic!("cap {cap} leaves room"));
            assert!(fit.output <= U256::from(cap), "cap {cap}: {}", fit.output);
            assert!(fit.input < p.input, "cap {cap}");
            // Price never better for the taker than the plan's.
            assert!(
                U512::from(fit.output) * U512::from(p.input)
                    >= U512::from(p.output) * U512::from(fit.input),
                "cap {cap}"
            );
        }
    }

    #[test]
    fn nothing_fits_without_room_or_below_the_feeable_minimum() {
        let p = plan(true, 1_000_000, 1_000_000);
        assert_eq!(fit_to_room(&p, &room_of(true, 0, u64::MAX), 1), None);
        // At 1 bps the smallest feeable output is 10_000 atoms.
        assert_eq!(fit_to_room(&p, &room_of(true, 9_999, u64::MAX), 1), None);
        assert!(fit_to_room(&p, &room_of(true, 10_000, u64::MAX), 1).is_some());
    }

    #[test]
    fn the_debt_leg_is_held_to_the_lower_of_the_two_order_caps() {
        let i = info(DeskStatus::Live);
        assert_eq!(
            cap_refusal(&plan(false, 10, 401), &i, U256::ZERO, None),
            Some(CapRefusal::MaxOrderDebt)
        );
        assert_eq!(cap_refusal(&plan(true, 400, 1), &i, U256::ZERO, None), None);
        assert_eq!(
            cap_refusal(
                &plan(true, 400, 1),
                &i,
                U256::ZERO,
                Some(U256::from(399u64))
            ),
            Some(CapRefusal::MaxOrderDebt)
        );
        assert_eq!(
            cap_refusal(
                &plan(true, 400, 1),
                &i,
                U256::ZERO,
                Some(U256::from(10_000u64))
            ),
            None,
            "the TOML cap only lowers the feed's"
        );
    }

    #[test]
    fn a_claim_holds_to_its_deadline_or_for_two_minutes_after_its_fill() {
        let mut claims = DeskClaims::new();
        claims
            .reserve("a", URL, true, U256::from(100u64), NOW + 10)
            .unwrap();
        claims
            .reserve("b", URL, false, U256::from(7u64), NOW + 10)
            .unwrap();
        claims
            .reserve("c", "https://other", true, U256::from(1u64), NOW + 10)
            .unwrap();
        assert_eq!(claims.held(URL, true, NOW), U256::from(100u64));
        assert_eq!(claims.held(URL, false, NOW), U256::from(7u64));
        // Unfilled: until a reading 120 s past the deadline plus skew, in
        // case it filled at the deadline unseen.
        let release = NOW + 10 + RELEASE_SKEW_SECS;
        assert_eq!(
            claims.held(URL, false, release + FILL_HOLD_SECS - 1),
            U256::from(7u64)
        );
        assert_eq!(
            claims.held(URL, false, release + FILL_HOLD_SECS),
            U256::ZERO
        );

        claims.mark_filled(&["a".into(), "not-a-desk-claim".into()], NOW + 5);
        let hold = NOW + 5 + FILL_HOLD_SECS;
        assert!(hold > release, "the fill hold outlives the deadline");
        assert_eq!(claims.held(URL, true, hold - 1), U256::from(100u64));
        assert_eq!(claims.held(URL, true, hold), U256::ZERO);
        // The feed went down before a reading could show the fill: however
        // late it gets, the account it last read still needs the claim.
        claims.prune(hold + DESK_GRACE_SECS - 1);
        assert_eq!(claims.held(URL, true, NOW + 6), U256::from(100u64));
        // Past the grace period the desk is refused on that old reading, so
        // the claim can go.
        claims.prune(hold + DESK_GRACE_SECS);
        assert_eq!(claims.held(URL, true, NOW + 6), U256::ZERO);
    }

    #[test]
    fn a_failed_write_stops_desk_quotes_until_one_goes_through() {
        let dir = std::env::temp_dir().join(format!("desk-claims-ro-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DESK_CLAIMS_FILE);
        let mut claims = DeskClaims::load(&path, NOW).unwrap();
        claims
            .reserve("a", URL, true, U256::from(100u64), NOW + 10)
            .unwrap();
        assert!(!claims.unsaved());
        // The folder goes away: the fill can't be saved.
        std::fs::remove_dir_all(&dir).unwrap();
        claims.mark_filled(&["a".into()], NOW + 5);
        assert!(
            claims.unsaved(),
            "a restart would reload the claim without its hold"
        );
        // Back again: the next prune retries and clears it.
        std::fs::create_dir_all(&dir).unwrap();
        claims.prune(NOW + 6);
        assert!(!claims.unsaved());
        let back = DeskClaims::load(&path, NOW + 6).unwrap();
        assert_eq!(back.held(URL, true, NOW + 6 + 100), U256::from(100u64));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_claim_counts_against_its_desk_however_the_url_is_spelled_after() {
        let dir = std::env::temp_dir().join(format!("desk-claims-key-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DESK_CLAIMS_FILE);
        let before = "https://api.x/price?otc=desk&pair=cngn-usdt";
        let mut claims = DeskClaims::load(&path, NOW).unwrap();
        claims
            .reserve("a", before, true, U256::from(100u64), NOW + 10)
            .unwrap();
        // The operator adds a parameter and restarts while the quote is live.
        let after = "https://api.x/price?pair=cngn-usdt&otc=desk&offHours=1";
        let back = DeskClaims::load(&path, NOW + 1).unwrap();
        assert_eq!(back.held(after, true, NOW + 1), U256::from(100u64));
        // Another desk on the same endpoint is still apart.
        assert_eq!(
            back.held(
                "https://api.x/price?otc=other&pair=cngn-usdt",
                true,
                NOW + 1
            ),
            U256::ZERO
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_claim_that_cannot_be_saved_is_not_kept() {
        let path = std::env::temp_dir()
            .join(format!("desk-claims-missing-{}", rand::random::<u64>()))
            .join(DESK_CLAIMS_FILE);
        let mut claims = DeskClaims::load(&path, NOW).unwrap();
        assert!(claims
            .reserve("a", URL, true, U256::from(100u64), NOW + 10)
            .is_err());
        assert!(
            claims.is_empty(),
            "the quote is withheld, so nothing is held for it"
        );
    }

    #[test]
    fn only_a_never_exposed_unfilled_loser_releases_early() {
        let mut claims = DeskClaims::new();
        claims
            .reserve("lost", URL, true, U256::from(1u64), NOW + 10)
            .unwrap();
        claims
            .reserve("picked", URL, true, U256::from(2u64), NOW + 10)
            .unwrap();
        claims.mark_exposed("picked");
        assert!(claims.release_unexposed("lost"));
        assert!(!claims.release_unexposed("picked"));
        assert_eq!(claims.held(URL, true, NOW), U256::from(2u64));
    }

    #[test]
    fn claims_survive_a_restart_with_their_fill_holds() {
        let dir = std::env::temp_dir().join(format!(
            "desk-claims-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DESK_CLAIMS_FILE);
        let mut claims = DeskClaims::load(&path, NOW).unwrap();
        claims
            .reserve("live", URL, true, U256::from(5u64), NOW + 10)
            .unwrap();
        claims
            .reserve("filled", URL, true, U256::from(9u64), NOW + 1)
            .unwrap();
        claims
            .reserve("gone", URL, true, U256::from(1u64), NOW - 400)
            .unwrap();
        claims.mark_filled(&["filled".into()], NOW);

        let back = DeskClaims::load(&path, NOW + 50).unwrap();
        assert_eq!(
            back.held(URL, true, NOW + 50),
            U256::from(14u64),
            "the live one's order expired but it may have filled unseen; the fill held"
        );
        assert_eq!(
            back.held(URL, true, NOW + 10 + RELEASE_SKEW_SECS + FILL_HOLD_SECS),
            U256::ZERO
        );
        // "live" expired but is kept through the grace period; "gone" was
        // past it before the restart.
        assert_eq!(back.by_rfq.len(), 2);
        assert!(back.by_rfq.values().all(|c| c.exposed));

        std::fs::write(&path, "{").unwrap();
        assert!(DeskClaims::load(&path, NOW).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_desk_book_quotes_the_desk_feed_flat() {
        let b = desk_book(&book(), &feed());
        assert_eq!(b.feed_url, URL);
        assert!(b.feed_sides && b.curve.is_none() && b.off_hours.is_none());
        assert_eq!((b.collateral, b.debt), (book().collateral, book().debt));
        assert_eq!(
            b.buy_spread,
            book().buy_spread,
            "sides stay as the pool has them"
        );
    }
}
