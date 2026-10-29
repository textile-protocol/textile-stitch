# FX protection modules

Modules are first-party Stitch policies. Most of them are for an OperatorVault; the inventory curve also runs on a bot that trades from its own wallet.

This guide covers the quote-policy modules in **Modules**. Each module opens in the same workspace as FX hedge. [FX hedging](fx-hedge.md#running-modules-together) uses separate settings and can run beside it, including the inventory curve on IDRX. Changing one group's mode or feature flag does not change the other.

Enable quote modules with one **root property**, before any TOML table:

```toml
modules_enabled = true
```

In the panel, a bot that trades from a vault has **Corridors → Experimental → Turn on modules**; a bot that trades from its own wallet has **Turn on inventory curve** in the same place, which sets the same flag and switches the curve on. It sets the flag for you, always in Preview, even if modules were last saved in Live. It also turns off what they can't run alongside: the public ladder, the taker leg, the closer, lean and TWAP (TWAP has no switch, so its window and deviation are removed). Those stay off if you turn modules off later. What it can't fix for you (a second corridor, RFQ off, or no Textile connection) is listed before you try, and **Turn off modules** sets the flag back.

It defaults to false. With the flag absent or false, there is no quote-policy runtime, decision recorder, dealer traffic or quote-module API access. Existing quotes use their existing prices and sizing. The Modules tab remains available if FX hedging needs it, including while disabled hedge positions or obligations still need monitoring.

Enabling the flag opens the quote controls in Modules and starts in **shadow** mode. Shadow records proposals while preserving existing quoting. Choose Live and save to apply enabled policies. Saving uses the normal validated config/restart flow; a stopped bot stays stopped. Off stops new actions from this group. Previously signed orders remain reserved and executable until expiry, including after the quote-module flag is disabled.

Modules quote one RFQ corridor per bot. On a vault, the pool's collateral must be the vault's corridor asset and its debt the settlement asset. Without a vault only the inventory curve runs: inventory balancing, dynamic spreads, spot rebalancing and the liquidity schedule read the vault's NAV and act through it, so on a wallet bot they stay off whatever the file says (the bot logs which at startup) and the panel doesn't show them. Disable legacy lean, TWAP, ladder, limit-taker and closer settings. Use one module decision owner per vault: exposure limits are local strategy policy, not a new on-chain invariant across independently operated bots.

## Strategies

**Inventory balancing** measures corridor value as a fraction of free vault NAV. Settlement deployed in the existing yield adapter counts toward NAV. Pending deposits and reserved redemption payouts are excluded by the vault's free-balance views. Spendable amounts still come from the existing quotable inventory path, with reserves, pauses, close-only rules and per-order caps intact.

Above the target, buying more corridor currency becomes less attractive and selling it becomes more attractive. Below target, the skew reverses. The configured spread floor bounds tightening. Buys stop at the maximum share; exact post-trade arithmetic also checks the limit. An outstanding inventory-increasing quote blocks another buy until its reservation is released. A proposed or unconfirmed sale never reduces measured exposure.

**Dynamic spreads** adds a bounded spread based on the high/low range of observed prices in a rolling window. It uses source timestamps already received, not future data. Until the window spans the warmup interval it adds the maximum (`max_extra_bps`) rather than pulling quotes, and it rejects stale or future prices. A price the feed flags `degraded` (a held, clamped or fallback value) is never added to the window, and it blocks every module decision until the feed is live again. The window is saved to `modules-history.json` beside the config, so a restart (every module save is one) resumes warm. Samples from a different feed URL are never reused. By default, the same extra spread is added to both enabled sides after inventory skew. The same decision changes indicative levels and firm quotes.

Enable **Favor inventory reduction** under Modules → Dynamic spreads → Settings, or set `modules.spreads.inventory_aware = true`, to reduce the volatility buffer on trades that move holdings toward the inventory target. Inventory balancing must also be enabled; otherwise the additions remain equal. Existing configurations default to false, so upgrading does not change their quotes.

Above target, buys retain the full volatility buffer. The sell buffer falls linearly with exposure, reaching zero at the maximum share, where buys remain paused. Below target, sells retain the full buffer and the buy buffer falls toward zero at zero corridor holdings. At target the additions are equal. The discount is rounded down to whole bps: `extra * min(distance_from_target, band) / band`, with `band = max - target` above target and `band = target` below. Each side stays within `max_extra_bps`. This does not reduce the inventory-adjusted spread itself, so its floor, disabled sides, reservations and purchase limits remain in force. `max_skew_bps` still bounds the inventory module's adjustment; volatility weighting only changes its own extra spread.

For example, with target 30%, maximum 60% and current inventory 45%, a 20 bps volatility buffer adds 20 bps to buys and 10 bps to sells. At 60% or more, it adds nothing to sells and purchases stay paused. This favors reducing excess exposure; it does not predict the currency's direction or guarantee a buyer.

**Spot rebalancing** defaults to a manual sale. Stitch suggests selling when corridor exposure reaches its threshold. In Fleet, choose the buyer wallet, exact corridor amount and minimum net settlement proceeds. The buyer completes the swap through Textile. Sales remain bounded by excess above target, the NAV fraction, spendable inventory, price protection and vault limits. A manual sale can be requested below the suggestion threshold, but never below the inventory target.

```toml
[modules]
mode = "shadow" # off | shadow | live

[modules.inventory]
enabled = true
target_bps = 3000
max_bps = 6000
max_skew_bps = 50
spread_floor_bps = 5

[modules.spreads]
enabled = true
inventory_aware = false # true weights the buffer when inventory balancing is enabled
window_secs = 300
warmup_secs = 30
multiplier = 1.0
max_extra_bps = 100

[modules.rebalance]
enabled = false
method = "manual" # default; dealer requires explicit configuration
trigger_bps = 5000
max_trade_bps = 200
max_slippage_bps = 50
cooldown_secs = 300
order_lifetime_secs = 60
```

Numbers are starter configuration, not recommendations for a particular currency. Measure feed quality, market depth and trading costs before selecting live parameters.

## Inventory curve

The inventory curve leans Swap quotes on the bot's own share of the corridor currency (cNGN first), the share it holds of everything it holds, valued at the feed's reference mid. With none, the bot sells dollars at the reference mid; full of it, it pays the reference buy, what clearing the currency on the reference market would cost. In between, the lean grows with the share raised to `shape` (2 by default: it stays near the mid through normal holdings and leans hardest near full). The two offsets move either end, in the soft currency (NGN for cNGN), and are measured from the reference book on every quote: positive widens the curve, negative narrows it (starting above the mid, ending below the buy). The curve only goes flat once negative offsets together take off the reference half-range plus the round-trip spread (twice the spread and fee), where the de-spread centres of the two ends cross. Smaller cuts can already put the empty-side sell above the full-side buy, and the curve still leans the right way: at any one share the bot's buy stays a round trip under its sell. Flat means one centre halfway between the two, never backwards; it narrows into that smoothly, so a half-range that wobbles around the crossing never jumps the price.

Each quote pays the average lean across the stretch of holdings that trade moves the bot through, not the lean where it starts, so a trade out and straight back always costs the taker the spread, at any size. Quotes already given out on one side count as moved for the next quote on that side (never the other side), so splitting a trade buys nothing. In Live the levels the venue routes on are priced off the same curve, eight rows a side, so firm replies stay inside the corridor's level slack (the venue sends each corridor's `levelSlackBps` when the session opens); a reply that would still land past it, after a large same-side quote or answered flat against curved levels, is declined rather than sent. A quote that fills stops counting as given out on the next balance read, not at its deadline: the bot reads the order's Permit2 nonce in the same Multicall3 batch as the balances, so the release and the balance that shows the trade come from one block. The full design, the attacks it closes and its decision log are in [the curve spec](cngn-quote-curve-spec.md).

```toml
[modules.curve]
enabled = false        # on in Preview from the panel; Live with [modules].mode
start_below_mid = 0.0  # NGN below the reference mid, holding none (negative: above it)
end_above_buy = 0.0    # NGN above the reference buy, holding nothing else (negative: below it)
shape = 2.0            # 0.5 to 4
```

**What it reads.** On a wallet bot, the wallet's raw balances (not what Permit2 lets it promise: an unapproved leg is still held). On a vault, the vault's free balances (`freeCorridor`, `freeSettlement`): NAV with staked settlement counted, pending deposits and reserved redemption payouts left out, the same reading inventory balancing uses, and only while it is under three seconds old. The reference book comes from the feed's `reference` field (`mid`, `halfRangeBps`): the raw reference mid before Textile's manual centre adjustments, and half the book's width as the median of the last 15 minutes of uncrossed books, so a crossed tick doesn't move it.

**What it needs.** Equal bps spreads on both sides (one curve serves both directions; unequal or absolute spreads would reopen the round trip), a feed the right way round (collateral is the soft currency; a reversed pool's `invert=1` feed is refused), and on a vault, inventory balancing off: both lean quotes on what the vault holds, so it's one or the other. Dynamic spreads can stay on, since they add the same buffer to both sides and the curve only sees a wider all-in. Spot rebalancing and the liquidity schedule don't touch prices and run alongside. A wallet bot running the curve that connects a vault keeps the curve: the connect writes inventory balancing off, since it defaults on and never ran on the wallet.

**When it answers flat.** With no reference book on the feed, a reference more than 1% from the jump-guarded print, no fresh balance reading, or nothing held, the quote goes out at the corridor spread and the bot logs why. The Overview shows how many recent quotes went out flat and Activity lists each one with its reason.

**Going Live on a Textile corridor.** Textile's served `price` carries the admin's manual centre adjustments, the hand-made version of this lean. In Live the bot reads `unadjustedPrice` instead, the same observation before those adjustments, for everything it quotes (flat fallbacks and the jump guard included), so the knobs never reach a curve bot and going Live is one switch. Preview keeps the served price, since that's what it answers at. The knobs keep moving every other consumer of the centre.

## Profit unwind

Profit unwind rides on the inventory curve. The bot keeps an average cost for the corridor currency it holds (NGN per USDT) from its own balance reads. Every naira that comes in, bought or deposited, enters at the feed of the read it lands in, and every naira that goes out leaves at the average cost. So the cost is what the naira was worth when it came in, the spread the bot earned on a fill is never counted as gain, and a taker's trade can only pull the cost towards the feed. When the feed then moves below that cost, the naira held is worth more than it was, and the curve lifts by a slice of the gain: takers bringing dollars get more naira for them, so the pile clears faster.

The lift is a shade like the curve's own. It's `min(cap_ngn, share x gain)` NGN at 100% naira, scaled down evenly to nothing at `start_pct`, applied off the curve's reference mid, and a quote pays its average over the stretch of share it walks. Each quote also holds the full lift to `4 x spread x mid / (1 - start)` (about 0.55 NGN at a 1 bps spread and start 0, 1.1 NGN at start 50), so a taker can't profit from the lift falling between their trades. It counts the spread, not the venue fee, because the fee is per request and the legs of a round trip needn't pay the same one. The panel shows the limit next to the cap and won't save a cap above it. Out and back over the same stretch cancel, so a round trip still costs the taker the spread twice, before or after a refresh. The ask is floored at the average cost: the lift never makes the bot pay out more naira per dollar than the naira was worth when it came in. (A plain curve that already sells under cost near full is left as it is.)

```toml
[modules.profit_unwind]
enabled = false      # needs [modules.curve] on
live = false         # quote with it; off, the bot computes and logs the lifted price and quotes the plain curve
share = 0.5          # share of the gain given back as a better price, 0 to 1
cap_ngn = 2.0        # the most the curve ever lifts, NGN per USDT
min_gain_ngn = 0.7   # gains under this move nothing
start_pct = 50.0     # off at or below this naira share, rising evenly to full at 100%
refresh_secs = 900   # how often the cost is frozen again
```

The lift only quotes with `live = true` on a Live curve. With `live = false` it's profit unwind's own preview: each curve quote's log line carries the cost, the full lift and the price it would have quoted (`unwound`) next to the one that went out. The gain follows the feed on every price; the cost is frozen every `refresh_secs`, except that new naira pulling it down lowers it at once, so the floor never sits above what's held.

The basis is saved in `profit-unwind-basis.json` next to `stitch.toml`, with the chain, the wallet or vault, the pair and the feed price it was marked against. A restart only carries it on for the same holder, and only if it was saved within twice `refresh_secs`. Anything else (another wallet or vault, Preview vs Live, unwind switched off for a while, a missing or unreadable file) starts over at the feed, so nothing moves until a real gain builds. The Parameters page shows the frozen cost, the feed, the gain and the lift at full and at today's share, and the curve it draws includes the lift once it quotes. The full design and its decision log are in [the profit unwind spec](cngn-profit-unwind-spec.md).

## Liquidity schedule

A vault with a yield adapter stakes all liquid settlement above its floor (`minLiquidSettlement`) in Aave. The venue only lets a vault order into a multi-maker bundle when it fits under that floor, because anything above it can be staked again between admission and broadcast. Settlement in Aave is still quotable, but only for single-order fills through the VaultOrderExecutor, which pulls it back first. So the floor decides how much of the vault can take part in bundles and how much earns yield.

The liquidity schedule moves the floor on a UTC clock. During market hours it keeps one share of free settlement liquid, outside them another. Free settlement is liquid plus the staked position, net of anything reserved for emergency redeemers.

```toml
[modules.liquidity]
enabled = true
open_utc = "08:00"        # UTC. A close before the open runs past midnight.
close_utc = "17:00"
days = ["mon", "tue", "wed", "thu", "fri"]   # UTC day each window opens on
market_floor_bps = 5000   # keep 50% of free settlement liquid in market hours
off_hours_floor_bps = 0   # stake everything outside them
tolerance_bps = 500       # leave gaps under 5% of free settlement alone

[modules.liquidity.authority]
roles_modifier = "0xYourRolesModifier"
role_key = "0x…"          # bytes32
```

Every 15 seconds the bot reads the vault and takes one step:

- **Raise.** The scheduled floor is above the current one: call `setMinLiquidSettlement` with the new value.
- **Recall.** The floor is right but fills have spent liquid below it: call `prepareSettlement` so the vault pulls the gap back from Aave. Anyone may call it.

The venue admits bundled orders up to the floor, but only liquid settlement can pay them. So whenever liquid sits short of the floor by any amount, after fills or right after a raise at the open, the bot caps its settlement-paying quotes at what is actually liquid. The check runs on every quote against the vault balances the bot reads every couple of seconds, so a fill that drains liquid caps the next quote without waiting for the 15-second schedule tick. A balance read older than three seconds, or none yet, caps them at zero, since it can't show that a fill hasn't just drained liquid. The tolerance only decides when a shortfall is worth a recall transaction; it never lifts the cap. That briefly narrows quotes, single-order fills included. A live schedule also starts with settlement-paying quotes capped at zero, and only a successful vault read sets or lifts a cap. While reads fail, the last cap stays in force with no expiry.
- **Lower.** The scheduled floor is below the current one. This is the risky direction: a bundled order admitted against the old floor needs that settlement liquid at broadcast, and once the floor drops, any `allocateIdle` (the executor calls one after every fill) can stake it. So the bot first caps its own quotes so live settlement commitments can never exceed the new floor, waits until what is already live fits under it, and only then lowers. The cap stays at the floor the lower was signed with, even if a deposit raises the schedule's target meanwhile, until the chain settles it. At close this can narrow settlement-paying quotes for about one order lifetime.

Every floor change, raise or lower, is tracked the same way until the account nonce it was signed at gets used, whatever the floor reads in the meantime: another admin transaction could have moved it, and the change could still execute after. Until then nothing else is sent. That's what stops two changes from both landing, for example a big raise that lands late after a smaller one, which would drop the floor under quotes admitted against the higher one. The change is saved to `liquidity-pending.json` with its chain, vault, signing account and nonce once it's signed and before its first broadcast (no save, no send), so a restart comes back with the slot held, a lower's cap already in force, and settles it the same way. The save includes the signed transaction, and a restart with the schedule live and the same signer broadcasts those same bytes once, in case the run stopped between saving and broadcasting; identical bytes can complete it but can't send a second one. A restart while a custody or MPC signer is still approving leaves nothing saved: nothing was broadcast, and the signature has no one to send it, so the schedule just plans again. A raise that may still land caps settlement-paying quotes at what's liquid while that's short of its target, checked on every balance read and in any mode, because the venue admits bundles up to the target the moment it lands. A bot pointed at another vault or chain while one is pending refuses to start, because that nonce can only be settled where it was signed. Point it back, or delete the file once you've checked the nonce was used. A change from a signer the bot no longer uses waits for that account's nonce.

Apart from that one broadcast of the identical bytes after a restart, the bot never resends or cancels a floor change. While it waits up to ten minutes for a receipt, the wallet re-sends it at a higher fee. After that the change stays pending, with the slot held and a lower's cap in force, until its nonce is used. One the node dropped isn't treated as gone, since peers may still hold it. If it's stuck, replace that nonce from the signing account; the Modules tab shows which nonce it waits on and the cap it holds. Turning the schedule off keeps a lower's cap and settles the change before the loop stops. Turning modules off entirely does too: a saved change starts a settling-only modules runtime (Preview, every module off) that does nothing else, and is gone after the next restart once the change settles.

Gaps smaller than the tolerance are ignored, so ordinary NAV drift and small fills do not each cost a transaction. Nothing happens while the vault is paused, has a closed redemption epoch, or has no yield adapter. At most one transaction is in flight; every step is re-derived from a fresh read, so a recall that lands late costs gas and nothing else. Every transaction from the bot's key, including redeem-epoch closes, picks its nonce under one process-wide lock, so two senders never sign at the same nonce. The Textile keeper keeps staking what sits above the floor every five minutes, whoever set it, so the schedule never calls `allocateIdle` itself.

### Authority

`setMinLiquidSettlement` is operator-admin only. Don't make the bot key the operator admin: that role can also rotate the strategy signer, guardian and fee recipient, and transfer itself away. Instead, the operator-admin Safe enables a [Zodiac Roles Modifier](https://docs.roles.gnosisguild.org/) and creates a role that:

- has this bot's signer as its only member,
- allows exactly one call: `setMinLiquidSettlement(uint256)` on this vault, plain call, no value,
- optionally bounds the argument, for example a minimum floor the bot can never go under.

The bot calls `execTransactionWithRole` on the modifier with `shouldRevert = true`, so a refused call fails gas estimation and nothing is broadcast. The modifier's `target()` has to be the vault's operator admin; the panel shows both and the bot refuses floor changes when they differ. Recalls go straight to the vault and need no role.

The bot's signer pays gas for both. It has to be able to sign transactions: local keys, Turnkey and MPCVault can; Fireblocks needs raw signing, which we don't recommend turning on just for this. Live mode with the schedule enabled refuses to start on a signer that can't. Preview works with any signer: it reads the vault and records what it would do, sends nothing and caps nothing.

Operators whose operator admin is a custody account rather than a Safe can't use this yet. The bot would need to drive the custody API for a second account, and that isn't built.

### Limits

The quote cap and the commitments it measures are this bot's own. Keep one bot per vault when the schedule is on, the same rule as every other module. The schedule uses UTC; it does not follow daylight saving or market holidays. A missed raise (bot down at the open) only means smaller bundles until it runs; a missed lower means a night without yield. Neither moves funds.

## Live visibility

Select a module in **Modules** to see its current readings. Open **Dynamic spreads → Overview** for the running mode, dynamic addition on each enabled side, observed high/low range, number of source samples and warmup progress. The breakdown separates base spreads, inventory adjustments and the final module quote policy. Buy and sell additions are derived from the recorded policy, including side limits, rather than recalculated from today's settings. The chart and recent changes show both sides over the last 200 recorded evaluations; they are quote calculations, not executed trades. A paused side is shown as off, not as a zero addition, and old observations without a breakdown leave gaps.

Live mode alone does not produce observations. The bot must be running with an authenticated RFQ session, an accepted corridor, a fresh reference price and vault data. No customer trade is required: the level-publication loop evaluates the policy. Waiting for a connection, RPC data or a usable feed is reported separately from a blocked evaluation. Zero extra spread is valid when sampled prices are unchanged, or when the multiplier/cap is zero. Warmup advances with new source timestamps, not repeated polling of the same price. While it runs, quotes stay live at the maximum addition.

The panel polls every five seconds and expires live readings independently of successful requests. It keeps old history visible but does not call it current. Running settings remain authoritative when saved settings differ after a failed or pending restart. Update both the bot and the panel to get input breakdowns and connection diagnostics; older status files still load, with an explanation of the missing details. Telemetry writes coalesce to the newest snapshot so a slow disk does not discard a later disconnect behind an older evaluation.

## Configuring modules in the panel

**Modules** opens one focused detail pane. The always-visible navigation lists every eligible module under Capital, Pricing and Liquidity, including disabled modules and bots with just one module. On mobile the same list sits above the detail pane. Saved Enabled/Disabled badges are separate from activity and Unsaved indicators. Selecting a module never enables it. Wallet bots have only the inventory curve among quote modules; vault bots expose the other policies as well. Each detail pane has **Overview**, **Settings** and **Activity**. Spot rebalancing’s Overview contains the existing manual-sale flow.

Settings shows the selected module’s enable switch and fields. The separate **Shared quote policy** disclosure below the workspace controls Off / Preview / Live for all enabled quote modules and names them before saving. Changing that mode has its own Save button; individual enable switches never change it. FX hedge retains its own mode. Percentages replace basis points in the form (0.01% = 1 bps); saved TOML and API units stay unchanged. Saving or discarding affects only the selected module (including profit unwind when editing the curve). Sibling drafts remain unsaved. The API still receives the full `[modules]` configuration, constructed from the saved snapshot plus only that editor’s changes, with the snapshot’s original revision. Revision conflicts preserve drafts and require a reload. Incompatible inventory policies are refused; no sibling is automatically disabled. Drafts survive module navigation; refresh reloads saved settings. Dealer credentials remain TOML-only. Preview is the panel name for `shadow` mode.

Inventory balancing and dynamic spreads each offer three draft-only presets: **Competitive**, **Balanced** and **Defensive**. Pick one from the **Preset** button at the top of an open module. Selecting one preserves run mode, module switches, the inventory minimum margin and dealer settings. Manual edits remain available. Open **How these presets were chosen** at the bottom of the preset list for the tradeoffs and dated market-data check; see [preset settings and evidence](./module-presets.md) for the full rationale. Existing configurations are never automatically migrated to a preset.

The interactive examples use draft settings and explicit hypothetical inputs. They sit behind **Simulate**, closed by default, below the fields on narrow panes and on the right when space allows. Every module's simulation shows the spread the bot would quote on each side at the example holdings: the corridor's saved buy and sell spread, what inventory balancing changes, what dynamic spreads add at the example price range, and the result. Modules never replace the corridor spread; they start from it and adjust it, so a 1 bps corridor with no inventory skew and no measured movement still quotes 1 bps. When the bot has a current evaluation, the example starts from its holdings and observed price range, and a **Quoting now** row (**Proposed now** in Preview) shows what that evaluation put on the book. With the saved settings, the simulated result and that row match; change a setting and the difference is its effect. Move either example input away and **Use the bot's current holdings** brings them back. The panel serves the bot's status file byte for byte, so the price range it recomputes is the bot's own. Modules that are off pass the spread through, and the open module's row is marked. The draft run mode decides whether that result is quoted (Live) or only recorded (Preview, Off). In Live with dynamic spreads on, the panel also names the spread the bot starts at before it has `warmup_secs` of price history: the full maximum on top of inventory balancing. The bot keeps its price window across restarts, so that only happens after switching from Off (which records no history) or after a stop longer than the window. An absolute corridor offset is converted to bps at the bot's last recorded price, so it needs the bot to have run once. `tests/module_spread_parity.rs` pins seeded cases from the bot's own `evaluate` and `apply` in `web/src/moduleSpreadParity.json`, and the web tests replay them against the panel, so the two can't drift apart unnoticed. The examples do not model returns, open quotes, funds, vault limits, freshness or execution. The example never saves settings. Invalid numeric combinations block saving and pause the example without silently adjusting related fields.

The save bar at the bottom lists the selected module’s unsaved changes as saved → draft value, including secondary settings (presets change the inventory price adjustment and the spread timing, for example). Discard returns only this module to its saved settings.

For new module UIs, use the shared controls in `web/src/components/ModuleControls.tsx`: a named enable switch, one or two main numeric controls, a labeled illustration, and a `SecondarySettings` panel for second-order settings. Explanations go behind the module's **How … works** link (`moduleGuides` in `ModuleParameterFields.tsx`), not in a card under the fields. Add every editable setting to the list behind `moduleChanges`, so the save bar covers it. Add the navigation entry to `moduleWorkspace.ts` and keep its eligibility and authorization explicit. Keep live measurements in Overview and hypothetical inputs in Settings. Reuse panel color tokens, native keyboard controls and the same percentage units. Add numeric constraints and illustration regression cases alongside `modulePresentation.ts`; keep server validation authoritative.

## Manual sales in Fleet

Enable spot rebalancing and save in Live mode, then open **Modules → Spot rebalancing → Overview**. Choose an amount and minimum **net** proceeds to the vault. Use your own buying wallet or enter the wallet of a buyer you invite. Creating the private 24-hour request moves no money. Share its link yourself, or open the buyer page. Fleet never sends a message to the buyer.

The buyer page runs on the Textile web app; Fleet stays private. It obtains a fresh, exact-output RFQ restricted to this vault. The bot checks the request against its locally stored authorization, current prices and balances, sale limits, minimum proceeds and reservations. Normal quote margins determine the price; the entered minimum is a floor, not a guaranteed quote. The existing venue supplies Warp co-signing and settlement calldata. Only the named buyer can fill. No contract changes are required.

The buyer sees the full token payment including the trading fee; network gas is separate. Existing wallet handling covers EOAs, Safe, custody/MPC and hardware wallets, including sequential token approvals and pending-request recovery. A token approval can outlast a quote. Complete it, then explicitly request a fresh quote. Do not queue an expired swap. Only the existing on-chain reconciliation marks a sale completed; a link, wallet proposal or transaction hash does not.

Closing a request stops new quotes. Already-signed orders stay executable until their deadline and remain reserved locally. Every refresh uses the same durable Permit2 nonce, including after a bot restart, so the request can settle at most once. The venue also refuses nonce reuse until the previous order deadline plus clock-skew allowance has passed and confirms that the nonce is unspent. If the vault's trading epoch changes, create a new request. Cooldown applies between different sale requests.

The holdings chart is an estimate using the current reference price and your minimum proceeds. Buying from your own vault moves the FX exposure into your personal wallet. It does not remove that exposure from your combined holdings. A request does not guarantee that a buyer will take it.

### Deployment and migration

Deploy the additive database migration, API and web buyer route, then update Stitch and Fleet together. The single `modules_enabled` switch continues to gate the entire feature. Rebalancing stays disabled until explicitly enabled. Existing configs without `modules.rebalance.method` now use manual mode **even when dealer details are present**. Preserve automatic dealer execution only by explicitly setting `method = "dealer"`.

Keep `manual-sale-*.json`, their `-nonce.json` files, `manual-sale-pacing.json` and the existing reservation ledger in the bot's persistent config directory. Never clear them to refresh a quote. Fleet cannot create requests without the bot's maker credential. One decision owner per vault remains required. The configured venue's `PUBLIC_URL` must point to the matching web deployment for buyer links.

## Dealer execution (optional, later rollout)

The dealer adapter remains available through explicit `modules.rebalance.method = "dealer"`. It reuses constrained vault orders, the Warp co-signer and the reactor / VaultOrderExecutor. A compatible dealer must be supplied; configuring the adapter does not provide a buyer or establish liquidity. Fleet defaults to the manual flow above.

```toml
[modules.rebalance.dealer]
url = "https://your-dealer.example/stitch/v1"
taker = "0x0000000000000000000000000000000000000003" # replace
api_key_env = "STITCH_DEALER_API_KEY" # optional; set the secret in stitch.env
```

Stitch sends `POST {url}/quote`:

```json
{
  "requestId": "module-rebalance:unique-id",
  "chainId": 56,
  "vault": "0x...",
  "sellToken": "0x...",
  "buyToken": "0x...",
  "sellAmount": "200000000",
  "minBuyAmount": "199000",
  "deadline": 1791200060
}
```

The dealer returns `requestId`, `taker`, `sellAmount`, `buyAmount`, and `expiresAt` (Unix seconds). Amounts are decimal strings in atomic units. `buyAmount` is the **net amount the vault receives**, including all dealer charges. The configured taker and exact sell amount must match; proceeds must meet the price floor; expiry must fit the request. Redirects are refused. Requests are bounded to five seconds and quote responses to 16 KiB. Credentials come from the named environment variable and never appear in decisions.

After checking the response against current exposure and prices, Stitch constructs the order itself: maker and output recipient are the vault, tokens are its configured pair, and preferred fillers bind the configured counterparty (plus the configured VaultOrderExecutor). It durably reserves inventory **before** starting background EIP-712 signing. Completed signatures return to the RFQ engine, which rechecks current reservations, price, inventory, order caps, epoch and expiry before sending `POST {url}/execute` with `requestId`, `chainId`, `encodedOrder`, and `strategySignature`. Any other live reservation withholds the signature. Disconnecting before this check drops the pending result; the durable reservation remains until expiry.

The dealer must obtain the independent risk signature through its authorized Warp integration, assemble the vault envelope, and fill through the existing reactor or VaultOrderExecutor. It supplies settlement tokens, including the reactor's applicable fee, and pays execution gas. Stitch neither exposes Warp credentials nor bypasses the risk signature. An HTTP 2xx means receipt only. Any timeout or failure keeps the reservation until deadline plus clock skew; the signature may already have been delivered. Stitch does not retry submission or label receipt as a confirmed fill. Fresh on-chain balances determine later exposure.

The reservation file, `modules-attempt.json` and `liquidity-pending.json` must survive restarts. An unreadable attempt or pending-floor file refuses module startup; unavailable reservation persistence withholds new module-enabled orders. Keep these files with the bot's config volume. Both local and custody/MPC strategy signers use the existing typed-signature interface. The dealer broadcasts, so the module does not manage operator transaction nonces or assume immediate custody approval; signatures arriving too late are withheld.

## Implementation boundary

`src/modules` owns versioned, plain-input strategy evaluation, configuration and observation. The inventory curve's maths is `src/pricing/curve.rs` and its quote path is `decide_quote_curved` and `levels_for_curved` in `src/rfq/responder.rs`; the module host (`src/rfq/modules.rs`) measures what it reads (`curve_inputs`) and records its readings and quotes in the module status. `src/rfq/modules.rs` translates decisions into the existing quote and reservation machinery. `src/rfq/liquidity.rs` runs the liquidity schedule: it reads the vault, applies the quote cap and sends the floor and recall transactions from the bot's wallet. `src/modules/liquidity.rs` only decides the step. Strategies do not receive a signer, RPC client, file handle or network client. The host owns execution and validates permissions, inventory, pricing and expiry.

Economic vault balances (`freeSettlement`, `freeCorridor`) are read every 2 seconds in the same Multicall3 batch as quote inventory, so modules cost no request of their own, Shadow mode included. A reverting module view only leaves the module balances missing; quote inventory still refreshes. On a chain without Multicall3 they refresh in a separate process-scoped loop with at most one request in flight instead, so a slow module read cannot delay ordinary quote-inventory refreshes. Missing or stale economic balances block live rebalancing and enabled quote strategies. With only rebalancing enabled, customer quotes keep their existing pricing and ordinary inventory checks.

`modules-status.json` exposes up to 200 recent observations and the running configuration to the authenticated panel. It is a bounded diagnostic snapshot, not an accounting ledger or a complete historical dataset. Recording runs outside the quote loop. Settings show draft versus saved configuration; telemetry shows the running configuration, and stale telemetry is not presented as current.

New internal policies can add evaluators and typed settings while retaining the host boundary. A future external runtime can produce the same bounded decisions, but would still need isolation, capability checks and version negotiation. There is no public module loader, marketplace or arbitrary-code execution in this release. Bump the module model version when decision semantics change.
