![Stitch README header](assets/stitch-readme-header.png)

# Stitch

Stitch is the Textile filler-network operator bot (binary: `stitch`). It
market-makes the Settlement pairs:

- **Green leg (two-sided)** — keeps live, funded UniswapX LimitOrders in the
  off-chain order book on _both_ sides of each pair: a **bid** (buy cNGN below
  the mid — "buy low") and an **ask** (sell cNGN above the mid — "sell high").
  A `/s/trade` sell fills against these via the reactor's `executeBatch`. The
  bid serves a trader selling cNGN; the ask serves a trader selling USDT (the
  reverse direction). Each side is independent — run one or both.
- **Blue leg** — closes settlement auction positions on-chain via `pool.fill()`
  to earn the closer fee (enabled per pool when `closer_pool` + `subgraph_url`
  are set).

Each side's spread is the operator's strategy, expressed however they prefer:
`*_offset_bps` (relative basis points) or `*_offset_abs` (absolute, in the
soft-per-stable price — collateral per debt, e.g. cNGN/USDT, COPM/USDT — so it
works for any pair). The bid needs the operator funded in the stable (USDT) with
a Permit2 approval on it; the ask needs the soft asset (cNGN) approved instead.

The wallet key is never in the config — pass `STITCH_PRIVATE_KEY` in the env.
See `stitch.example.toml` for every field.

## Install & run (operators)

The bot is a single binary. It reads `stitch.toml` fresh on every start (change
the file, restart, new values apply — there's no hot reload), takes the wallet
key from `STITCH_PRIVATE_KEY`, and traps `SIGTERM`/Ctrl-C to exit cleanly after
the current tick.

```bash
STITCH_PRIVATE_KEY=0x... stitch --config stitch.toml
STITCH_PRIVATE_KEY=0x... stitch --config stitch.toml --dry-run   # sign + log, don't post
stitch --version    # print version
stitch --update     # self-update to the latest release
stitch --help
```

### Linux server (systemd)

Run it as a service so it restarts on crash and on reboot, with the key in a
mode-600 `EnvironmentFile` rather than the unit. See `deploy/stitch.service`
and `deploy/stitch.env.example` — the unit header has the copy-paste install
steps. Logs go to `journalctl -u stitch -f`; a config change is
`sudo systemctl restart stitch`.

### Windows

Drop `stitch.exe` next to `stitch.toml`, set the key, and run it:

```powershell
$env:STITCH_PRIVATE_KEY="0x..."
.\stitch.exe --config stitch.toml
```

To keep it running across reboots, register it as a Windows service with
[NSSM](https://nssm.cc) (`nssm install stitch`) or a Task Scheduler entry
set to run at startup. Set `STITCH_PRIVATE_KEY` as the service/user environment
variable, not on the command line.

## Upgrading

`stitch --update` checks the latest GitHub release and replaces the binary
in place. It only works for a binary installed via the release installer (it
reads the install receipt) — a `cargo build` binary has no receipt and prints a
clear message instead. On startup the bot also logs a one-line nudge when a
newer release exists, so operators know without having to check.

Under systemd, `--update` swaps the binary; `sudo systemctl restart stitch`
picks it up. (Releases and the one-line install/update scripts are produced by
the `dist` release workflow — see the build/release setup before the first tag.)

## Source sync and releases

The monorepo is the source of truth while the public repository at
`textile-protocol/textile-stitch` is the distribution mirror. Changes merged to the
monorepo `dev` branch under `packages/stitch-bot/**` run
`.github/workflows/sync-stitch.yml`, which tests the crate, creates a source
export artifact, and pushes the flattened package to the public repo.

The sync workflow needs a monorepo secret named `STITCH_SYNC_TOKEN`. Use a
GitHub App token or PAT with write access to `textile-protocol/textile-stitch`;
because the export includes `.github/workflows/**`, the token also needs
workflow-write permission (`workflow` scope for a classic PAT).

Do not edit generated mirror files directly in `textile-protocol/textile-stitch`.
Make changes here, merge them to `dev`, and let the sync workflow update the
public repo. The public repo then runs its own CI on `main` and uploads a Linux
binary artifact for each synced change. To publish operator-facing binaries,
tag the synced public repo commit with a version such as `v0.1.0`; that runs the
cargo-dist release workflow and publishes the binaries and installers that power
`stitch --update`.

## Run against the local chain (e2e)

The local Docker stack runs Stitch by default:

```bash
docker compose -f docker/docker-compose.yml up
```

The app deploys the local chain, exposes prices at `/api/price`, and then the
`stitch` service starts. `run-local.mjs` funds **Hardhat account #1** on both
tokens (mints + approves Permit2 on USDT for the bid and cNGN for the ask), so
both sides post with no extra setup.

One command from the repo root — it funds the operator, regenerates
`stitch.local.toml` (one `[[pools]]` block per deployed corridor, each with its
own `feed_url` pointed at `/api/price?pair=<key>`), waits for the app price
endpoint, then runs the bot:

```bash
node packages/stitch-bot/scripts/run-local.mjs
```

The app endpoint serves `{ price, timestamp }` per corridor via `?pair=<key>`,
read live from each corridor's on-chain oracle:

```bash
curl -s 'http://localhost:8916/api/price?pair=cngn-usdt'
```

It quotes **all** the local corridors (cNGN/USDT, cNGN/USDC, COPM/USDT…), each
priced off its own oracle — so every pair and direction on `/s/trade` has bids,
not just cNGN/USDT. You'll see `posted order label="bid"` /
`label="ask"` lines for each.

> Local oracles other than cNGN/USDT have no keepalive job, so they go stale
> after 30 min and their reads (the `/s/trade` rate and `/api/price`) start
> reverting. `run-local.mjs` re-pushes every forward oracle at startup and every
> 20 min to keep them warm.

Both directions of any corridor have depth. Local depth is laddered by default:
`TOTAL_ORDER_SIZE_USD` controls the per-side total and `MIN_ORDER_SIZE_USD`
controls the smallest slice, defaulting to 20 and 10.

```bash
# forward (sell cNGN): collateralAsset=<cNGN>, debtAsset=<USDT> → the bid
# reverse (sell USDT): collateralAsset=<USDT>, debtAsset=<cNGN> → the ask
curl -s localhost:8916/api/graphql -H 'content-type: application/json' \
  --data '{"query":"query{fillerOrderBook(chainId:31337,collateralAsset:\"<cNGN>\",debtAsset:\"<USDT>\"){inputToken outputToken inputAmount outputAmount rateRay}}"}'
```

On `/s/trade`, **either direction** now fills: pick a **−1% / −2% / −5%** chip
(the bot quotes 0.5% off the mid, so Market is just too strict) and enter a sell
amount above the bid/ask size. The strip shows the fill; Confirm runs the swap.

Knobs (env): `OFFSET_BPS` / `SELL_OFFSET_BPS` (the two spreads),
`TOTAL_ORDER_SIZE_USD` (per-side depth), `MIN_ORDER_SIZE_USD` (smallest ladder
slice), `MAX_LADDER_ORDERS`, `STITCH_PRIVATE_KEY`, `FEED_URL`, `INDEXER_URL`,
`BLOCKCHAIN_RPC_URL`. `ORDER_SIZE_USD` is still accepted as a backwards-
compatible alias for `TOTAL_ORDER_SIZE_USD`. The addresses rotate on every
docker restart; `run-local.mjs` re-reads them each run, so just rerun it.

## Tests

```bash
cargo test --manifest-path packages/stitch-bot/Cargo.toml
# on-chain tx submission against a local node (opt-in):
cargo test --manifest-path packages/stitch-bot/Cargo.toml -- --ignored
```
