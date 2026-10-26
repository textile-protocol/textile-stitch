# cNGN quote curve — stitch pricing spec

The cNGN stitch answers every quote request with a price derived from a constant centre. This
replaces that constant with a centre that moves on the stitch's own naira inventory, and prices
each quote across the stretch of inventory the trade would move it through rather than at the
point it starts from.

Two sentences for why: a constant centre has no defence as naira builds up, and a centre that
moves but is quoted at a single point can be round-tripped for free.

This is the canonical design document for the implementation in `src/pricing/curve.rs` and
`decide_quote_curved` in `src/rfq/responder.rs`.

**Scope: cNGN only.** IDRX, XAUt, wBRL and the rest keep answering flat off their reference.
The test for whether a corridor wants this is in "Which corridors" at the end.

## 1. How a quote happens today

Grounding, because every rule below is a rule about this one function.

A taker asks the venue for a price. The venue fans the request out to stitches. Each stitch
answers, the best answer wins, and the winner's signed order settles. In the bot that answer is
`decide_quote` in `rfq/responder.rs`.

What arrives in the request (`QuoteRequestFrame`):

| field | meaning |
|---|---|
| `sell_token` / `buy_token` | direction. What the taker sells, what it buys. Determines whether the stitch is buying or selling cNGN. |
| `sell_amount` **or** `buy_amount` | the size. **Exactly one is set** — see §5. |
| `fee_bps` | the venue fee, injected on top of the maker's output. Not the stitch's spread. |
| `quote_ttl_ms`, `reply_by` | how long the answer stays firm, and the cutoff for sending it. |

What the stitch already has in hand: its own wallet balances (`InventoryView`), its configured
spread (`book.buy_spread` / `book.sell_spread`), its remaining capacity, and the size of every
quote it has already given out and not yet seen expire (`reserved_bid` / `reserved_ask`).

What it does with them today: takes `mid`, adds or subtracts the spread, and prices the whole
requested size at that one number.

**Two properties of this channel drive the entire design:**

1. **The taker names a size.** So the stitch knows, before answering, exactly where the trade
   would leave its inventory. It never has to price a size it cannot see.
2. **An answer is a promise.** A quote is firm for `quote_ttl_ms` whether or not it is taken,
   and several can be outstanding at once. Inventory the stitch has promised is gone for pricing
   purposes even though the chain still shows it.

## 2. Units, stated once

The operator, the Bybit reference and the endpoints below all live in **NGN per USDT**. The
bot's order amounts live in the reciprocal, **USDT per cNGN**.

**Do the whole curve in NGN per USDT and invert exactly once**, at the step where a price turns
into order amounts. Every formula in this document is in NGN per USDT for that reason.

That is not a stylistic choice. A shade that raises the centre in NGN per USDT lowers it in
USDT per cNGN, so a formula mirrored into the bot's units turns every sign around. Getting it
backwards leans the curve *into* naira instead of out of it, which is worse than not shipping.
One inversion, at the boundary, pinned by the endpoint test in §11.

The two ends are not mirror images: the half-range sits on the `f = 1` end only. So the curve
assumes the pool's collateral is the soft currency and its debt the dollar. On a reversed
corridor (USDT collateral, cNGN debt) `f` would be the dollar share and the half-range would
land on the dollar-heavy end, where shedding is cheap, while the naira-heavy end quotes at the
mid. Config refuses the curve on a pool whose feed reads `invert=1`, which is how the corridor
registry generates reversed pools. A hand-written feed on the reversed pair key isn't detected,
so don't point a curve pool at one.

```
f = 0   no naira    shade at its lowest (negative)   the stitch sells dollars at the mid
f = 1   all naira   shade at its highest             the stitch pays the Bybit buy for dollars
```

## 3. The rule

The curve runs between two prices the operator sets, each an offset in NGN from a Bybit
reference. **These two are the knobs:**

```
at f = 0  (no naira)     the stitch SELLS USDT at   bybitMid - startNgn
at f = 1  (all naira)    the stitch BUYS  USDT at   bybitBuy + endNgn
```

Both default to 0, and both may only ever widen the curve. At 0 / 0 the stitch sells dollars at
the Bybit mid when it holds no naira, and pays the Bybit buy price for dollars when it is full
of naira. These are **all-in** prices, after the spread and the fee, so they are the numbers a
taker actually sees.

Everything else follows:

```
f        = the stitch's own cNGN share of its own balances, valued in USDT
           f = (cngn / bybitMid) / (cngn / bybitMid + usdt)    0 = no naira, 1 = all naira

allIn    = this corridor's configured spread + req.fee_bps                         bps

s0       = ( (bybitMid - startNgn) / (bybitMid x (1 + allIn/10000)) - 1 ) x 10000   bps
s1       = ( (bybitBuy  + endNgn)  / (bybitMid x (1 - allIn/10000)) - 1 ) x 10000   bps

shade(f) = s0 + (s1 - s0) x f^2                                                     bps
centre   = bybitMid x (1 + shade/10000)                             NGN per USDT

the stitch SELLS USDT (it buys cNGN)  = centre x (1 + allIn/10000)
the stitch BUYS  USDT (it sells cNGN) = centre x (1 - allIn/10000)
```

**`s0` is negative, and that matters.** At 0 / 0 knobs it sits at about `-allIn`, and `s1` at
about `bybitHalfRange + allIn`, so the span is `bybitHalfRange + 2 x allIn` — about **13.4 bps**
on a 9.4 bps half-range, not 11.4. The curve starts one spread *below* the mid, and that is
exactly what puts the all-in selling price on the mid. **A formula that starts the shade at zero
is wrong by one full spread across the bottom of the range.**

`bybitHalfRange` is `(bybitBuy - bybitMid) / bybitMid` in bps, about 9.4 on a normal reference.

**Convert the knobs to bps off the live mid on every quote**, not once at startup, so "0.5 NGN
below mid" stays literally true as the mid moves.

**Refuse the curve unless `s1 > s0`** and fall back to flat quoting (§9). That is also what
catches a crossed Bybit reference, where the buy prints below the mid.

**`allIn` is computed, not configured.** The venue fee arrives on the request as `fee_bps`, and
the spread is already on the corridor, so the stitch can derive the true all-in cost per quote.
This matters because `allIn` sets where the curve ends (§7): configure it and a fee change
silently moves the endpoint; derive it and the endpoint stays correct by construction.

**Everything is relative.** Nothing needs re-tuning when the stitch's inventory grows or the
Bybit reference widens.

**`f` is per stitch, per corridor, per chain.** One stitch quoting cNGN on BSC and on Celo runs
two curves off two balances. The skew cannot live in `/api/price`: the server does not know any
stitch's inventory, and two stitches holding different amounts need different shades off the
same mid.

## 4. A quote prices the stretch, not the point

**This is the core of the design and the part most likely to be built wrong.**

The taker names a size, so the stitch knows where the trade ends. It must quote the **average
shade across the inventory it moves through**, not the shade where it starts.

```
S  = the quoted size, valued in USDT          (see §5 — it is the size QUOTED, not REQUESTED)
V  = the stitch's total balances, valued in USDT

f1 = f0 - S/V    when the stitch sells cNGN   (cNGN leaves, naira share falls)
f1 = f0 + S/V    when the stitch buys  cNGN   (cNGN arrives, naira share rises)

averageShade(f0, f1) = s0 + (s1 - s0) x (f1^3 - f0^3) / (3 x (f1 - f0))
```

General form for exponent `n`: `s0 + (s1 - s0) x (f1^(n+1) - f0^(n+1)) / ((n+1) x (f1 - f0))`.
It is the mean value of `shade` over the interval, so **order does not matter** — that is the
whole safety argument. A zero-width interval has no average; fall back to the point shade.

**The `s0` term is not optional.** Averaging only the `f^n` part prices every quote one full
spread away from where it belongs, and the error is worst exactly where the curve is meant to be
most aggressive. It still passes a round-trip test, because a constant cancels out of the round
trip — so that test will not catch this. The endpoint test in §11 is what catches it.

`S/V` is the distance travelled along the share axis because a fill at fair moves value from one
leg to the other without changing the total.

Clamp `f1` into `[0, 1]` defensively, but note it cannot bind: the largest buy-side fill is the
stitch's whole USDT balance, which takes `f` to exactly 1, and the largest sell-side fill takes
it to exactly 0. **Capacity and the boundary are the same line.** If the clamp ever fires, the
size did not come from capacity and something upstream is wrong.

### Why this is not a refinement

Quote the point and the stitch can be robbed, with the reference price never moving:

1. Ask for a large quote in the direction the stitch is loaded. Take it. The whole size prices
   at the starting shade, the best price in the trade.
2. The fill moves `f`, which moves the centre in the taker's favour for the other direction.
3. Ask for a quote the other way and sell straight back.
4. Keep the difference.

The attacker profits whenever one trade moves the shade by more than the round-trip spread. The
worst case is the whole book in one go, which harvests the entire span less the spread:
**`span - 2 x allIn`**, about 9.4 bps at 0 / 0 knobs on a 9.4 bps half-range. And it is
repeatable — a round trip leaves the book exactly where it started, so the attacker simply goes
again.

Pricing the stretch closes it structurally, with no rate limits and no timing guards: out and
back is the same integral, so the shade cancels and a round trip nets exactly minus the spread,
twice, at every size and every starting point.

**Measured.** A sweep of every starting inventory from 1% to 99% against every size from 1% to
100%, both directions, ~19,800 cases per reference width:

| pricing rule | 0 / 0 knobs | mid−0.5 / buy+1 knobs |
|---|---|---|
| the point `f0` | **+9.40 bps to the attacker** | **+20.27 bps to the attacker** |
| the stretch `[f0, f1]` | −4.0 bps (the spread, twice) | −4.0 bps |

Negative is the taker paying the stitch. The stretch rule never goes positive at any size, any
inventory level, or any knob setting. The point rule fails harder the wider the curve, so the
cost of getting this wrong grows with every turn of a knob.

The sweep that guards this in the suite is
`a_round_trip_costs_the_taker_the_spread_at_every_size_and_share` and
`invariants_hold_over_a_randomized_sweep` in `pricing/curve.rs`, plus
`a_live_round_trip_pays_the_stitch_the_spread` through `decide_quote_curved`.

## 5. Sizing: what `S` actually is

Three things make `S` not simply "the number in the request". All three are ordering
requirements, so §6 fixes the order.

**a. One side is named, and it may be the wrong side.** Exactly one of `sell_amount` /
`buy_amount` is set. Exact-input means the taker's named amount is a gross cap and the stitch
computes what fits under it; exact-output means the stitch pays exactly the named amount. When
the named amount is in cNGN, converting it to USDT to get `S` needs a price — and the price
needs `S`. That circularity is real but tiny: the shade is a few bps, so take the point price at
`f0` as the first estimate and iterate twice. It converges immediately. Do not pretend it is
not there.

**b. The venue fee is on top.** `fee_bps` is injected on the maker's output and is not part of
the signed order. The size that moves the stitch's inventory is the maker's own leg, not the
taker's gross. Walk the curve on the maker's leg.

**c. The quote may be smaller than the request.** Capacity is the configured cap minus the
stitch's own outstanding claims minus the wallet, and when the request exceeds it the stitch
quotes what remains rather than rejecting. **Clamp first, then price.** Pricing the requested
size and then clamping quotes a stretch the trade never travels, which under-charges and
reopens the hole on exactly the large tickets that matter.

## 6. Order of operations in `decide_quote`

The rules above are order-sensitive. This is the order.

1. **Direction.** From `sell_token` / `buy_token`, as today. Reject if not this corridor's pair.
2. **`allIn`** = corridor spread + `req.fee_bps`.
3. **`f0` and `V`** from the bot's holdings (`CurveInputs.holdings`, measured by the host):
   a wallet bot's raw wallet balances, a vault's free balances (§8), valued at the raw Bybit
   mid. If the reading is missing or stale, quote flat (§9).
4. **Advance `f0` by this side's own outstanding quotes** — §8. **This side only.** It is the
   step most easily forgotten, and it is a correctness step, not an optimisation.
5. **Capacity**, as today, net of reservations.
6. **Clamp the size to capacity** — §5c. Everything after this prices the size actually quoted.
7. **`S` in USDT**, from the clamped maker-leg size, iterating if the named side is cNGN (§5a).
8. **`f1 = f0 ± S/V`**.
9. **`centre = bybitMid x (1 + averageShade(f0, f1)/10000)`** in NGN per USDT, then apply **the
   corridor spread alone** for the direction, **then invert once** into the bot's own units (§2).

   **Not the all-in.** The venue injects `fee_bps` on the amounts in step 10, so an order that
   carried the all-in would charge the taker the fee twice and miss the knob endpoints by
   exactly `fee_bps`; the realized-rate tests pin this. The all-in's whole job is sizing `s0`/`s1` (§3), so that the TAKER-REALIZED price —
   the order with the venue fee injected on top — is what lands on the knobs.
10. **Amounts** from that price, exactly as today (this is where the venue fee joins).
11. **Reserve** the quoted size, so step 4 sees it on the next quote.

Steps 1, 5, 10 and 11 exist today. Steps 2 to 4 and 6 to 9 are the work.

**The levels follow the same curve.** The venue judges every firm reply against the levels it
was routed on and rejects one more than `levelSlackBps` (5 by default) worse for the taker as
`level_slack`. Flat levels beside a Live curve would get the loaded side struck exactly where it
leans: past about 72% naira at 0 / 0 knobs. So a Live side publishes `CURVE_LEVEL_ROWS` (8) rows
instead of one, the remaining capacity cut into equal slices from the same starting point
(steps 2 to 4), each priced at the curve's average over its own slice. The average is additive,
so the venue's size-weighted anchor over the rows a request consumes is the firm quote's own
price; a part-consumed last row is priced over its whole slice, which only errs in the reply's
favour. The levels carry no request, so they use the venue fee from the last one (it is per
chain). Shadow publishes flat levels, as it answers flat.

The venue judges a reply against the snapshot it routed on, and a firm quote prices from the
far end of this side's outstanding quotes (§8), so a large same-side quote between two
publishes moves the next one along the curve before the levels follow. Two things keep that
from becoming a strike: a Live corridor republishes its levels right after every request instead
of waiting for the 1 s tick, and a reply that would still sit more than the corridor's slack
past the levels it last sent is declined as `Busy`. That covers flat replies too: a request the
curve can't price (no reference book, no fresh balance reading) answers flat against levels that
may have been curved, a full spread apart near the middle of the range. The slack is the corridor's own
`levelSlackBps`, sent per corridor pair in `sessionAccepted`; a venue that predates the field
gets the default, 5 (`DEFAULT_LEVEL_SLACK_BPS`).

**Every input is already in the function.** Verified against the code, not assumed:

| the curve needs | where it already is |
|---|---|
| `f0`, `V` | `CurveInputs.holdings`: wallet raw balances, or the vault's free balances |
| the size | `req.sell_amount` / `req.buy_amount`, already parsed |
| `allIn` | `book.buy_spread` / `book.sell_spread` plus `req.fee_bps` |
| this side's outstanding quoted size | `reserved_bid` / `reserved_ask`, already parameters |
| raw Bybit mid and half-range | **the only genuinely missing input** — land this first |

## 7. Where the curve starts and ends, and how steeply it travels

### The two knobs

`startNgn` pays to win dollar-buying flow while the stitch holds no naira. `endNgn` pays to shed
naira once it is full. Both are subsidies, both default to 0, and both widen the curve.

At 0 / 0 the end is the principled stopping point: acquiring dollars at the Bybit buy costs
about 9.4 bps over fair, which is exactly what dumping the naira on Bybit as a taker would cost.
Past that the stitch should dump rather than lean. Leaning at that price still beats dumping at
it — the stitch keeps its spread, settles atomically, and never crosses a P2P spread with escrow
and waiting. Push `endNgn` past 0 with that trade-off in view, not by default.

Widening either knob widens the span, which raises what a mispriced quote is worth to a taker.
It does not weaken §8 — reservations are exact at any span — but it does raise the cost of
getting §8 wrong. The round-trip sweep must be re-run against any knob setting before it ships.

### The exponent is `f^2`

It keeps the stitch near the Bybit mid through normal inventory and only defends at the top. At
50% naira it has spent a quarter of its span; at 80%, 64%.

The consequence that matters: **the stitch stays the cheapest dollars in the market until about
84% naira**, at 0 / 0 knobs on a 9.4 bps half-range. The crossing is where its all-in selling
price reaches the Bybit buy:

```
cheapest until f = ( (bybitHalfRange - allIn - s0) / (s1 - s0) ) ^ (1/n)
```

On today's narrower 5.65 bps reference that line sits at about **64%**. Under the older endpoint,
which started the shade at zero, the same two figures were 81% and 41% — so the start knob's
default is worth about 3 points at the wide end and 23 at the narrow one. That was the main
doubt about whether the curve earns its keep on a tight book, and it is much reduced.

It also matches every reference design — DODO's PMM, Curve's StableSwap, Avellaneda-Stoikov all
stay flat near the anchor and steepen at the edge. The cost is weaker naira-clearing through the
middle of the range. **If inventory does not come down after this ships, the exponent is the
first dial to turn, not the endpoint.**

## 8. Two quotes cannot own the same stretch

The average in §4 is charged **per quote**, and it can only see the quote it is pricing. Left
alone that is exploitable: split what would have been one large trade into many small quotes
fired inside a single inventory read. Each is tiny, so each pays almost no shade, and each is
individually priced correctly. Together they move the book as far as the single large trade
would have, and the attacker unwinds once in a trade that *does* pay the full average.

Splitting the order dodges the slippage, because the slippage is per order and nothing connects
one order to another.

**Fix: an outstanding quote reserves its stretch of curve, and the next quote on that side
prices from the far end of it.**

```
f0 for this quote = share(wallet) advanced by this stitch's own un-expired quotes ON THIS SIDE
```

Not new plumbing. `Reservations::reserved(corridor, bid, now)` already computes outstanding
quoted size per side with expiry handled, and the values are already parameters of
`decide_quote` — they exist because the stitch already refuses to over-commit inventory. The
curve has to let them move the **price** as well as the **cap**. Note they key on corridor and
side, not on counterparty, so an attacker splitting across identities hits the same pool.

### Same side only, and that is not a detail

If a quote on one side moved the **other** side's price, a taker could request a large quote
they never intend to take, purely to shift the price they do trade at: hold a big buy quote,
execute a sell, let the buy expire. Measured at **+4.47 bps free**, no capital and no risk.

Restricted to the same side, that same sequence is worth **0.000 bps**. A quote you hold can
only ever make that same direction worse for you, so there is nothing to farm.

Opposite sides do not need to know about each other anyway: a taker who trades both at once
pays the spread twice and moves the stitch's inventory nowhere.

### What this buys

- **Same side, any execution order: exact.** Reservations hand out disjoint stretches, so
  whoever fills first the pieces still sum to the whole move. Measured at 0.0000 bps.
- **Opposite-side flow in between: in the stitch's favour.** A held quote was priced at an
  inventory that the intervening trade has since moved away from, and always in the direction
  that leaves the held price generous to us — about **+2.7 bps** on a 20%-of-book example. The
  curve says the more naira we hold the more we pay for dollars; a held buy quote priced before
  someone sold us naira locks in the older, cheaper price. Not a hole, a windfall.
- **A quote that expires unfilled:** the next one on that side was over-charged slightly. In our
  favour, and it decays on its own.

### The one case we accept

If the **stitch's own** activity moves inventory the same direction a signed quote travels — a
rebalance, the closer, a manual wallet move — that quote goes stale against us, about −2 bps.

**Accept it. Do not make rebalances wait for outstanding quotes** (Tomer, 26 Oct 2026). Waiting
looks like the obvious fix and is a worse trade: a counterparty who keeps quotes rolling would
block rebalancing indefinitely and freeze the stitch's capital. That is unbounded damage, and
it blocks risk management specifically, which is what an attacker would want. The exposure it
would be buying off is a couple of bps on a rare coincidence.

**Solvency is already handled and must stay that way.** Reservations hold the funds behind a
live quote, and the rebalance path already nets them off (`rfq/modules.rs`), so a quote that is
taken always has its money behind it. Should that ever fail, the on-chain fill reverts. The
stitch is never on the hook for money it does not have. Any future change to the rebalance path
has to keep subtracting reservations.

The invariant worth remembering: **taker flow can only make our stale quotes better for us.
Only our own operations can make them worse.** The quote TTL is therefore a bound on our own
drift, not a defence against takers.

### A filled quote releases on the next balance read

A `selected` result says a taker holds the signature, not that they used it, so it doesn't
release the claim. Once the order fills, though, the balance shows the trade while the claim
still counts it, and the next quote on that side is worse for the taker by the shade across the
filled stretch. Left to the deadline that lasts the quote TTL plus `RELEASE_SKEW_SECS` (about
90 s at the 60 s default), right after the large fills that carry most of the volume (§14).

So the bot reads the fill itself (`rfq/fills.rs`). Every order spends one Permit2 nonce under its
swapper when it fills, and nothing else spends it. Each balance refresh reads `nonceBitmap` for
the exposed claims in the same Multicall3 batch as the balances, plus the batch's block number;
the loop publishes the balances, then the spent nonces, and the responder releases those claims
before it reads any balance. Two rules keep a release from ever meeting pre-fill balances: the
nonces and the balances come from one block, and the loop drops any batch from an older block
than one it already published (a load-balanced RPC can answer a block behind). The double count
drops to one refresh, about 2 s. Without Multicall3 the reads can't share a block, and claims
hold to their deadline as before.

### On a vault

**The curve is a module, and runs on a vault too.** Its settings are `[modules.curve]` under the
shared `[modules].mode` (Off / Preview / Live), the same switch every module uses. On a wallet
bot it is the only module that runs; the vault-only ones stay off whatever the file says.

On a vault the share is the vault's free balances (`freeCorridor`, `freeSettlement`): NAV with
the settlement staked for yield counted, pending deposits and reserved redemption payouts left
out. It is the reading inventory balancing already uses, read in the same batch as quote
inventory, and the curve only trusts it under three seconds old. It is deliberately not the
quotable amounts the quote path sees, which are capped by allowances and vault policy: a leg
that can't be promised is still held, and leaning on capacity would pin the curve to its most
aggressive end.

Inventory balancing and the curve both lean quotes on what the vault holds, so config refuses
both at once. Run together they would stack two leans, and the inventory module's per-side skew
would hand the curve's two directions different spreads, the unequal-spread hole §3 refuses.
Dynamic spreads can stay on: they add the same buffer to both sides (weighting it by inventory
needs inventory balancing, which is off), so the curve only sees a wider all-in. A round trip
across a change in that buffer still costs the taker at least the smaller all-in twice. Spot
rebalancing and the liquidity schedule don't move prices and run alongside.

### Residual: blocking, not theft

A rival maker can park large un-taken quotes to make our side uncompetitive and win the auction.
They take our flow, not our money. Bounded by the TTL, and by capacity, which a reservation
already consumes. Watch requests against fills per counterparty: large sizes with a low hit rate
is the signature.

## 9. Degraded inputs

**No reference book.** `/price` serves `reference` (`mid`, `halfRangeBps`) only when the sample
has a midpoint and an agreed half-range and isn't degraded. When it is absent the stitch cannot
size the curve and falls back to **answering flat at its configured spread** — the behaviour
before this change — rather than going silent. A malformed `reference` reads as absent: the
curve's optional input never takes the price itself, or the corridor, down with it.

Decided (Tomer, 25 Oct 2026), and it carries a known weakness: answering flat while long naira
is exactly the state the curve exists to defend against, so the fallback is worst precisely when
it is most needed. **Log it loudly** so a long run on the flat path is visible rather than
silent.

**A crossed book is the feed's problem, not the curve's.** The Bybit P2P book is crossed on
roughly a third of ticks. An earlier version clamped a crossed tick's half-range to zero and had
the stitch quote flat on it, which flipped the curve's far end on and off every few minutes in a
way anyone could see on `/price`: buying off the curve on one tick and selling back flat on the
next paid a taker about 3 bps at 82% naira and 7 bps at 100%. The half-range is now the median
of the last 15 minutes of uncrossed books, computed by the pricing cron once a minute and stored
on the sample (`referenceHalfRangeBps`), so it moves only as fast as the book's typical width. With
fewer than three uncrossed books in the window it is null and the reference is omitted.

**A span that is not positive.** If `s1 <= s0`, or the half-range is zero or less, there is no
curve to walk. Fall back to flat and log.

**A reference that disagrees with the price.** The jump guard only sees the served `price`. A
reference mid or buy more than 1% (`MAX_REFERENCE_GAP_BPS`) from that guarded print is a bad
value, not a market: quote flat and log.

**No balance reading** (a wallet read missing, or a vault read older than three seconds). Quote flat, loudly — the same fallback as a missing reference, per the
standing decision that missing information falls back to naive quoting rather than going dark.
(Capacity's own missing-reading behaviour is untouched: a `max` side with no fresh reading still
fails closed, as today.) Never fall back to the point price, which is the stealable rule.

**The share reads raw balances, not quotable amounts.** Quotable inventory is capped by the
Permit2 allowance; a leg the operator never approved (a one-sided pool's inactive side) would
read as empty however loaded the wallet is, pinning the curve to its most aggressive end. The
wallet IS the inventory; the allowance only limits what can be promised.

**A reference far from the print.** The jump guard only sees the served `price`, so the bot
checks the reference itself: if the reference mid or buy is more than 1%
(`MAX_REFERENCE_GAP_BPS`) from the guarded print, quote flat and log it. The guarded print sits
off the raw mid only by the half-range on a `buy`/`sell` alignment and, in Preview, the manual
adjustments (at most ±1 NGN, about 7 bps), so 1% is a bad value, not a market.

**Stale or degraded feed.** Unchanged: the existing `usable()` gate refuses the corridor
outright. This is a different condition from a missing half-range, and must stay different.

## 10. The anchor is the raw Bybit mid

The anchor is `reference.mid` on `/price`, the raw Bybit midpoint, served as its own field. Deliberately **not** the price
`/api/price` serves today:

1. **The alignment toggle.** `/api/price` serves whatever the admin alignment setting says — the
   Bybit ask (`buy`), the mid (`center`) or the bid (`sell`). It was pinned to the ask from 18
   Aug to 25 Oct 2026. Anchor on the served price and flipping that toggle shifts the whole curve
   by a half-range, so `f = 0` starts where `f = 1` was meant to end.

2. **The manual centre adjustments.** The served centre also carries the live centre adjustment
   (`+0.50` NGN as of 23 Oct 2026, about 3.7 bps on a 1365 mid) and, for opted-in feeds, the
   additional one. Those knobs **are the hand-rolled version of this curve** — a human deciding
   the centre should sit above the mid because the stitch is long naira. The curve automates
   exactly that, so it anchors below them, on the raw mid.

**The manual adjustments never reach a Live curve bot.** Anchoring on the raw mid keeps them out
of the curved price, but the flat fallbacks, the jump guard and the flat levels run off the
feed's price. So `/price` also serves `unadjustedPrice`, the same observation before the live and
the additional adjustment, read off the sample's own anchor (`monierate + correction`) rather than
by subtracting today's setting. A bot whose curve is Live reads it everywhere: the tick loop, RFQ
and NAV co-signing share one jump guard per feed, so they switch together
(`Config::price_basis`). Preview keeps the served price, since that's what it answers at.

Cutover is therefore one switch, on the bot. Nothing has to be zeroed in step: the knobs keep
moving every other consumer of the centre (the Swap UI price, NAV attestations, tickers, other
makers) and none of a curve bot's quotes. Whether to zero them later is a separate call about
those consumers. The live adjustment needs a full ADMIN; the additional one is open to OPS_ADMIN.

Serving the book is **not purely additive**: a reversed pair or `/price?invert=1` turns `price`
over, so a mid spread through unchanged would sit in the opposite unit beside it, and inverting a
range swaps its sides. The half-range is served pre-computed as a unitless bps figure, which
inverts cleanly, and both inversion paths go through one `invertQuote`, which turns
`unadjustedPrice` and `reference.mid` over with `price`. The field is source-agnostic (`reference`, not `bybit*`): the
curve reads a reference book, and another corridor's can serve the same shape.

## 11. Acceptance tests

**The round-trip property is the acceptance test.** For every starting inventory and every size
in both directions, a taker who trades and immediately trades back comes out **negative** —
never zero, never positive. It runs on the curve in `pricing/curve.rs` and through
`decide_quote_curved` in `rfq/responder.rs`.

Watch for a false pass. The first run of this harness reported a worst case of exactly
`+0.000`, which read as a clean boundary and was in fact the taker running out of depth rather
than being stopped by the price. Give the return leg headroom so the test measures pricing and
not capacity, and treat any suspiciously round result as a bug in the test before believing it.

Also assert:

- **Both endpoints, at several knob settings, read off the TAKER-REALIZED amounts** — the
  gross the taker sends against what they receive, venue fee included — never off an internal
  price. Judging endpoints off the order price is the blind spot that let a fee double-count
  pass a full test suite once. At `f = 0` the realized selling price equals
  `bybitMid - startNgn`; at `f = 1` the realized buying price equals `bybitBuy + endNgn`; and
  the realized endpoint must NOT move when `fee_bps` changes. Include 0 / 0. **This is the test that catches the two errors most likely to be made**: the inverted
  units in §2, and a shade that starts at zero instead of `s0` (§3). Allow about 0.01 bps of
  tolerance — the spread is a multiplier, so the endpoints land to a few parts in 10^8, not
  exactly.
- **The knobs do not affect safety.** The round-trip sweep stays negative at every knob setting,
  not only the defaults.
- **The edges, which a sweep skips.** `f0 = 0`, `f0 = 1`, and trades spanning the whole book.
  A sweep written as `if (f0 + T <= 1)` silently never tests them.
- **Same side, out of order.** Reserve two quotes on one side, execute the second then the
  first: the total paid must equal pricing the whole move in one go, to the last decimal.
- **Hold one side, trade the other.** Reserve a large quote, execute the opposite side, let the
  first expire. Must be worth **0.000 bps** to the taker. If it is positive, reservations are
  leaking across sides (§8).
- **Opposite-side interleaving pays us.** Reserve, let the other side trade, then execute: the
  stitch must come out ahead, never behind.
- **Order independence.** `averageShade(a, b) == averageShade(b, a)`.
- **Clamp-then-price.** A request larger than capacity prices the stretch of the clamped size,
  not the requested size.
- **`allIn` tracks `fee_bps`.** A request with a different `fee_bps` moves the endpoint.
- **Flat fallback.** A quote with no `reference` answers at the configured spread and logs.

## 12. Rollout

1. **The feed field** (`reference` with `mid` and `halfRangeBps`) rides `/api/price` and
   `pricing/feed.rs`. Backward compatible; existing bots ignore unknown fields.
2. **Preview.** Turn the curve on (panel: Corridors → Experimental; by hand `modules_enabled`
   plus `[modules.curve] enabled = true`). Modules start in Preview: the curve price is computed
   beside the flat one, the flat one is answered, and both are logged as `curve quote` lines and
   recorded in the module status, which the panel's Modules tab shows (holdings, the curve now,
   every quote and every flat fallback with its reason).
3. **Observe for several days.** Check the `f` trajectory is sane and the curve lands where the
   model says. Compare against the *adjusted* live quote, since the manual adjustment is what the
   curve is replacing.
4. **Switch modules to Live.** One step on the bot: it moves to `unadjustedPrice` and the curve
   together (§10). The manual adjustments don't have to change.
5. **KPIs and a rollback trigger, agreed before step 4.** This is a replacement, not an A/B, so
   there is no control group. Watch naira share, naira clearing time, volume, and **quote win
   rate** — the share of answers the taker executes, which is the natural competitiveness
   measure when every price is quoted on request. Roll back if volume drops materially or naira
   climbs.

   Watch win rate **per counterparty** as well as in aggregate: large requests with a low hit
   rate is the signature of someone parking quotes to block a side (§8).

Volume is concentrated: 96 fills over $100K were half of a month's volume. Daily noise will be
large and a short test will not be conclusive.

## 13. Which corridors

Not "does it accumulate" and not "is the price clear". Those collapse into one test:

```
wide reference   -> you do not really know the price AND dumping is expensive  -> curve
tight reference  -> you know the price AND dumping is cheap                    -> hedge, dump
```

The curve's lean room is half the reference width and the cost of dumping is also half the
reference width. Same number. **The curve is worth having exactly where dumping is not.**

- **cNGN qualifies** on both counts. The Bybit P2P reference runs about 19 bps wide and is
  crossed roughly a third of ticks, so there is no single observable price and clearing inventory
  as a taker is expensive.
- **IDRX does not.** The reference is tight, and the corridor is not accumulating anyway: net
  IDRX flow is −9.89bn against +$550,089 of USDT and the IDRX balance is zero. A corridor that
  never builds inventory has nothing for a curve to shed.
- **wBRL is open.** It holds $106,181 off $227,538 of lifetime volume, a worse inventory ratio
  than naira, but nobody has measured the wBRL/USDT reference width. If it is tight, hedge
  instead.

## 14. Measurements behind this

Taken 25 Oct 2026. **Re-measure before trusting any of them; the market moves.**

| | |
|---|---|
| Bybit cNGN reference (1M NGN probe) | ask 1,378.99 / bid 1,376.40, 19 bps wide |
| Reference crossed at the top | roughly 1 tick in 3 (per `bybitReference.ts`) |
| BSC stitch balances | cNGN $243,719 + USDT $60,264 |
| Naira share | 82% on BSC, 67% counting Celo USDT |
| BSC cNGN turnover | 330x since launch, 1,004x over 30d, 1,437x over 14d |
| Ticket mix | 93% of volume is tickets over $20K; 64% of fills are under $3K and are 2% of volume |

Bybit's cost by ticket size, bps over its own mid: $1K +3.4, $3K +7.4, $20K +9.8, $50K +10.2,
$100K +10.7. The stitch beats all of these from a low `f`, which is why a tight spread at every
size is a real edge rather than an accident.

**One caveat on the reference width.** A later reading put the live Bybit book at 5.65 bps wide,
not 19, which cuts max shade from 11.4 to 4.8 bps and moves the cheapest-dollars line from 81%
naira to about 41%. The curve re-sizes itself automatically, so this is not a correctness
problem — but it does change how much the curve is worth. **Measure the width distribution off
`cngnPricingReport` before concluding the curve earns its keep.**

## 15. Open questions

- **Is a 1 bps spread viable against a one-minute feed?** Polling the served centre for 30
  minutes on 25 Oct gave 31 updates: median move 0.00 bps, mean 1.84, but **33% moved more than
  2 bps and 13% moved more than 5**. The whole all-in spread is 2 bps. n=30 in one window, so
  indicative only — but it says a counterparty watching live Bybit can take the stale side of a
  quote on a meaningful fraction of minutes, and nothing here prices that. The curve makes it
  more consequential: a loaded stitch quotes further from the mid, leaving more room to be picked
  off. Separate from the curve and it applies to flat quoting just as much. Measure off
  `cngnPricingReport`, which holds every minute's Bybit mid going back days.

- **`spreadIncomeUsd` is wrong, and it blocks measuring any of this.** Every corridor reads
  negative, clustered at −5 to −6 bps, across four chains and five unrelated assets (BSC cNGN
  −3.79, Celo IDRX −5.26, Celo wARS −5.84, Celo wBRL −5.90, Ethereum XAUt −5.88). Gold and naira
  do not lose money the same way, so this is a metric bug, not trading. Unrelated to the curve,
  but it has to be fixed before step 5's KPIs mean anything.

## Decision log

Resolved 25 Oct 2026 (Tomer):

- **Anchor is the raw Bybit mid**, served as its own field — not the adjusted centre and not
  whatever the alignment toggle says. (The manual adjustments were to go to zero at cutover;
  superseded on 26 Oct, see below.)
- **`f` is wallet balances only**, per stitch, per corridor, per chain.
- **A missing half-range falls back to flat quoting**, not silence.
- **The fair-crossing invariant is dropped** on the curve path rather than implemented twice.
  This is a pricing mechanism; quoting above the mid when loaded is the point of it.
- **Cross-chain cNGN is several separate inventories**, not one. Academic today (99.9% of the
  cNGN is on BSC) but it fixes the shape of `f`.
- **Both quote rules stay in the code**, selected per corridor. The existing band lean is not
  deleted: Stitch ships open source and an external operator may be running it on a corridor we
  do not control.

Resolved 26 Oct 2026:

- **Rebalances do not wait for outstanding quotes.** The staleness it would buy off is ~2 bps on
  a rare overlap; the wait would let a counterparty freeze the stitch's capital by rolling
  quotes. Solvency is already covered by reservations, so nothing is at risk but a couple of
  bps. See §8.
- **The curve is defined by two NGN knobs** (§3), not by a formula with a fixed endpoint. At
  0 / 0 it sells dollars at the Bybit mid when empty of naira and pays the Bybit buy when full.
  The shade therefore starts at `-allIn`, not at zero, and the span is
  `bybitHalfRange + 2 x allIn`.
- **Price follows settled swaps and this side's own outstanding quotes** (§8), never the other
  side's. Cross-side propagation was measured at 4.47 bps of free money for a taker; same-side
  only is worth 0.000.
- **A rejected alternative: capping total outstanding quoted size** instead of reserving the
  stretch. It bounds the error below the spread rather than removing it, so two quotes at once
  are still both mispriced. Reservation is exact; the cap was not.
- **Stitch pricing is the only channel this targets.** The resting book keeps its band lean
  (`pricing/lean.rs`); the curve never reaches it.
- **Config lives in the module**: `[modules.curve]` with `enabled`, the two NGN knobs
  (`start_below_mid`, `end_above_buy`) and `shape`, run by `[modules].mode`. `allIn` is derived
  per request, so it is not config. The formula has no floor term, so the band rule's
  `lean_floor_bps` has nothing to clamp here.
- **The curve is a module for wallet bots and vaults alike** (§8): one config home, one mode
  switch, one status file and one panel workspace. On a vault it reads the free balances and
  replaces inventory balancing; dynamic spreads stay compatible.
- **The manual adjustments never touch a curve bot** (§10). A Live curve reads
  `unadjustedPrice` for everything it quotes, so cutover is one switch on the bot and the knobs
  keep serving the centre's other consumers. The alternative, a curve that stops leaning while
  a knob is set, would make the knob a remote off switch for the curve.
- **A fill releases its claim on the next balance read** (§8), read on-chain as the order's
  spent Permit2 nonce in the balance batch, not waited out to the deadline or taken from a venue
  frame: the venue learns of fills on a one-minute sweep in another service, and the bot would
  still have to line its frame up with its own balance block.
- **The level slack is the corridor's own**, sent in the session handshake, not the venue's
  default assumed. A corridor set below 5 would otherwise draw strikes, one set above it lost
  quotes.
- **The feed owns the reference book's quality** (§9, §10): the half-range is the API's median
  over uncrossed books, not each tick's own width, and the field is source-agnostic. The bot
  keeps its own 1% check against the guarded print as a second line, since it can be pointed at
  any feed.
