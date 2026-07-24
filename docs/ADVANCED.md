# Stitch Advanced Guide

Configuration reference, tuning, and troubleshooting for operators who need more
than the [README](../README.md) quick start. If you installed with the AI prompt,
the primary settings are already filled in — this guide is for understanding and
changing them.

## Configuration Reference

Start from [stitch.example.toml](../stitch.example.toml). Stitch reads the config at
startup, so restart the process after any change.

### Price Feed

Each pool prices off an HTTP feed that returns JSON with a price and a Unix
timestamp:

```bash
curl -s "https://your-feed.example/cngn-usdc"
```

```json
{ "price": 0.000724, "timestamp": 1760000000 }
```

`price` is **debt per collateral** — the stable per soft, e.g. USDC per cNGN
(≈ 0.000724), **not** cNGN per USDC. Stitch quotes off it directly as the
bid/ask mid; publishing the inverted soft-per-stable number (e.g. 1382) makes the
bot post wildly mispriced orders. (The absolute spread below is the opposite
orientation — soft per stable — by design; only the feed `price` is debt per
collateral.)

Check:

- `price` is debt per collateral (stable per soft).
- `timestamp` is a Unix timestamp, and its age is less than `staleness_secs`.
- Each pool with a different price has its own `feed_url`; one shared feed can't
  price cNGN, COPM, and KES at once.

If the feed is stale, Stitch skips quoting that pool rather than posting orders
at an old price.

### Spreads

Each side needs one spread, either relative (basis points) or absolute (soft per
stable):

```toml
buy_offset_bps = 10        # 0.1% below the mid
sell_offset_bps = 10       # 0.1% above the mid
# or, absolute in soft-per-stable units (collateral per debt, e.g. cNGN/USDC),
# added to the bid and subtracted from the ask so the bid always buys cheaper
# and the ask sells dearer. At a 1382 soft-per-stable mid, buy_offset_abs = 5
# bids at 1387 and sell_offset_abs = 5 asks at 1377.
buy_offset_abs = 5.0
sell_offset_abs = 5.0
```

If both are set for a side, basis points win.

### TWAP Quoting (Smoothed Center)

By default each quote centers on the feed's instantaneous value. For a
volatile pair like WETH that means the book chases every tick: when the price
spikes for a few seconds and reverts, the quote either chases the spike (and
buys the top) or sits stale (and the resting side gets picked off). Setting
`twap_window_secs` centers the spread on a short rolling time-weighted average
of the feed instead:

```toml
twap_window_secs = 60         # ~1 min rolling TWAP; omit to quote off spot
# twap_max_deviation_bps = 50 # optional guard, default 50 — see below
refresh_threshold_bps = 0     # no deadband: stay pinned to the smoothed center
```

The average moves slowly, so a transient spike leaves the quote near the
settled mean: the ask sells *into* the spike above the reverting average and
keeps the spread when it snaps back. A move that persists flows into the
average, and the center fully converges within one window. The trade is
explicit — TWAP wins on noise (spikes that revert) and pays a lag on trend
(moves that persist) — so it's worth it exactly when short-term price action
is more noise than trend. Two knobs to tune:

- **The window** (`twap_window_secs`, ~60–300s is sensible): longer filters
  more noise but lags real moves more.
- **The spread** (`buy_offset_bps` / `sell_offset_bps`): what each fill earns
  around the center.

The TWAP weights each feed observation by how long it was actually live
(last observation carried forward), so it is correct for both a fast feed
(fresh sample every tick) and a slow one (the cNGN center re-samples every
~3 min — though for a feed that smoothed already, a TWAP on top adds little).
A feed gap longer than `staleness_secs` resets the average rather than
carrying a price across an outage the bot refused to quote through.

**The deviation guard.** The lag on trend needs a bound: with the book
re-posting every tick, an unguarded ask would keep selling further and further
below a market that keeps rising until the average catches up.
`twap_max_deviation_bps` (default 50) trails each side at most that far
through the instantaneous feed — the ask never posts more than the deviation
below spot, the bid never more than it above. The clamp only ever moves a
quote *away* from being picked off (ask up, bid down), which also makes it
glitch-safe: a single bad feed print lifts the ask out of reach (unfillable,
harmless) instead of dragging the bid toward the glitch. Inside the deviation
budget nothing is clamped, so ordinary spikes still get sold into.

**Drop the deadband with TWAP.** `refresh_threshold_bps` exists to skip
re-signing when the price barely moved, but a TWAP center *always* barely
moves — a deadband on top of it holds the book stale between threshold
crossings, which is the exact failure TWAP is meant to fix. Set it to 0 (or
remove it; 0 is the default) so the book re-posts every `tick_interval_secs`,
pinned to the current center. Posting is off-chain and free. The costs of
every-tick reposting are local: one balance/allowance read per side per tick
against your RPC, and one signature per live order per tick — a full 40-slice
ladder on both sides is 80 signatures per tick, instant for the local
hotwallet but a round trip each for an MPC signer (raise
`max_concurrent_signs`, or lengthen `tick_interval_secs`, if your signing
backend rate-limits).

TWAP composes with the inventory lean: the lean's fair price becomes the
smoothed center, and the deviation guard clamps the lean's quotes the same as
the configured spreads. The auction closer deliberately keeps pricing off the
instantaneous feed — it values a one-shot on-chain fill at execution time,
where the current price is the right mark; the TWAP smooths standing quotes
that rest in the book waiting to be picked off.

### Liquidity And Order Sizing

Stitch can post one order per side or a ladder of smaller orders:

```toml
buy_total_liquidity_debt = "50000000000"
buy_min_slice_debt = "10000000"
buy_max_orders = 40

sell_total_liquidity_collateral = "50000000000"
sell_min_slice_debt = "10000000"
sell_max_orders = 40
```

- `*_total_liquidity_*` controls total depth for that side.
- Set `*_total_liquidity_* = "max"` to quote all currently funded wallet
  inventory for that side. Stitch resolves this at quote time from the wallet's
  token balance, Permit2 allowance, live committed input, and reusable
  same-slot replacement input.
- `*_min_slice_debt` controls the smallest order slice.
- `*_max_orders` caps the number of live slices. If the cap is too low to
  express the full target depth with the configured minimum slice, Stitch leaves
  the remainder unquoted instead of flooding the live book.
- Configured liquidity is a ceiling. Stitch reads the side's input token balance
  and Permit2 allowance every quote tick, then posts only the funded portion.
  If inventory falls below the target, the ladder shrinks until the wallet is
  rebalanced or the config is lowered.
- Requotes reuse stable replacement slots for each side. When a slot is
  refreshed, Stitch reuses the slot's nonce and treats that slot's previous
  input as replaceable, so funded checks reserve only the additional input
  needed by the new quote. If a fill spends a slot nonce, Stitch rotates only
  that slot on the next retry.
- The default `stitch approve` mode pairs naturally with `"max"` because it
  grants Permit2 a max allowance once. `stitch approve --exact` is only for
  fixed numeric liquidity amounts.

All amounts are atomic token units. For a 6-decimal token:

| Human amount | Atomic value |
| ---: | ---: |
| 10 | `10000000` |
| 100 | `100000000` |
| 1,000 | `1000000000` |
| 50,000 | `50000000000` |

The buy side spends the `debt` token; the sell side spends the `collateral`
token. Raising total liquidity increases total quoted depth; raising the minimum
slice usually increases individual order sizes.

A fixed numeric size also caps depth when the wallet later grows: top up the
wallet without raising `*_total_liquidity_*` and the extra inventory never
reaches the book. Stitch logs
`wallet can back more than the configured size` (info level) when this happens.
If you want the book to track the wallet, use `"max"`; keep a fixed size only
when you deliberately reserve part of the wallet's inventory for something
else.

### Limit-Order Taker

Traders can rest their own limit orders in the same book Stitch quotes into.
The taker leg fills those orders on-chain when their price is at or beyond
your own quote: a trader selling the soft asset fills at or below your bid, a
trader buying it at or above your ask. There is no separate pricing — the
side spreads above are the margin, so a side without a spread is never taken.
Off by default; enable per pool:

```toml
[[pools]]
limit_taker_enabled = true
# limit_taker_min_profit_debt = "50000"  # per-order profit floor, atomic debt units
# limit_taker_max_orders = 10            # most orders per fill transaction
```

What one tick does when the leg is on:

1. Fetch the corridor's resting trader limit orders from the indexer, both
   directions.
2. Re-verify each order locally: the EIP-712 digest is recomputed from the
   served fields and the signature must recover to the claimed maker, so the
   indexer is never trusted with fund-moving inputs. Own orders and orders
   near expiry are skipped.
3. Price each order against your bid or ask, including the protocol's taker
   fee — read once from the chain at startup (the fee controller caps it at
   5 bps). If the fee can't be read, the leg stays off for the run rather
   than guessing.
4. Fill the profitable batch (most profitable first, capped by
   `limit_taker_max_orders` and your live token balance) with one on-chain
   transaction per direction. A just-filled order is on cooldown for a few
   minutes so a pending transaction can't double-fill it.

Costs and flows to know:

- **Taking costs gas** — one transaction per batch, plus a one-time ERC20
  approval of each spent token to the reactor, sent automatically before the
  first fill. This is a direct reactor allowance, separate from the Permit2
  approvals that back your quotes.
- **You pay the taker fee** when filling (on top of the trader's price); the
  profitability check already accounts for it. `limit_taker_min_profit_debt`
  adds a per-order floor, valued in atomic debt units, so dust fills don't
  burn gas for nothing.
- **Fills spend quoting inventory.** The taker leg spends from the same
  wallet your ladders quote; a fill shrinks the next ladder like any other
  balance change, and the received tokens land as inventory.
- `--dry-run` logs the batches the leg would fill without sending anything.

### Inventory-Lean Quoting

A fixed symmetric spread has a failure mode: one side keeps filling until the
wallet is 100% soft asset (or 100% stable), and the book goes one-sided until
someone manually rebalances. The lean fixes that by pricing off the wallet's
own inventory — and only the inventory, never price momentum or recent flow.

The inventory share `x` is the soft asset's value share of the wallet
(`0` = all stable, `1` = all soft), valued at fair. The quote rule:

- **Balanced (`x` 0.40–0.60):** quote fair ± `lean_base_bps`, both sides.
- **Soft-heavy (`x` 0.60–0.85):** the ask tightens linearly toward the floor
  (unload faster) while the bid widens by up to `lean_wide_bps` (accumulate
  slower). Stable-heavy mirrors it.
- **Critical (`x` > 0.85 or < 0.15):** the unloading side quotes at the floor;
  the accumulating side stops quoting. Already-posted orders age out through
  their TTL — there is no on-chain cancel.

Three hard rules hold regardless of the lean:

1. **No quote ever crosses fair.** Every offset is clamped to
   `lean_floor_bps`, so every fill is at fair or better by construction.
2. **The lean can't be whipsawed.** `x` is recomputed on observed fills and at
   most once per 60s otherwise, and the smoothed offsets move at most 0.5 bps
   per update.
3. **A fair jump > 25 bps in one tick pulls both quotes** for that tick;
   quoting resumes on the next one. (Feed staleness already pulls quoting —
   see `staleness_secs`.)

`lean_floor_bps` is the tightest honest spread: the p95 of your feed's error
vs live Pyth (fair = Pyth XAUt/USD ÷ USDT/USD for the gold corridor).
**Measure it — don't assume it.** Stitch refuses a lean config without it. If
your feed is a cached endpoint (e.g. the 60s `/price`), the honest floor is
around 3 bps — likely wider than a sub-1bp fixed spread you may be quoting
today, which just means that spread was tighter than the feed could support.

Roll out in two steps:

```toml
[[pools]]
# Step 1 — shadow: compute and log the lean quotes next to the live ones each
# tick ("lean shadow" log lines). No behavior change. Run it a couple of days
# and check every lean quote is on the correct side of fair and the x
# trajectory is sane.
lean_shadow = true
lean_floor_bps = 3.0   # REQUIRED: measured p95 feed error vs live Pyth, bps

# Step 2 — live: quote the book off the lean prices. Flip back to false and
# restart to revert instantly.
lean_enabled = false

# Tunables (defaults shown):
# lean_base_bps = 1.0  # balanced-zone half-spread
# lean_wide_bps = 3.0  # extra widening at the heavy inventory edge
```

With the lean live, the taker leg prices off the same lean bid/ask, and a
pulled side takes nothing. The configured `buy_offset_*` / `sell_offset_*`
spreads still gate which sides run and are what shadow mode logs as the live
comparison, but live lean pricing replaces them.

### MPC Wallet Signers

By default Stitch signs with the local private key (the hotwallet). An optional
`[signer]` section swaps that for an MPC wallet. Whichever signer is configured
handles every signature the bot makes: the EIP-712 limit orders and the on-chain
fill/approve transactions. You pick one backend for the whole bot.

Three options:

- **Local hotwallet** (default): the bot signs with `STITCH_PRIVATE_KEY` /
  `STITCH_PRIVATE_KEY_FILE`. Omit `[signer]` entirely, or set
  `provider = "local"`.
- **Turnkey**: a TEE-backed MPC wallet. Use it when you want MPC custody with no
  extra infra. It's a single synchronous API call per signature, made from
  inside the bot binary, so there's no sidecar to run. Each operator uses their
  own Turnkey org and API key.
- **MPCVault**: an MPC wallet that runs through a separate `client-signer`
  sidecar. Use it when your custody is already on MPCVault. It can't be run as
  one shared service for many operators: MPCVault binds one client-signer per
  vault and each operator needs their own vault and funds, so it's one sidecar
  per operator.

Secrets always come from the environment, never the config file (same rule as the
existing key). Each secret has a `_FILE` variant (a path) that takes precedence
over the raw value, exactly like `STITCH_PRIVATE_KEY_FILE` vs
`STITCH_PRIVATE_KEY`.

The operator wallet or vault still needs a little native gas for Permit2
approvals (`stitch approve`) regardless of the signer.

**Desktop app.** If you use `stitch-setup`, you don't need to edit any of this by
hand. The first-run wizard and the Settings screen both have a **Signer**
dropdown (hot wallet / Turnkey / MPCVault) that collects the fields below, writes
the `[signer]` section, stores each secret in an owner-only file, and points
`stitch.env` at it. Changing the signer in Settings rewrites all three and
restarts a running bot. The rest of this section is the reference for CLI and
server operators editing `stitch.toml` directly. MPCVault still needs its sidecar
running (below) either way.

Local hotwallet (same as omitting the section):

```toml
[signer]
provider = "local"
```

Env vars: `STITCH_PRIVATE_KEY` / `STITCH_PRIVATE_KEY_FILE` (unchanged).

Turnkey:

```toml
[signer]
provider = "turnkey"
organization_id  = "<turnkey org id>"
sign_with        = "0x<wallet account address or private key id>"
operator_address = "0x<the EVM address sign_with resolves to>"
api_base_url     = "https://api.turnkey.com"   # optional, this is the default
max_concurrent_signs = 8                        # optional
```

Env vars: `TURNKEY_API_PUBLIC_KEY` (not secret, plain env), and
`TURNKEY_API_PRIVATE_KEY` / `TURNKEY_API_PRIVATE_KEY_FILE` (secret).

For the full walkthrough (org, wallet account, API key, and the signing policy),
see [Turnkey signer setup](signer-turnkey.md).

MPCVault:

```toml
[signer]
provider = "mpcvault"
api_base_url         = "https://api.mpcvault.com"   # optional, default
vault_uuid           = "<mpcvault vault uuid>"
client_signer_pubkey = "ssh-ed25519 AAAA... "       # the sidecar's public key
operator_address     = "0x<the vault wallet EVM address>"
callback_listen_addr = "0.0.0.0:8088"   # optional, default; where the bot serves the approval callback
poll_timeout_secs    = 30                # optional
max_concurrent_signs = 4                 # optional
```

Env var: `MPCVAULT_API_TOKEN` / `MPCVAULT_API_TOKEN_FILE` (secret).

For the full walkthrough (MPCVault vault, API token, Client Signer, the sidecar,
and validation), see [MPCVault signer setup](signer-mpcvault.md).

MPCVault is a two-process setup: the bot plus the sidecar on the same host. The
bot runs an HTTP "callback approval" server at `callback_listen_addr`; the
MPCVault `client-signer` Docker container calls that callback to approve each
signing request.

#### MPCVault sidecar

1. Generate an ed25519 key for the sidecar:

   ```bash
   ssh-keygen -t ed25519 -C "mpcvault-client-signer" -f ./client-signer-key -N ""
   ```

   No passphrase. The public key (`client-signer-key.pub`) goes in
   `client_signer_pubkey` in `stitch.toml`. Register it in the MPCVault console
   under the vault's Team & policies, then approve the resulting "Key grant
   access" request in the MPCVault app.

2. Write the sidecar `config.yml`:

   ```yaml
   http-health:
     listening-addr: 0.0.0.0:8080
   vault-uuid: "<vault uuid>"
   ssh:
     private-key: |
       -----BEGIN OPENSSH PRIVATE KEY-----
       ...contents of client-signer-key...
       -----END OPENSSH PRIVATE KEY-----
     password: ""
   callback-url: "http://<bot host or container>:8088/callback"   # must reach the bot's callback_listen_addr
   ```

3. Run the sidecar next to the bot:

   ```bash
   docker run -d --name mpcvault-signer --restart unless-stopped \
     -p 8080:8080 -v "$(pwd)/config.yml:/config.yml:ro" \
     ghcr.io/mpcvault/client-signer:latest --config-path=/config.yml
   ```

4. Make sure the bot's `callback_listen_addr` is reachable from the sidecar
   container. If both are containers, share a network or use host networking; the
   `callback-url` must resolve to the bot.

The bot's callback fails closed: it approves (HTTP 200) only when the request's
signed raw-message `content` is a digest the bot currently has in flight, and
rejects (403) otherwise. It correlates on that signed field specifically, not a
substring of the body, so a request the bot didn't create can't be signed. The
callback accepts both the protobuf `SigningRequest` (`application/octet-stream`)
the client-signer POSTs and JSON. The exact protobuf field numbers should still
be confirmed against a running sidecar; see [MPCVault signer setup](signer-mpcvault.md) for the
security model and known limitations.

#### Deployment notes

On AWS Fargate the provided CloudFormation template supports local and Turnkey
directly: fill the matching keys in the `<BotName>/operator` secret
(`TURNKEY_API_PUBLIC_KEY`, `TURNKEY_API_PRIVATE_KEY`, or `MPCVAULT_API_TOKEN`).
MPCVault additionally needs the client-signer sidecar running alongside, which
suits the EC2/docker-compose or systemd host setup better than the
single-container Fargate Quick Create.

## Troubleshooting

### First Checks

Check the binary:

```bash
stitch --version
stitch --help
```

Run without posting live orders:

```bash
STITCH_PRIVATE_KEY_FILE=~/Stitch/stitch.key \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

Increase log detail:

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  STITCH_PRIVATE_KEY_FILE=~/Stitch/stitch.key \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

If running under systemd:

```bash
journalctl -u stitch -f
systemctl status stitch
```

### No Orders Are Posting

Check these in order:

1. The side is enabled. A side needs a spread plus a size.
2. The wallet has the token it spends.
3. The wallet has granted Permit2 approval for that token (`stitch approve`).
4. `permit2`, `reactor`, `indexer_url`, and `chain_id` match the target chain.
5. The feed is fresh and reachable.
6. The spread is not so wide that your orders are outside the expected fill
   range.
7. `refresh_threshold_bps` has not prevented an unchanged quote from being
   reposted (0 — the default — re-posts every tick; see
   [TWAP Quoting](#twap-quoting-smoothed-center)).

For the buy side, the wallet spends the pool's `debt` token. For the sell side,
the wallet spends the pool's `collateral` token.

### The Taker Leg Is Not Filling

The limit-order taker only acts when everything lines up; silence usually
means one of these:

1. `limit_taker_enabled = true` is missing on the pool, or the side that
   would price the fill has no spread configured (the taker prices with your
   bid/ask; no spread on a side means that direction is never taken).
2. Startup logged `could not read the reactor fee; taker leg disabled for
   this run` — the fee read failed, so the leg is off until a restart with a
   working RPC.
3. No resting order actually clears your spread after the taker fee, or
   `limit_taker_min_profit_debt` filters what does.
4. The wallet's balance of the spent token can't cover the order plus fee.
5. The order was just filled or attempted — it's on the resubmit cooldown.

`--dry-run` logs `would fill resting limit orders` with the batch it planned,
which pins down whether the problem is discovery, pricing, or funding.

### Update Does Not Work

`stitch --update` only works for binaries installed through the release
installer. A binary built with `cargo build` does not have an install receipt,
so it cannot self-update.

Use the release installer or download the latest binary from GitHub Releases.

### Build From Source

Source builds are useful for local verification:

```bash
cargo build --release
cargo test
```

The compiled binary is at:

```bash
target/release/stitch
```

Source-built binaries can run normally, but they do not support installer-based
self-updates.

### Corridor Catalog

The desktop app (`stitch-setup`) and `stitch init` ship pre-filled configs for
every supported corridor:

- **cNGN / USDT on BNB Smart Chain** — the NGN stablecoin against USDT on BSC (chain 56).
- **XAUt / USDT on Ethereum** — Tether Gold against USDT on Ethereum (chain 1).
- **wARS / USDT on Celo** — the ARS stablecoin against USDT on Celo (chain 42220).
- **wBRL / USDT on Celo** — the BRL stablecoin against USDT on Celo (chain 42220).

Each config is embedded in the binary and written verbatim when you pick a
corridor during setup. The wallet key is never stored in the TOML. The RPC URL in
each template is a free public endpoint; swap it for your own if you have one.

If you are running a different corridor, copy `stitch.example.toml` and edit it
directly. The setup app and `stitch init` are convenience wrappers for the
supported corridors; any valid `stitch.toml` works with the bot.

### Safe Restart Checklist

Before restarting live:

1. Run `stitch --config ~/Stitch/stitch.toml --dry-run`.
2. Confirm the feed price is current.
3. Confirm balances and Permit2 approvals.
4. Confirm order sizes are in atomic units.
5. Restart the process or systemd service.

For systemd:

```bash
sudo systemctl restart stitch
journalctl -u stitch -f
```
