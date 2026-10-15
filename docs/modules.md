# FX protection modules

Modules are first-party Stitch policies for an OperatorVault. Enable the feature with one **root property**, before any TOML table:

```toml
modules_enabled = true
```

In the panel, **Corridors → Experimental → Turn on modules** sets it for you, always in Preview, even if they were last saved in Live. It lists anything that stops the bot loading with modules on (no vault, a second corridor, the ladder or taker leg, TWAP or lean, or no Textile connection) before you try, and **Turn off modules** sets it back.

It defaults to false. With the flag absent or false, there is no module runtime, decision recorder, dealer traffic, Modules tab or module API access. Existing quotes use their existing prices and sizing.

Enabling the flag opens the Modules panel and starts in **shadow** mode. Shadow records proposals while preserving existing quoting. Choose Live and save to apply enabled policies. Saving uses the normal validated config/restart flow; a stopped bot stays stopped. Off stops new module actions. Previously signed orders remain reserved and executable until expiry, including after the master flag is disabled.

This first release supports one RFQ corridor per bot, funded by an OperatorVault. The pool's collateral must be the vault's corridor asset and its debt the settlement asset. Disable legacy lean, TWAP, ladder, limit-taker and closer settings. Use one module decision owner per vault: exposure limits are local strategy policy, not a new on-chain invariant across independently operated bots.

## Strategies

**Inventory balancing** measures corridor value as a fraction of free vault NAV. Settlement deployed in the existing yield adapter counts toward NAV. Pending deposits and reserved redemption payouts are excluded by the vault's free-balance views. Spendable amounts still come from the existing quotable inventory path, with reserves, pauses, close-only rules and per-order caps intact.

Above the target, buying more corridor currency becomes less attractive and selling it becomes more attractive. Below target, the skew reverses. The configured spread floor bounds tightening. Buys stop at the maximum share; exact post-trade arithmetic also checks the limit. An outstanding inventory-increasing quote blocks another buy until its reservation is released. A proposed or unconfirmed sale never reduces measured exposure.

**Dynamic spreads** adds a bounded spread based on the high/low range of observed prices in a rolling window. It uses source timestamps already received, not future data. Until the window spans the warmup interval it adds the maximum (`max_extra_bps`) rather than pulling quotes, and it rejects stale or future prices. The window is saved to `modules-history.json` beside the config, so a restart (every module save is one) resumes warm. Samples from a different feed URL are never reused. By default, the same extra spread is added to both enabled sides after inventory skew. The same decision changes indicative levels and firm quotes.

Enable **Favor inventory reduction** under Parameters → Dynamic spreads, or set `modules.spreads.inventory_aware = true`, to reduce the volatility buffer on trades that move holdings toward the inventory target. Inventory balancing must also be enabled; otherwise the additions remain equal. Existing configurations default to false, so upgrading does not change their quotes.

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

## Live visibility

Overview shows current holdings, buy/sell margins and saved module switches. Expand **Details & recent activity → Dynamic spreads · live monitor** for the running mode, dynamic addition on each enabled side, observed high/low range, number of source samples and warmup progress. The breakdown separates base spreads, inventory adjustments and the final module quote policy. Buy and sell additions are derived from the recorded policy, including side limits, rather than recalculated from today's settings. The chart and recent changes show both sides over the last 200 recorded evaluations; they are quote calculations, not executed trades. A paused side is shown as off, not as a zero addition, and old observations without a breakdown leave gaps.

Live mode alone does not produce observations. The bot must be running with an authenticated RFQ session, an accepted corridor, a fresh reference price and vault data. No customer trade is required: the level-publication loop evaluates the policy. Waiting for a connection, RPC data or a usable feed is reported separately from a blocked evaluation. Zero extra spread is valid when sampled prices are unchanged, or when the multiplier/cap is zero. Warmup advances with new source timestamps, not repeated polling of the same price. While it runs, quotes stay live at the maximum addition.

The panel polls every five seconds and expires live readings independently of successful requests. It keeps old history visible but does not call it current. Running settings remain authoritative when saved settings differ after a failed or pending restart. Update both the bot and the panel to get input breakdowns and connection diagnostics; older status files still load, with an explanation of the missing details. Telemetry writes coalesce to the newest snapshot so a slow disk does not discard a later disconnect behind an older evaluation.

## Configuring modules in the panel

Parameters shows the whole module config as one **Quote strategy** card. The run mode (Off, Preview, Live) sits in its header. Each module gets a row with its on/off switch, its two defining settings and its preset. **Edit** opens that row's values in place, one module at a time. Each open module has a **How … works** link in its top-right corner that explains the behavior. Percentages replace basis points in the form (0.01% = 1 bps); the saved TOML and API units are unchanged. Sliders have exact numeric inputs. Saving writes the `[modules]` tables as one block after the bot's other tables. Main controls come first; price floors, timing and execution limits sit below them in separate, always-visible panels (**Price adjustments**, **Observation timing**, **Execution limits**). Manual sale requests have their own tab; dealer credentials are configured through TOML. Preview is the panel name for `shadow` mode.

Inventory balancing and dynamic spreads each offer three draft-only presets: **Competitive**, **Balanced** and **Defensive**. Pick one from the **Preset** button at the top of an open module. Selecting one preserves run mode, module switches, the inventory minimum margin and dealer settings. Manual edits remain available. Open **How these presets were chosen** at the bottom of the preset list for the tradeoffs and dated market-data check; see [preset settings and evidence](./module-presets.md) for the full rationale. Existing configurations are never automatically migrated to a preset.

The interactive examples use draft settings and explicit hypothetical inputs. They sit behind the **Simulate** tab on the right edge of an open module, closed by default; once opened it stays open as you switch modules. Inventory examples start with a 0.20% base margin; spread examples show only the extra volatility margin. They do not model returns, reservations, freshness or execution. The example never saves settings. Invalid numeric combinations block saving and pause the example without silently adjusting related fields.

The save bar at the bottom lists every unsaved change as saved → draft value, including secondary settings (presets change the inventory price adjustment and the spread timing, for example). Discard returns the draft to the saved settings.

For new module UIs, use the shared controls in `web/src/components/ModuleControls.tsx`: a named enable switch, one or two main numeric controls, a labeled illustration, and a `SecondarySettings` panel for second-order settings. Explanations go behind the module's **How … works** link (`moduleGuides` in `ModuleParameterFields.tsx`), not in a card under the fields. Add the module's two headline settings to `moduleKeyFigures` and every editable setting to the list behind `moduleChanges`, so the strategy row and save bar cover it. Keep live measurements in Overview and hypothetical inputs in Parameters. Reuse panel color tokens, native keyboard controls and the same percentage units. Add numeric constraints and illustration regression cases alongside `modulePresentation.ts`; keep server validation authoritative.

## Manual sales in Fleet

Enable spot rebalancing and save in Live mode, then open **Manual sale**. Choose an amount and minimum **net** proceeds to the vault. Use your own buying wallet or enter the wallet of a buyer you invite. Creating the private 24-hour request moves no money. Share its link yourself, or open the buyer page. Fleet never sends a message to the buyer.

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

The reservation file and `modules-attempt.json` must survive restarts. An unreadable attempt file refuses module startup; unavailable reservation persistence withholds new module-enabled orders. Keep these files with the bot's config volume. Both local and custody/MPC strategy signers use the existing typed-signature interface. The dealer broadcasts, so the module does not manage operator transaction nonces or assume immediate custody approval; signatures arriving too late are withheld.

## Implementation boundary

`src/modules` owns versioned, plain-input strategy evaluation, configuration and observation. `src/rfq/modules.rs` translates decisions into the existing quote and reservation machinery. Strategies do not receive a signer, RPC client, file handle or network client. The host owns execution and validates permissions, inventory, pricing and expiry.

Economic vault balances (`freeSettlement`, `freeCorridor`) are read every 2 seconds in the same Multicall3 batch as quote inventory, so modules cost no request of their own, Shadow mode included. A reverting module view only leaves the module balances missing; quote inventory still refreshes. On a chain without Multicall3 they refresh in a separate process-scoped loop with at most one request in flight instead, so a slow module read cannot delay ordinary quote-inventory refreshes. Missing or stale economic balances block live rebalancing and enabled quote strategies. With only rebalancing enabled, customer quotes keep their existing pricing and ordinary inventory checks.

`modules-status.json` exposes up to 200 recent observations and the running configuration to the authenticated panel. It is a bounded diagnostic snapshot, not an accounting ledger or a complete historical dataset. Recording runs outside the quote loop. Settings show draft versus saved configuration; telemetry shows the running configuration, and stale telemetry is not presented as current.

New internal policies can add evaluators and typed settings while retaining the host boundary. A future external runtime can produce the same bounded decisions, but would still need isolation, capability checks and version negotiation. There is no public module loader, marketplace or arbitrary-code execution in this release. Bump the module model version when decision semantics change.
