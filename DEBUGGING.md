# Debugging Stitch

This guide is for operators who need to verify that Stitch is configured,
quoting, posting, or closing correctly.

## First Checks

Check the binary:

```bash
stitch --version
stitch --help
```

Run without posting live orders:

```bash
STITCH_PRIVATE_KEY=0x... stitch --config stitch.toml --dry-run
```

Increase log detail:

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  STITCH_PRIVATE_KEY=0x... \
  stitch --config stitch.toml --dry-run
```

If running under systemd:

```bash
journalctl -u stitch -f
systemctl status stitch
```

## Price Feed

The feed must return fresh JSON:

```bash
curl -s "https://your-feed.example/cngn-usdc"
```

Expected shape:

```json
{ "price": 1234.56, "timestamp": 1760000000 }
```

Check:

- `price` is the soft-per-stable price for that pair.
- `timestamp` is a Unix timestamp.
- The timestamp age is less than `staleness_secs`.
- Each pool with a different price has its own `feed_url`.

If the feed is stale, Stitch skips quoting rather than posting orders at an old
price.

## No Orders Are Posting

Check these in order:

1. The side is enabled. A side needs a spread plus a size.
2. The wallet has the token it spends.
3. The wallet has granted Permit2 approval for that token.
4. `permit2`, `reactor`, `indexer_url`, and `chain_id` match the target chain.
5. The feed is fresh and reachable.
6. The spread is not so wide that your orders are outside the expected fill
   range.
7. `refresh_threshold_bps` has not prevented an unchanged quote from being
   reposted.

For the buy side, the wallet spends the pool's `debt` token. For the sell side,
the wallet spends the pool's `collateral` token.

## Orders Are Too Small Or Too Large

All config amounts are atomic token units.

For a 6-decimal token:

| Human amount | Atomic value |
| ---: | ---: |
| 10 | `10000000` |
| 100 | `100000000` |
| 1,000 | `1000000000` |
| 50,000 | `50000000000` |

For laddered liquidity:

- `*_total_liquidity_*` controls total depth for that side.
- `*_min_slice_debt` controls the smallest order slice.
- `*_max_orders` caps the number of live slices.

Raising total liquidity increases total quoted depth. Raising the minimum slice
usually increases individual order sizes.

## Settlement Closing Is Not Running

Settlement closing only runs when both conditions are true:

- top-level `subgraph_url` is set;
- the pool has `closer_pool`, `floor_ray`, `buffer_ray`, and `window_secs`.

Also check:

- the RPC URL is reachable;
- the wallet has enough debt token to close positions;
- `max_positions_per_fill` is not too low for your desired batch size;
- `min_margin_collateral` is not filtering every candidate;
- `skip_past_window` is not excluding the positions you expected to close.

## Update Does Not Work

`stitch --update` only works for binaries installed through the release
installer. A binary built with `cargo build` does not have an install receipt,
so it cannot self-update.

Use the release installer or download the latest binary from GitHub Releases.

## Build From Source

Source builds are useful for local verification:

```bash
cargo build --release
cargo test
```

The compiled binary is at:

```bash
target/release/stitch
```

Source-built binaries can run normally, but they do not support
installer-based self-updates.

## Safe Restart Checklist

Before restarting live:

1. Run `stitch --config stitch.toml --dry-run`.
2. Confirm the feed price is current.
3. Confirm balances and Permit2 approvals.
4. Confirm order sizes are in atomic units.
5. Restart the process or systemd service.

For systemd:

```bash
sudo systemctl restart stitch
journalctl -u stitch -f
```
