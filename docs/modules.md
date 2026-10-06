# FX protection modules

Modules are first-party Stitch policies for an OperatorVault. Enable the feature with one **root property**, before any TOML table:

```toml
modules_enabled = true
```

It defaults to false. With the flag absent or false, there is no module runtime, decision recorder, dealer traffic, Modules tab or module API access. Existing quotes use their existing prices and sizing.

Enabling the flag opens the Modules panel and starts in **shadow** mode. Shadow records proposals while preserving existing quoting. Choose Live and save to apply enabled policies. Saving uses the normal validated config/restart flow; a stopped bot stays stopped. Off stops new module actions. Previously signed orders remain reserved and executable until expiry, including after the master flag is disabled.

This first release supports one RFQ corridor per bot, funded by an OperatorVault. The pool's collateral must be the vault's corridor asset and its debt the settlement asset. Disable legacy lean, TWAP, ladder, limit-taker and closer settings. Use one module decision owner per vault: exposure limits are local strategy policy, not a new on-chain invariant across independently operated bots.

## Strategies

**Inventory balancing** measures corridor value as a fraction of free vault NAV. Settlement deployed in the existing yield adapter counts toward NAV. Pending deposits and reserved redemption payouts are excluded by the vault's free-balance views. Spendable amounts still come from the existing quotable inventory path, with reserves, pauses, close-only rules and per-order caps intact.

Above the target, buying more corridor currency becomes less attractive and selling it becomes more attractive. Below target, the skew reverses. The configured spread floor bounds tightening. Buys stop at the maximum share; exact post-trade arithmetic also checks the limit. An outstanding inventory-increasing quote blocks another buy until its reservation is released. A proposed or unconfirmed sale never reduces measured exposure.

**Dynamic spreads** adds a bounded spread based on the high/low range of observed prices in a rolling window. It uses source timestamps already received, not future data. It waits for the warmup interval and rejects stale or future prices. By default, the same extra spread is added to both enabled sides after inventory skew. The same decision changes indicative levels and firm quotes.

Enable **Weight volatility by inventory** in Parameters, or set `modules.spreads.inventory_aware = true`, to reduce the volatility buffer on trades that move holdings toward the inventory target. Inventory balancing must also be enabled; otherwise the additions remain equal. Existing configurations default to false, so upgrading does not change their quotes.

Above target, buys retain the full volatility buffer. The sell buffer falls linearly with exposure, reaching zero at the maximum share, where buys remain paused. Below target, sells retain the full buffer and the buy buffer falls toward zero at zero corridor holdings. At target the additions are equal. The discount is rounded down to whole bps: `extra * min(distance_from_target, band) / band`, with `band = max - target` above target and `band = target` below. Each side stays within `max_extra_bps`. This does not reduce the inventory-adjusted spread itself, so its floor, disabled sides, reservations and purchase limits remain in force. `max_skew_bps` still bounds the inventory module's adjustment; volatility weighting only changes its own extra spread.

For example, with target 30%, maximum 60% and current inventory 45%, a 20 bps volatility buffer adds 20 bps to buys and 10 bps to sells. At 60% or more, it adds nothing to sells and purchases stay paused. This favors reducing excess exposure; it does not predict the currency's direction or guarantee a buyer. Historical and imported-data simulations use this same calculation and export the selected setting with the report.

**Automatic spot rebalancing** requests a sale when corridor exposure reaches its trigger. The sale is bounded by excess above target, a NAV fraction, spendable corridor inventory and the vault's per-order cap. It waits while any quote claims inventory, and it persists its cooldown before contacting a dealer. It never opens derivatives or sends vault funds to an exchange account.

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
trigger_bps = 5000
max_trade_bps = 200
max_slippage_bps = 50
cooldown_secs = 300
order_lifetime_secs = 60
```

Numbers are starter configuration, not recommendations for a particular currency. Measure feed quality, market depth and trading costs before selecting live parameters.

## Live visibility

Open **Modules → Overview → Dynamic spreads · live monitor**. It shows the running mode, dynamic addition on each enabled side, observed high/low range, number of source samples and warmup progress. The breakdown separates base spreads, inventory adjustments and the final module quote policy. Buy and sell additions are derived from the recorded policy, including side limits, rather than recalculated from today's settings. The chart and recent changes show both sides over the last 200 recorded evaluations; they are quote calculations, not executed trades. A paused side is shown as off, not as a zero addition, and old observations without a breakdown leave gaps.

Live mode alone does not produce observations. The bot must be running with an authenticated RFQ session, an accepted corridor, a fresh reference price and vault data. No customer trade is required: the level-publication loop evaluates the policy. Waiting for a connection, RPC data or a usable feed is reported separately from a blocked evaluation. Zero extra spread is valid when sampled prices are unchanged, or when the multiplier/cap is zero. Warmup advances with new source timestamps, not repeated polling of the same price.

The panel polls every five seconds and expires live readings independently of successful requests. It keeps old history visible but does not call it current. Running settings remain authoritative when saved settings differ after a failed or pending restart. Update both the bot and the panel to get input breakdowns and connection diagnostics; older status files still load, with an explanation of the missing details. Telemetry writes coalesce to the newest snapshot so a slow disk does not discard a later disconnect behind an older evaluation.

## Dealer execution

The existing Textile RFQ endpoint serves customer-initiated requests; it cannot make this vault spend as a taker. Stitch therefore exposes a dealer adapter that reuses existing constrained vault orders, the Warp co-signer and the reactor / VaultOrderExecutor. No contracts change. **A compatible dealer must be supplied; this PR does not deploy a buyer or imply that any corridor has liquidity.** Without one, the panel reports that a spot sale is indicated but cannot execute.

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

The dealer returns `requestId`, `taker`, `sellAmount`, `buyAmount`, and `expiresAt` (Unix seconds). Amounts are decimal strings in atomic units. `buyAmount` is the **net amount the vault receives**, including all dealer charges. The configured taker and exact sell amount must match; proceeds must meet the price floor; expiry must fit the request. Redirects are refused. Requests are bounded to five seconds and quote responses to 16 KiB. Credentials come from the named environment variable and never appear in decisions or simulation output.

After checking the response against current exposure and prices, Stitch constructs the order itself: maker and output recipient are the vault, tokens are its configured pair, and preferred fillers bind the configured counterparty (plus the configured VaultOrderExecutor). It durably reserves inventory **before** starting background EIP-712 signing. Completed signatures return to the RFQ engine, which rechecks current reservations, price, inventory, order caps, epoch and expiry before sending `POST {url}/execute` with `requestId`, `chainId`, `encodedOrder`, and `strategySignature`. Any other live reservation withholds the signature. Disconnecting before this check drops the pending result; the durable reservation remains until expiry.

The dealer must obtain the independent risk signature through its authorized Warp integration, assemble the vault envelope, and fill through the existing reactor or VaultOrderExecutor. It supplies settlement tokens, including the reactor's applicable fee, and pays execution gas. Stitch neither exposes Warp credentials nor bypasses the risk signature. An HTTP 2xx means receipt only. Any timeout or failure keeps the reservation until deadline plus clock skew; the signature may already have been delivered. Stitch does not retry submission or label receipt as a confirmed fill. Fresh on-chain balances determine later exposure.

The reservation file and `modules-attempt.json` must survive restarts. An unreadable attempt file refuses module startup; unavailable reservation persistence withholds new module-enabled orders. Keep these files with the bot's config volume. Both local and custody/MPC strategy signers use the existing typed-signature interface. The dealer broadcasts, so the module does not manage operator transaction nonces or assume immediate custody approval; signatures arriving too late are withheld.

## Historical simulation

Choose a period in **Modules → Historical simulation**, enter your estimated execution cost, and select **Collect data and run**. JSON upload is optional under Advanced. Collection uses the Textile API configured by `indexer_url`; the browser cannot supply an arbitrary data-service URL. It requires the same `modules_enabled = true` flag as the rest of Modules. It never saves settings, starts the bot, signs, or sends transactions.

The collector resolves the registered vault's exact chain and token pair. It reads free balances, reserves and order limits at the last block **strictly before** the period starts, loads recorded reference prices, and fetches maker-attributed swaps from a pinned subgraph snapshot. Canonical cNGN/USDT and cNGN/USDC corridors use the `CngnPricingSample.center` series that the live Textile price feed serves, including its recorded center adjustments. Other feeds use `MarketRateObservation` rows for the corridor's registered source. Missing cNGN samples are not replaced with raw Monierate prices. No new recording setup or database migration is needed. Both database ingestion and source observation must precede use of a price. Late older observations cannot rewind the simulated feed.

The API must have archive-capable RPC access, a registered vault, stored prices and a healthy trade index covering the requested end time. Missing archive state, missing boundary prices, incomplete trade history and oversized periods produce actionable errors. Current balances never replace unavailable historical balances. Periods span 5 minutes to 30 days and end at least 5 minutes ago; presets end 10 minutes ago. Raw collection is capped at 10,000 prices and 5,000 swaps, and the combined replay at 10,000 events. Shorten dense periods instead of silently downsampling volatility. The public data endpoint is restricted to known vaults, shares the chain read budget, and coalesces requests in a bounded cache.

The report compares a baseline portfolio using the **current quote settings with modules disabled** against a separate portfolio running the selected draft policies. It calls the same pure strategy functions used for live quoting. Each portfolio starts from the same archived free holdings and evolves from its own simulated fills. Later observed balances never overwrite either portfolio. This is a fixed-capital experiment, not reconstruction of realized vault or LP returns.

Results show return differences, drawdown, peak corridor exposure, fills, modeled execution costs and portfolio value over time. The data section shows the source, actual requested period, starting block, index coverage, fresh-price coverage, largest observation gap and stale-price trades skipped. Export the collected replay dataset or the full result including captured parameters, baseline quote settings, source metadata, coverage and assumptions. A run without customer trades cannot establish the benefit of quote strategies; incomplete coverage is called out in the conclusion.

Automatic collection uses executed fill prices as **proxy customer limits**. It cannot discover rejected RFQs or customer willingness to pay different prices. Stored reference prices may differ from the operator's adjusted feed, and sparse observations miss price moves between samples. Dynamic spreads warm up from an empty rolling window during each run. Same-second prices precede fills, whose intra-block order is unknown. Changing spreads may reject historical trades without proving that those losses could actually have been avoided.

Historical executable dealer quotes are not currently stored, so the automatic default simulates no spot rebalance fills. The optional **hypothetical dealer scenario** uses the entered price discount and corridor-token depth, replenished on each fresh price observation. Results label this assumption explicitly; it is not evidence of executable historical liquidity. Keep the rebalance module enabled in the draft to evaluate it. Test different depth, discount and execution-cost assumptions.

### Optional JSON import

Download the empty template in the panel. Fill in the actual chain, asset pair, token precisions, initial balances, per-order limits, reserves and settlement-denominated execution cost per fill. Add chronological events:

```json
{
  "at": 1700000000,
  "price_at": 1700000000,
  "price": 0.001,
  "trade": {
    "vault_buys": true,
    "corridor_amount": "1000000",
    "limit_price": 0.00099
  },
  "dealer": {
    "max_corridor": "1000000",
    "net_price": 0.000995
  }
}
```

This event is illustrative. `at` is when data became available to the strategy; `price_at` is when the source observed the price. Prices are settlement per corridor token. `trade` and `dealer` are optional. A customer's `limit_price` is the minimum they accept when selling to the vault, or the maximum they pay when buying from it. **Executed trades alone do not reveal their acceptance threshold or what they would have done at a different price.** Supply those assumptions explicitly rather than presenting changed spreads as an exact replay of realized returns.

Customer fills require an acceptable price and sufficient funds. Spot sales require dealer prices and depth in the dataset: absent observations mean zero simulated rebalance fills. Dealer prices must be net of spread, slippage and dealer fees. `cost_per_trade` adds settlement-denominated gas/other execution costs to every modeled fill. Compare multiple cost assumptions, calm periods and falling-currency periods. Exports include the selected configuration and all model limitations.

This is a conditional simulation with immediate settlement. It does not model latency, concurrent quote reservations, failed transactions, demand changes, deposits/withdrawals, yield accrual, changing vault restrictions, off-venue trades, or management/performance fees. It can overstate achievable results. Returns deduct only the costs entered; zero cost means before unrecorded execution expenses. Results are not realized LP returns or evidence of future profit. Imported data is limited to 10,000 events and a 1.9 MB upload. Existing historical activity is not silently relabeled as executable dealer liquidity.

## Implementation boundary

`src/modules` owns versioned, plain-input strategy evaluation, configuration, replay and observation. `src/rfq/modules.rs` translates decisions into the existing quote and reservation machinery. Strategies do not receive a signer, RPC client, file handle or network client. The host owns execution and validates permissions, inventory, pricing and expiry. Replay has no path to execution.

Economic vault balances refresh in a separate process-scoped loop with at most one request in flight. A slow module read cannot delay ordinary quote-inventory refreshes. Missing or stale economic balances block live rebalancing and enabled quote strategies. With only rebalancing enabled, customer quotes keep their existing pricing and ordinary inventory checks.

`modules-status.json` exposes up to 200 recent observations and the running configuration to the authenticated panel. It is a bounded diagnostic snapshot, not an accounting ledger or a complete historical dataset. Recording runs outside the quote loop. Settings show draft versus saved configuration; telemetry shows the running configuration, and stale telemetry is not presented as current.

New internal policies can add evaluators and typed settings while retaining the host boundary. A future external runtime can produce the same bounded decisions, but would still need isolation, capability checks and version negotiation. There is no public module loader, marketplace or arbitrary-code execution in this release. Bump the module model version when decision or replay semantics change.
