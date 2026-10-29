# cNGN Profit unwind: stitch pricing spec

**Status: built on the quote curve, 29 Oct 2026 (`[modules.profit_unwind]`,
`src/pricing/unwind.rs`).** It's an add-on to the quote curve (`docs/cngn-quote-curve-spec.md`);
flat + Profit unwind is designed below and follows separately. It's off by default and opt-in per
bot, and it previews (`live = false`) until switched to quote.

## 0. An additional strategy, not a replacement

Profit unwind is an **add-on** a stitch can switch on. It doesn't replace either existing strategy,
and nothing about them changes. It works on top of **either** one, so there are four choices:

| Strategy | Prices by | When to pick it |
|---|---|---|
| **Flat** | the reference, plus a fixed spread | Inventory isn't a problem. Every corridor today except cNGN |
| **Flat + Profit unwind** | the flat price, moved by what its naira was worth when it came in | No constant lean wanted, but clear naira when the market pays for it |
| **Curve** | how much naira the stitch holds | Inventory builds up on one side and has to be leaned against |
| **Curve + Profit unwind** | how much it holds **and** what that naira was worth when it came in | As above, plus clear the pile faster when the market moves your way |

Turn Profit unwind off and the stitch quotes exactly its base strategy, flat or curve. Each stitch
picks its own combination, per corridor.

**On the curve,** the lift is an extra shade on the curve (§3). The gain is measured against **our
own feed** (the price before manual adjustments, the one a Live bot quotes off), not the P2P
reference the curve anchors on: cNGN is always valued off our feed.

**Built first on the curve only (28 Oct).** Flat bots don't read their balances today, so flat +
Profit unwind follows as its own change.

**On flat,** the lift moves the flat centre, so both sides move together and the spread is
unchanged. The gain is measured against the price the flat bot already quotes off. A flat price has
no share to average over, so flat + Profit unwind needs its own answer to §5 before it ships.

`f` comes from the bot's balance reads in both cases. Flat RFQ bots already read their balances to
cap quotes.

## 1. What and why

The quote curve knows **how much** naira the stitch holds. It doesn't know **what that naira was
worth when it came in**.

Say the stitch is long naira that came in while the market sat around 1386 NGN per USDT, and the
market moves to 1380. The naira is now worth more dollars, about 43 bps more. Today the whole curve
slides down to 1380 with the reference, so that gain goes unused. The stitch keeps quoting as if
nothing happened, and the pile only clears at whatever pace the curve already had.

Profit unwind uses the gain. While the market sits on the profitable side of that cost, the curve
slides down **less than the market does**, so the stitch pays a little more naira per USDT than the
reference. Takers who bring USDT get a better rate, the naira clears faster, and the stitch gives
back part of the gain to buy that speed.

In absolute numbers the stitch's price still goes **down** with the market. It just goes down less.

It only ever acts on a gain from the market moving. When the market moves against the naira the
stitch holds, profit unwind is zero and the curve behaves exactly as it does today. There is no
"wait until we break even" logic, because that is how a maker ends up stockpiling losing inventory.
And the spread the stitch earns on each fill is its own: it's never counted as gain, so it's never
given back (§4).

## 2. Words

All prices are NGN per USDT, the unit the curve works in (curve spec §2).

| Term | Meaning |
|---|---|
| `mid` | our feed's price, NGN per USDT, read on every price update (in Live, the price before manual adjustments) |
| `f` | naira share of the wallet, the same `f` the curve uses |
| `N` | naira the stitch holds on this corridor |
| `B` | what that naira was worth in dollars when it came in (its cost basis) |
| `C = N / B` | average cost, in NGN per USDT |
| `g` | gain on the naira held, in bps: `(C / mid - 1) * 1e4`. Positive when the naira is worth more than it was |
| `L` | the full lift, in NGN per USDT, at `f = 1` |

## 3. The rule

```
C_s = average cost from the snapshot (§5)
L   = 0                                         if (C_s - mid) < min_gain_ngn
L   = min(cap_ngn, share * (C_s - mid))         otherwise, NGN per USDT
lift(f) = L * w(f)
w(f)    = 0                                     if f <= start
        = (f - start) / (1 - start)             otherwise
start   = start_pct / 100
```

`lift(f)` is **a shade on top of the curve's own**: at share `f` the curve's shade is
`s0 + (s1 - s0) * f^shape + lift(f) / refMid * 1e4` (curve spec §3). The lift stays in NGN until
the curve turns it into bps against its own reference mid, the one its shades apply to, so `cap_ngn`
is exactly the most the centre moves in NGN, however far the feed sits from the reference (up to
the 1% the curve allows). At any one share both sides of the quote
move up by the same `lift(f)`: the stitch pays more naira for USDT and asks more naira for USDT.

A quote pays the lift's **average over the stretch of share it walks**, exactly like the curve's own
shade (curve spec §4). `w` is piecewise linear, so the average has a closed form: with
`W(f) = (f - start)^2 / (2 (1 - start))` above the start and 0 below,
`avg = (W(hi) - W(lo)) / (hi - lo)`. Out and back over the same stretch integrate to the same
number, so the lift cancels and a round trip nets exactly minus the spread, whatever the share did
in between, before or after a refresh. Everything else downstream is unchanged: same-side
reservation (§8 of the curve spec), clamp-then-price (§6), the venue fee handling.

**The ask floor.** The lift may raise the ask (the stitch selling naira) towards `C_s` but never
past it: `ask = min(lifted, max(plain, C_s))`, on the order price (spread only, before the venue's
fee). So no ask the lift moves pays out more naira per dollar than the naira was worth when it came
in. A plain curve that already sells under cost near full is the curve's choice and is left alone;
profit unwind just adds nothing there. The floor only touches the ask, so it can only widen the gap
between the two sides.

**The round-trip limit.** Out and back at one lift cancel, but the lift can fall between a taker's
legs: naira they sell back enters the basis at the feed and pulls `C` (and so `L`) down, at once.
Tomer's three legs show it: buy most of the naira at the full lift, sell part back near empty
(where `w` is small, so it costs almost no lift) to dilute `C`, then sell the rest back at the
lower lift. The taker keeps about `(L1 - L2) * mean(w)`, and draining the whole book averages `w`
to `(1 - start) / 2`. So when each quote is priced, the full lift is held to

```
L <= 4 * spread * mid / (1 - start)
```

where `spread` is the corridor spread (the signed order's half-width) and `mid` is the curve's
reference mid. That keeps `L * (1 - start) / 2` within `2 * spread`, the one cost every round trip
is sure to pay. The venue fee isn't in it: it arrives per request, and the legs of a round trip
needn't pay the same one, so a dear first leg and cheap returns would otherwise leave room. At a
1 bps spread and a 1,380 mid the limit is about 0.55 NGN at start 0 and 1.1 NGN at start 50, so
there the default 2 NGN cap is cut to 1.1. A larger `cap_ngn` is cut to the limit when quoting; the
panel shows the limit next to the cap and won't save a cap above it at the current reading.

**Why both sides move, never only the side that pays for USDT.** The two sides sit only about 0.55
NGN apart (twice the all-in). Raise only the price the stitch pays for USDT by 1.5 NGN and it pays
more for a dollar than it sells one for. A taker then buys USDT from the stitch and sells it straight
back, keeping about 1 NGN per dollar in a loop. Moving both sides keeps the gap, so only one-way flow
happens: takers bringing USDT and taking naira. Example: naira that came in at 1386 and sold at
1381.25 banks about $3.40 per $1,000.

**Why the lift follows the share, averaged, and not one number for the whole curve.** An earlier
draft added one `Δ = w(f_s) * L` to both curve ends, with `f_s` frozen in a snapshot. Within a
snapshot that's safe, but the snapshot refreshes: a taker sells naira (raising `f`), waits for the
refresh to pick up the higher `f_s`, and buys it back at a bigger `Δ`. With a steep start the jump
in `Δ` beats the round-trip spread. Pricing `lift(f)` along the stretch, like the shade, makes out
and back cancel at any time, so the share needs no freezing at all.

**Why "Start profit unwind at".** A gain on a small naira position shouldn't move the whole price
much, and a balanced book doesn't need help unloading. Below the start share nothing happens. Above
it the lift grows evenly to full at 100% naira, instead of switching on all at once, so crossing the
start never jumps the price. At 0% it grows with the naira share from the first naira.

**Why cap it in NGN.** The cap is the most the stitch will ever pay over the curve. It's written in
NGN so it reads like the curve's own knobs.

### Worked example

The stitch holds 313M naira at an average cost of 1386. The mid moves to 1380.

| Step | Value |
|---|---|
| gain `g` | (1386 / 1380 − 1) × 1e4 = **43.5 bps** (6 NGN) |
| `share × gain` at share 0.5 | 3 NGN |
| cap at 2 NGN | the cap binds: `L` = 2 NGN |
| `w(0.75)`, start at 0% | 0.75 |
| **`lift(0.75)`** | 0.75 × 2 = **1.5 NGN, about 10.9 bps** |

At 75% naira the stitch quotes about 1.5 NGN above where the plain curve would at 1380. It keeps
about 4.5 NGN of the 6 NGN move and spends 1.5 NGN of it to clear naira faster. A quote that walks
the share from 0.75 down to 0.70 pays the average over that stretch, a touch under 1.5 NGN.

With **Start profit unwind at 50%**, `w(0.75) = (0.75 − 0.5) / 0.5 = 0.5`, so the lift there is 1.0
NGN, about 7.3 bps. At 50% naira or less it is 0.

## 4. How the stitch knows what its naira was worth

The bot keeps `N` and `B` itself, in a small state file next to `rfq-reservations.json`
(`profit-unwind-basis.json`, one entry per pool). It **doesn't** depend on the Textile API or the maker
ledger. Stitch is open source, and a bot run by anyone has to work alone.

It updates on every balance read, from the change in the **naira** balance since the previous read:

| Change between reads | Update |
|---|---|
| naira up (a fill, a deposit, anything) | `N += n`, `B += n / mid` (marked at the feed of that read) |
| naira down (a fill, a withdrawal, anything) | `B *= (N - n) / N`, then `N -= n` (out at average cost, so `C` doesn't move) |

Notes on that table:
- **Every naira in enters at the feed**, fill or deposit alike. The stitch's own spread on a buy is
  never counted as gain, so it's never handed back, and topping up the wallet never invents a gain.
- **A taker can only pull `C` towards the feed.** Naira a taker sells in lands at the feed, diluting
  any gain; naira a taker buys out leaves at average cost. Neither can raise the gain.
- **Only the naira balance matters.** The dollar side (this bot's fills, a sibling bot on the same
  wallet, gas) isn't read, so it can't skew the basis.
- **Selling at average cost** keeps `C` fixed when naira leaves.

**First run, a missing or corrupt file, or a basis that isn't this bot's:** start with `C = mid`, so
`g = 0`. Profit unwind stays off until the market moves away from that point. That's the safe way to
fail: it can only lose a gain, never invent one. Each saved entry carries the chain, the wallet or
vault it was read from, the pair, the feed price it was marked against (Preview and Live read
different ones) and when it was saved. A restart carries it on only for the same holder, and only if
it was saved within twice `refresh_secs`. A basis that sat unwatched longer (profit unwind switched
off, the bot down) may not describe what's held now, so it starts over.

**Vault bots** use the vault's free balances, the same reading the curve uses (curve spec §8, "On a
vault").

## 5. The snapshot

The share needs no freezing: the lift is priced along the stretch (§3), so a taker moving `f` moves
their own two legs the same way. The gain needs none either: no taker moves the feed, and no taker
can raise `C` (§4).

The average cost `C_s` is still frozen in a snapshot, refreshed every `refresh_secs` (default 900),
so the lift doesn't churn on every fill. One exception: naira that comes in and pulls the cost down
lowers `C_s` at once (on the same refresh schedule), so the ask floor never sits above what the
naira now held is worth. Holding nothing clears it. A higher cost waits for the refresh. The gain is
measured against the **live feed on every price update** (decided 28 Oct). A snapshot older than
twice `refresh_secs` prices nothing (§7).

Firm quotes fold in the latest balances before they price, not just the levels tick, so a quote
never sits on a cost that naira already in the wallet has pulled down. And signing can take a
while (custody, MPC and Safe signers), so after signing an ask the bot reads the balances again and
withholds the signature if the cost it priced on has fallen since. Bids aren't rechecked: a stale,
higher lift there only asks the taker for more naira.

**Required test, and it's in:** `a_round_trip_across_a_profit_unwind_refresh_never_pays_the_taker`
in `src/rfq/responder.rs`. It runs taker-realized round trips through `decide_quote_curved`, with
the venue fee on both legs and the snapshot refreshing between them: both directions, sizes up to a
third of the wallet, naira shares from 55% to 97%, starts at 0, 50 and 90%, gains up to 20 NGN, the
default reference book and a 30 bps one with `end_above_buy = 2`. The worst case costs the taker
about 4 bps, twice the all-in. Pricing the lift at the share a quote starts from instead of over its
stretch fails it at about +6 bps. Next to it,
`a_drain_dilute_return_round_trip_never_pays_the_taker` runs Tomer's three legs (drain, dilute,
return across a refresh) at caps up to 10 NGN, starts at 0, 30 and 50%, and venue fees of 1/1, 10/1
and 25/1 bps on the way out and back; with the round-trip limit the best case for the taker is about
-2.1 bps. Without it, cap 2 at start 0 paid the taker +2.5 bps, and a limit that counted the fee
paid +10.6 bps at a 25 bps first leg.

## 6. Knobs

These live in their own block, `[modules.profit_unwind]`. The lift only quotes with `live = true`
and a Live curve (`[modules].mode`).

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Off unless switched on. Needs `[modules.curve]` on |
| `live` | `false` | Quote with the lift. Off, the bot tracks the cost, computes the lift and logs the price it would quote, and quotes the plain curve |
| `share` | `0.5` | Share of the gain passed back as a better price. Allowed range 0–1 |
| `cap_ngn` | `2` | The most the curve ever lifts for profit unwind, in NGN. Held to the round-trip limit (§3) when quoting |
| `min_gain_ngn` | `0.7` | Ignore gains smaller than this, in NGN per USDT (about 5 bps), so feed noise doesn't move the price |
| `start_pct` | `50` | Start profit unwind at this naira share, in %. Below it, no lift; above it the lift grows to full at 100%. Allowed range 0–99 |
| `refresh_secs` | `900` | How often the cost is frozen again (§5). TOML only, not on the panel |

Validation, mirroring the curve:
- the share must be within 0–1;
- the start must be within 0–99%;
- the cap and min gain must be 0 or more;
- refresh must be at least 60 s;
- it needs the curve on (v1 is curve only, §0).

All the keys go into the parameter change log, like `modules.curve.*`.

The panel's curve editor shows the current average cost, the feed, the gain, and the lift at full
and at today's share. The curve it draws includes the lift once it quotes. Its maths is pinned to the
bot's by `tests/unwind_parity.rs`, which writes `web/src/unwindParity.json` for the web tests.

## 7. Degraded inputs

| Case | Behaviour |
|---|---|
| No basis yet (first run, missing, corrupt or someone else's file) | `C = mid`, no lift (§4) |
| Gain below `min_gain_ngn`, or negative | No lift |
| The curve falls back to flat (curve spec §9) | Flat means flat. No lift |
| Snapshot older than 2 × `refresh_secs` (bot paused, clock jump) | No lift until the next refresh |
| `N = 0` | No lift |
| A stale or `degraded` feed | The tick refuses it before the basis sees it, so nothing is marked at it |

## 8. What we accept

- **Selling into a trend.** If the naira keeps strengthening, profit unwind clears the pile early and
  misses part of the move. That's the point of it: certain profit now in exchange for maybe more
  later. `share` and `cap` set how much we give up.
- **Naira only.** The mirror case (long USDT, and the naira weakens so the USDT is worth more) uses
  the same maths with the sides swapped. It's left out of v1 until the naira side proves itself.
- **Two reads merge.** Several moves between two balance reads net into one, so a buy and a sale in
  the same window cancel before the basis sees them. Both would have been marked near the same feed
  anyway.

## 9. Acceptance tests

1. **Round trip** (§5): negative everywhere with profit unwind on, including across a refresh.
2. **Basis maths:**
   - naira in, fill or deposit, enters at the feed;
   - a sell doesn't move `C`;
   - a withdrawal is proportional;
   - a taker's trade can only pull `C` towards the feed.
3. **Profit side only:** a negative gain gives no lift at any `f`.
4. **Cap:** the lift never goes above `cap_ngn`, whatever the gain or the gap between the feed and
   the reference.
5. **Snapshot:** a fill between refreshes doesn't raise the next quote's cost; naira that dilutes
   it lowers it at once.
6. **Restart:** the basis survives a restart for the same holder within twice the refresh, and
   anything else starts at `C = mid`.
7. **Ask floor:** no lifted ask pays out more naira per dollar than `C_s`.
8. **Taker-realized prices:** tests read what the taker actually receives, fee included (curve spec
   §11). That's the lesson from the curve's fee double-count.
9. **Golden values** shared with the panel, accurate to 1e-9 (`tests/unwind_parity.rs`).

## 10. Rollout

1. **Simulator first.** Add an average-cost input and the profit unwind knobs to the curve simulator,
   and show the lifted curve next to the plain one. Tomer checks the numbers before any code.
2. **Preview** with `enabled = true`, `live = false`. The bot computes the lift and logs it but
   quotes the plain curve. Each curve quote's log line carries `unwind_cost`, `unwind_full_ngn`,
   `unwind_live` and `unwound` (the price it would have quoted) next to `curved`. Run for a few days on
   the cNGN BSC bot that runs the curve; its curve can stay Live.
3. **Live** with `live = true`, share 0.5 and cap 2 NGN.
4. **Watch:**
   - the naira share trend;
   - realized gain on naira sold;
   - USDT brought in versus taken out;
   - quote win rate.

   Roll back (`live = false`) if the naira share doesn't fall within a week, or if a lifted ask ever
   pays out more naira per dollar than the cost it was sized on. The floor makes the second case
   impossible, so seeing it would mean a bug.

## 11. Open questions

1. ~~Scale by `f`, or only above an inventory floor?~~ Settled 28 Oct: one knob, **Start profit
   unwind at** (%), suggested 50 (§6).
2. **Refresh cadence.** 15 minutes is a guess. With the share priced along the stretch and the cost
   only able to fall towards the feed, it no longer guards a round trip, so shorter is fine.
3. **Cross-check against the maker ledger.** The bot's own basis is the source of truth, but the
   dashboard could show the ledger's view next to it, to catch drift.

## Decision log

- **29 Oct 2026 (review of #2817, Benoit):**
  - naira in enters the basis at the feed, fills included, not at the price paid. Marking fills at
    the price paid counted the stitch's own spread and shade as gain, handed part of it back, and
    let a taker profit across a refresh;
  - the lift is `L * w(f)` priced along the stretch like the curve's shade, not one `Δ` frozen at
    the snapshot share, so a taker raising the share before a refresh gains nothing;
  - the ask is floored at the cost, so the "never sells under cost" claim holds by construction;
  - the lift is carried in NGN and turned into bps against the curve's reference mid, not the
    feed, so `cap_ngn` holds when the two differ;
  - its own `live` switch, so it previews on a curve that's already Live;
  - a saved basis carries the holder and when it was saved, and is only picked up by the same holder
    within twice the refresh;
  - naira that dilutes the cost lowers the frozen cost at once, so the ask floor never sits above
    what's held;
  - the full lift is held to `4 * spread * mid / (1 - start)` when quoting (Tomer's bound), so a
    taker who drains, dilutes and returns can't beat the round trip. The spread, not the all-in:
    the venue fee is per request and the legs needn't pay the same one. Enforced per quote with
    the live mid, and shown and checked in the panel;
  - a buy-out and sell-back inside one balance read leaves the basis unchanged: the position didn't
    change and the bot earned the spread. Booking each fill would make the lift drop after every
    round trip, the lever above;
  - firm quotes refresh the tracker before pricing, and an ask whose cost fell while it was being
    signed is withheld.
- **28 Oct 2026 (Tomer):** named the strategy **Profit unwind**, an additional strategy, not instead of the curve or flat, and available on top of either one (§0). Earlier the same day:
  - add a price-move parameter, so that when naira moves our way the stitch takes profit and
    rebalances;
  - profit side only;
  - spec before simulator, simulator before code.
  - the knobs Tomer sets are in NGN, like the curve's start and end. The minimum gain moved from
    `min_gain_bps = 5` to `min_gain_ngn = 0.7`; `share` stays a fraction.
  - "Scale by" (naira share vs only above 50%) was too complicated. It's replaced by one knob,
    **Start profit unwind at**, in % of naira share: off below it, growing to full at 100%.
- **28 Oct 2026 (design):**
  - the basis comes from the bot's own balance reads, not the API, because Stitch has to work
    standalone;
  - the cost comes from a 15-minute snapshot;
  - deposits enter at the mid;
  - first run starts with zero gain.
