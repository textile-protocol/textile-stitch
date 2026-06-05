<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/stitch-readme-header-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/stitch-readme-header-light.png">
  <img alt="Stitch README header" src="assets/stitch-readme-header-light.png">
</picture>

# Stitch

Stitch is the Textile operator bot for filler-network market making and
settlement closing. It runs as a single binary named `stitch`.

Stitch can do two jobs:

- **Market making**: keep live buy and sell quotes for a configured
  soft-asset/stablecoin pair.
- **Settlement closing**: close eligible settlement auction positions on-chain
  when the configured margin rules are met.

You can run either job by itself, or both jobs together for the same pool.

## Contents

- [Quick Start](#quick-start)
- [Manual Install](#manual-install)
- [How It Works](#how-it-works)
- [Requirements](#requirements)
- [Configuration](#configuration)
- [Running As A Service](#running-as-a-service)
- [Updating](#updating)
- [Security Notes](#security-notes)

## Quick Start

The recommended way to install Stitch is to have an AI assistant guide the
setup, collect the required operator settings, create the config, run a dry run,
and only start live operation after you confirm.

Use this short prompt to begin:

```text
Help me install and configure Textile Stitch.

Follow the full install prompt at
https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/AI_INSTALL_PROMPT.md
in full.
Use recommended defaults where safe, ask for values with no safe default, protect
STITCH_PRIVATE_KEY, run a dry run first, and do not start live operation until I
confirm.
```

For the full copyable prompt, open [AI_INSTALL_PROMPT.md](AI_INSTALL_PROMPT.md).
For troubleshooting and operational checks, see [DEBUGGING.md](DEBUGGING.md).

## Manual Install

Install the latest release:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/textile-protocol/textile-stitch/releases/latest/download/stitch-installer.sh | sh
```

Make sure the install directory is on your `PATH`, then check the binary:

```bash
stitch --version
```

Create a config file:

```bash
curl -L -o stitch.toml \
  https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml
```

Set the operator wallet key in the environment. Do not put the private key in
`stitch.toml`.

```bash
export STITCH_PRIVATE_KEY=0x...
```

Approve Permit2 to pull the tokens Stitch quotes. The operator wallet needs a
one-time approval for each input token (the `debt` token on the buy side, the
`collateral` token on the sell side). Without it, orders post but silently fail
to fill, and a live start refuses to run. Preview what's needed, then approve:

```bash
stitch approve --config stitch.toml --dry-run   # show what needs approving
stitch approve --config stitch.toml             # maximum allowance (recommended)
```

Maximum is the standard market-maker choice: approve once and never re-approve.
You're approving the canonical Permit2 contract, and the reactor can only pull
against orders you actually signed.

If you'd rather cap the allowance, use `--exact` to approve only the liquidity in
your config:

```bash
stitch approve --config stitch.toml --exact
```

Be aware of the trade-off: an exact allowance is consumed as orders fill, so once
it's used up Stitch keeps posting orders that **silently fail to fill** until you
re-approve, and you must re-run `stitch approve` every time you raise your
configured liquidity. Each approval is one gas-paying transaction per token, and
the command is idempotent (it skips tokens already approved).

Run once in dry-run mode before posting live orders:

```bash
stitch --config stitch.toml --dry-run
```

Run live:

```bash
stitch --config stitch.toml
```

## How It Works

Stitch reads `stitch.toml`, polls your configured price feed, signs UniswapX
limit orders, and posts those signed orders to the Textile indexer. The wallet
private key is read from `STITCH_PRIVATE_KEY`.

For market making, each configured pool can have:

- a **buy side**, where Stitch spends the stable/debt asset to buy the
  soft/collateral asset below the feed price;
- a **sell side**, where Stitch spends the soft/collateral asset to sell above
  the feed price.

For settlement closing, Stitch can also discover open positions through a
subgraph and submit `fill()` transactions when a close is profitable under your
configured margin and auction parameters.

Stitch reads the config at startup. After changing `stitch.toml`, restart the
process.

## Requirements

You need:

- an operator wallet private key;
- RPC access for the target chain;
- Textile indexer URL;
- a price feed endpoint returning fresh `{ "price": ..., "timestamp": ... }`;
- the Permit2 and reactor addresses for the target chain;
- funded token balances for the sides you enable;
- Permit2 approvals for the tokens Stitch will spend (set up with
  `stitch approve` — see [Manual Install](#manual-install));
- a small native balance for gas (approvals and, for closing, `fill()` txs);
- a subgraph URL if you enable settlement closing.

## Configuration

Start from [stitch.example.toml](stitch.example.toml). A minimal market-making
pool looks like this:

```toml
chain_id = 8453
rpc_url = "https://mainnet.base.org"
indexer_url = "https://api.textilecredit.com"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0x0000000000000000000000000000000000000000"
tick_interval_secs = 5

[feed]
url = "https://your-feed.example/cngn-usdc"
staleness_secs = 30

[[pools]]
collateral = "0xcngn0000000000000000000000000000000000c0"
collateral_decimals = 6
debt = "0xusdc0000000000000000000000000000000000d7"
debt_decimals = 6

buy_offset_bps = 150
buy_total_liquidity_debt = "50000000000"
buy_min_slice_debt = "10000000"
buy_max_orders = 150

sell_offset_bps = 150
sell_total_liquidity_collateral = "50000000000"
sell_min_slice_debt = "10000000"
sell_max_orders = 150

ttl_secs = 30
refresh_threshold_bps = 10
```

### Price Feed

The feed must return JSON with a price and Unix timestamp:

```json
{ "price": 0.000724, "timestamp": 1760000000 }
```

`price` is **debt per collateral** — the stable per soft, e.g. USDC per cNGN
(≈ 0.000724), **not** cNGN per USDC. Stitch quotes off it directly as the
bid/ask mid (USDT per cNGN); publishing the inverted soft-per-stable number
(e.g. 1382) makes the bot post wildly mispriced orders. (The absolute spread
below is the opposite orientation — soft per stable — by design; only the feed
`price` is debt per collateral.) If you quote multiple pairs with different
prices, set a `feed_url` inside each `[[pools]]` block instead of relying on the
top-level `[feed]`.

### Spreads

Each side needs one spread:

```toml
buy_offset_bps = 150
sell_offset_bps = 150
```

or an absolute spread in soft-per-stable units:

```toml
buy_offset_abs = 2.0
sell_offset_abs = 2.0
```

If both are set for a side, basis points win.

### Liquidity And Order Sizing

Stitch can post one order per side or a ladder of smaller orders. The example
uses laddered liquidity:

```toml
buy_total_liquidity_debt = "50000000000"
buy_min_slice_debt = "10000000"
buy_max_orders = 150

sell_total_liquidity_collateral = "50000000000"
sell_min_slice_debt = "10000000"
sell_max_orders = 150
```

Amounts are atomic token units. For a 6-decimal token:

| Human amount |  Atomic value |
| -----------: | ------------: |
|           10 |    `10000000` |
|          100 |   `100000000` |
|        1,000 |  `1000000000` |
|       50,000 | `50000000000` |

The buy side spends the `debt` token. The sell side spends the `collateral`
token.

### Settlement Closing

To enable settlement closing, add the closing fields to a pool and set the
top-level `subgraph_url`:

```toml
subgraph_url = "https://api.goldsky.com/.../textile-protocol/gn"

[[pools]]
closer_pool = "0x0000000000000000000000000000000000000000"
floor_ray = "20000000000000000000000000"
buffer_ray = "20000000000000000000000000"
window_secs = 432000
min_margin_collateral = "0"
max_positions_per_fill = 10
discover_first = 200
skip_past_window = true
```

Omit these fields to run market making only.

## Running As A Service

On Linux, run Stitch under systemd so it restarts after crashes and reboots.

Create local config and environment files:

```bash
curl -L -o stitch.toml \
  https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml

cat > stitch.env <<'EOF'
STITCH_PRIVATE_KEY=0x...
RUST_LOG=info
EOF

curl -L -o stitch.service \
  https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/deploy/stitch.service
```

Install files:

```bash
sudo install -m 0755 "$(command -v stitch)" /usr/local/bin/stitch
sudo mkdir -p /etc/stitch
sudo install -m 0644 stitch.toml /etc/stitch/stitch.toml
sudo install -m 0600 stitch.env /etc/stitch/stitch.env
sudo install -m 0644 stitch.service /etc/systemd/system/stitch.service
sudo systemctl daemon-reload
sudo systemctl enable --now stitch
```

Approve Permit2 before the first live start (the service won't run until the
input tokens are approved):

```bash
STITCH_PRIVATE_KEY=0x... stitch approve --config /etc/stitch/stitch.toml
```

View logs:

```bash
journalctl -u stitch -f
```

Restart after config changes:

```bash
sudo systemctl restart stitch
```

## Updating

If Stitch was installed from the release installer, update the binary in place:

```bash
stitch --update
```

Then restart the service:

```bash
sudo systemctl restart stitch
```

You can also download a new binary or installer from the latest GitHub Release.

## Security Notes

- Keep `STITCH_PRIVATE_KEY` out of `stitch.toml`, shell history, and process
  managers that expose command lines.
- Use a dedicated operator wallet.
- Fund only the inventory you intend Stitch to use.
- Review token balances, Permit2 approvals, spreads, and order sizes before
  running live. Set approvals with `stitch approve`; prefer a maximum allowance
  unless you have a specific reason to cap it.
- Use `--dry-run` after every config change that affects pricing or sizing.

## License

Stitch is free, open-source software licensed under the **GNU Affero General
Public License v3.0 or later** (`AGPL-3.0-or-later`). Copyright (c) 2026
Textile, Inc.

Copyleft: if you modify Stitch and distribute it — or run a modified version as
a network service — you must release your changes under the same license. See
[`LICENSE`](./LICENSE) for the full text.
