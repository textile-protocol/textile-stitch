# Stitch Advanced Guide

Configuration reference, tuning, and troubleshooting for operators who need more
than the [README](README.md) quick start. If you installed with the AI prompt,
the primary settings are already filled in — this guide is for understanding and
changing them.

## Configuration Reference

Start from [stitch.example.toml](stitch.example.toml). Stitch reads the config at
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
- `*_min_slice_debt` controls the smallest order slice.
- `*_max_orders` caps the number of live slices. If the cap is too low to
  express the full target depth with the configured minimum slice, Stitch leaves
  the remainder unquoted instead of flooding the live book.

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

### Settlement Closing

Stitch runs settlement closing alongside market making by default. The install
flow fills the top-level `subgraph_url` and each pool's closer fields from the
deposit catalog:

```toml
subgraph_url = "https://api.textilecredit.com/subgraph?chainId=8453"

[[pools]]
closer_pool = "0x0000000000000000000000000000000000000000"
floor_ray = "500000000000000000000000"      # 0.05% opening rate (RAY)
buffer_ray = "20000000000000000000000000"   # 2% decaying buffer (RAY)
window_secs = 432000
min_margin_collateral = "0"
max_positions_per_fill = 10
discover_first = 200
skip_past_window = true
```

`floor_ray` / `buffer_ray` / `window_secs` mirror the pool's on-chain auction
params. Advanced operators can omit the closer fields to run market making only,
but the recommended setup keeps both jobs enabled.

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
   reposted.

For the buy side, the wallet spends the pool's `debt` token. For the sell side,
the wallet spends the pool's `collateral` token.

### Settlement Closing Is Not Running

Settlement closing should run in the default setup. If it is not running, first
confirm both required config groups are present:

- top-level `subgraph_url` is set;
- the pool has `closer_pool`, `floor_ray`, `buffer_ray`, and `window_secs`.

Also check:

- the RPC URL is reachable;
- the wallet has enough debt token to close positions;
- `max_positions_per_fill` is not too low for your desired batch size;
- `min_margin_collateral` is not filtering every candidate;
- `skip_past_window` is not excluding the positions you expected to close.

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
