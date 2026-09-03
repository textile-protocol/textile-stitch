# Migrating from the public ladder to Swap (RFQ)

Stitch used to quote by resting a ladder of signed orders on the public filler
book. Swap no longer reads that book — it asks makers for a firm quote — so a
ladder posted today is invisible to takers while still holding your inventory
behind live orders.

This guide moves an existing bot from the ladder to RFQ. The mechanical part is
a few minutes and one restart; the wait is Textile approving your maker, which
happens in between. Your ladder keeps running until then.

- [Getting access](#getting-access) — Connect registers you; Textile has to approve you before you quote
- [Panel and Desktop operators](#panel-and-desktop)
- [Standalone CLI operators](#standalone-cli) — `stitch` from a terminal, systemd, or Docker without the panel
- [What changes in your config](#what-changes)
- [Verifying it worked](#verifying)
- [Rolling back](#rolling-back)

<a id="getting-access"></a>

## Getting access

Connect registers your maker and issues a credential. It does **not** put you on
the tape. Textile seats corridors only after a person approves the maker, so
every new maker starts registered and dark:

1. **Connect.** The bot signs `MakerEnroll` with its funding wallet and gets a
   maker id and key back. No corridor is enabled yet.
2. **Request access.** Tell Textile who you are. An email address you own is
   required (placeholder and throwaway domains are refused); WhatsApp is
   optional. That sends ops a dossier (wallet, chain, maker id, corridor,
   contact) with Approve and Reject, and sends *you* a link to confirm the
   address. Click it — the request is reviewed either way, but it shows up as
   unverified until you do. You ask once: the same request covers every bot
   you run on this wallet.
3. **They approve**, which seats the maker on every RFQ corridor on every
   chain — including pairs Textile lists later. You get an email saying so.
4. **Check status.** Your bot picks the seat up and goes live. This does not
   rotate your key.

Until step 3, `[rfq]` stays off and your ladder keeps running — so a leftover
book bot is never dark in the gap. That is the whole reason to leave the ladder
*after* you are approved, not before.

<a id="panel-and-desktop"></a>

## Panel and Desktop

1. Open the bot, go to **Settings**.
2. In the **RFQ** card, press **Connect**. The bot signs a `MakerEnroll` message
   with its own funding wallet; Textile registers the maker and the panel writes
   `rfq-api.key` beside the config. You never paste an id or key.
3. Ask Textile to seat the maker. Newer panels have a **Request access** form
   in the RFQ card — an email address you own, WhatsApp optional — and the
   card then reads "Access requested". Confirm the address from the link they
   email you. If your panel has no such form, mail `contact@textilecredit.com`
   with the maker id from `[rfq].maker_id`, the chain, and the pair.
4. Wait for approval. When it lands, press **Check status** if your panel has
   it — otherwise press **Reconnect**, which re-enrolls and picks the seat up
   (it rotates the maker key; Check status does not). Either way the card goes
   live and `[rfq]` is enabled.
5. If the bot was on the ladder, the card now offers **Switch to RFQ only**.
   Take it — Connect on a leftover book bot writes the credential and leaves the
   ladder alone, so this is the step that actually moves you.
6. Save. The bot restarts and quotes Swap.

If your bot still uses the **flat file layout** (a single `stitch.<name>.toml` in
the bots root, from an adopted container), migrate it to the per-bot directory
layout first — the panel will tell you. A flat-layout container only mounts the
config and the signer key, so it cannot read the credential Connect writes.

<a id="standalone-cli"></a>

## Standalone CLI

No panel, no Docker socket — just the `stitch` binary and a `stitch.toml`.

```bash
cd ~/Stitch
set -a; . ./stitch.env; set +a      # exports STITCH_PRIVATE_KEY_FILE
stitch connect --config ./stitch.toml
```

`stitch connect` does exactly what the panel's Connect button does: signs
`MakerEnroll` with the wallet in your env, registers with Textile, writes
`rfq-api.key` next to the config (owner-only), and writes the `[rfq]` block into
`stitch.toml`.

On a maker Textile has not approved yet — which is every new maker — it prints
that you are registered and waiting, leaves `[rfq]` off, and leaves your ladder
running. That is the expected first result, not an error.

### Requesting access without the panel

The Request access form lives in the Stitch panel; there is no `stitch` verb for
it yet. Two ways through:

- **Run the panel once.** Point it at your existing bot directory, Connect (or
  it picks up the credential you already have), ask for access, and pick the
  seat up after approval. You can shut the panel down afterwards — the seat
  lives on the venue, not in the panel.
- **Ask Textile directly.** Send `contact@textilecredit.com` your maker id (it
  is in `[rfq].maker_id` after Connect), the chain, and the pair. Ops can seat
  you by hand.

Once approved, re-run `stitch connect` to pick the seat up:

```bash
stitch connect --config ./stitch.toml
```

That writes the corridor slug and enables `[rfq]`, and — because a bot that
quotes Swap should not also rest a ladder — sets `book_enabled = false`.

Then restart however you run it:

```bash
sudo systemctl restart stitch      # systemd
# or just re-run: stitch --config ./stitch.toml
```

**Re-running `stitch connect` rotates your maker key.** It is safe in the sense
that the config and `rfq-api.key` are rewritten together from the fresh
response, so the bot keeps working — but the old key stops working the moment
you do it. Don't run it on a live bot for no reason, and don't run it on two
machines pointed at the same maker. The panel's Check status exists precisely
because it applies the seat *without* rotating; the CLI has no equivalent yet.

The other case `connect` reports is a **rejected or flagged maker**: the
credential is saved but no quote requests will ever arrive. Talk to Textile
before re-running.

### Pointing at a different venue

`connect` derives the enroll endpoint from `[rfq].url`, falling back to
`indexer_url`. Override it when you need to:

```bash
stitch connect --config ./stitch.toml --venue https://api.textilecredit.com/v2/maker/enroll
```

### MPC signers

Nothing special. `connect` uses the same signer the bot runs with, so a Turnkey
or MPCVault bot enrolls the wallet it actually trades from. Make sure the signer
env is exported first (`stitch.env` handles this if you wrote it with
`stitch init`).

<a id="what-changes"></a>

## What changes in your config

`connect` adds an `[rfq]` block and flips one top-level key:

```toml
book_enabled = false   # was absent (defaults true) or explicitly true

[rfq]
enabled = true
url = "wss://api.textilecredit.com/v2/maker/stream"
maker_id = "mk_..."
api_key_env = "STITCH_RFQ_API_KEY"
validation_contract = "0x..."
```

Your spreads, liquidity, and per-pool sizing are untouched — RFQ prices off the
same `buy_offset_bps` / `sell_offset_bps` and caps sizes with the same
`buy_total_liquidity_debt` / `sell_total_liquidity_collateral`.

The credential itself is **not** in the TOML. It goes in `rfq-api.key` beside
the config, and the bot reads it from there (or from `STITCH_RFQ_API_KEY` /
`STITCH_RFQ_API_KEY_FILE` if you'd rather manage it yourself).

What stops applying once the ladder is off: `ttl_secs`, `refresh_threshold_bps`,
`buy_max_orders` / `sell_max_orders`. Those are ladder mechanics; RFQ uses the
venue's own quote TTL. TWAP and inventory lean keep applying to the taker leg
but do **not** reach RFQ, which quotes off the latest feed print plus your
spreads.

Limit orders are unaffected either way. Filling traders' resting limit orders is
the taker leg (`limit_taker_enabled`), which runs independently of both the
ladder and RFQ.

<a id="verifying"></a>

## Verifying it worked

On start you should see the responder come up:

```
starting RFQ responder  url=wss://... maker_id=mk_... corridors=["cngn-usdt-bsc"]
public ladder off — this bot will not rest orders on the book
```

If instead you see `RFQ responder NOT started: the maker API key is missing`,
the bot cannot find `rfq-api.key` — check it sits next to `stitch.toml` and is
readable by the user the bot runs as.

If the responder starts but no requests arrive, you are registered without a
seat — that is the approval step, not a bug. Check status in the panel (or ask
Textile) rather than restarting the bot.

A bot with the ladder off and no working credential quotes nothing at all. The
panel refuses to Start or Restart one in that state; on the CLI, watch for that
log line.

<a id="rolling-back"></a>

## Rolling back

The ladder is still there, just off.

**Panel / Desktop:** Settings has a collapsed **Legacy** card at the bottom of
the bot page with the `book_enabled` switch. Turning it on restarts the bot back
onto the public book.

**CLI:** set `book_enabled = true` in `stitch.toml` (or delete the line — it
defaults to true) and restart.

Either way, leave `[rfq]` in place. `enabled = false` parks the responder
without discarding your maker id, so switching back is one edit rather than
another enrollment. Your approval is not lost either way: it is per maker, not
per pair, so switching back never needs a second review.

Orders you already signed stay live on the book until they expire, whichever
direction you move.
