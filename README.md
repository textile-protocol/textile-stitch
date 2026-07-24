<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/assets/stitch-readme-header-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/assets/stitch-readme-header-light.png">
  <img alt="Stitch README header" src="https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/assets/stitch-readme-header-light.png">
</picture>

# Stitch

Stitch is the Textile operator bot for filler-network market making. It runs
as a single binary named `stitch`.

Stitch does one job for each configured pool by default, plus an optional
second:

- **Market making**: keep live buy and sell quotes for a configured
  soft-asset/stablecoin pair.
- **Limit-order taking** (opt-in): fill traders' resting limit orders on-chain
  when their price is at or beyond your own quote, priced by the same spreads
  as your market making.

## Contents

- [Quick Start](#quick-start)
- [Other ways to install](#other-ways-to-install)
- [How It Works](#how-it-works)
- [Requirements](#requirements)
- [Configuration](#configuration)
- [Security Notes](#security-notes)

## Quick Start

Two easy paths. Pick one.

### Option 1 — Desktop app

No terminal needed. [Download the release for your OS](https://github.com/textile-protocol/textile-stitch/releases) and open the Stitch app:

- **macOS**: download `Stitch.dmg` from the release, open it, and drag Stitch
  into Applications. Launch it from Applications so it runs from a stable path
  (this also avoids Gatekeeper's App Translocation). The image is Developer
  ID-signed and Apple-notarized, so it opens without a Gatekeeper prompt. You can
  also run the `stitch-setup` binary from a terminal.
- **Windows**: double-click `stitch-setup.exe`.
- **Linux**: run `stitch-setup` (or use the bundled `stitch.desktop` entry).

Pick a corridor, paste your operator wallet key, and click Create. The same
window then runs the bot: Start/Stop, a dry-run toggle, a Permit2 "Approve
tokens" button, live logs, and an Update button. Closing the window stops the
bot; for unattended 24/7 running, install it as a service (see the install guides
below).

### Option 2 — Install with an AI agent

Your coding agent collects the settings, writes the config,
runs a dry run, and starts live only after you confirm. Then it handles start,
stop, logs, parameter changes, and upgrades on request.

- **Claude Code** — paste:

  > `curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/.claude/skills/stitch/SKILL.md --create-dirs -o ~/.claude/skills/stitch/SKILL.md` — run that as-is (don't WebFetch the URL). After it succeeds, tell me to run `/stitch`.

  Then run `/stitch`. With the repo checked out, Claude Code finds the skill
  automatically.

- **Codex** — paste:

  > Install the stitch skill from
  > https://github.com/textile-protocol/textile-stitch/tree/main/.codex/skills/stitch
  > After it succeeds, tell me to restart Codex and ask: `Use the stitch skill to install and run Stitch.`

  Restart Codex, then ask: `Use the stitch skill to install and run Stitch.`

<details>
<summary>Using a different agent?</summary>

Paste this into Claude, GPT, Gemini, or any agent with terminal access to the
machine where Stitch should run:

```text
Help me install and configure Textile Stitch.

Read the full install prompt and follow it in full:
https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/docs/AI_INSTALL_PROMPT.md
If you can't fetch that URL, read docs/AI_INSTALL_PROMPT.md directly from the
textile-protocol/textile-stitch repo (main branch) instead. Don't guess from
other sources.
Use recommended defaults where safe, ask for values with no safe default, protect
STITCH_PRIVATE_KEY, run a dry run first, and do not start live operation until I
confirm.
```

The full copyable prompt is in [AI_INSTALL_PROMPT.md](docs/AI_INSTALL_PROMPT.md).

</details>

## Other ways to install

For running on a server, in a container, or by hand, see the dedicated guides:

- [Cloud (AWS Fargate)](docs/install-cloud.md) — operator-owned managed deployment.
- [Docker](docs/install-docker.md) — prebuilt image or build from source.
- [Manual install — macOS](docs/install-macos.md)
- [Manual install — Windows](docs/install-windows.md)
- [Manual install — Linux](docs/install-linux.md) — includes the systemd service setup.

Server operators can also run `stitch init` (after installing the binary) to
write `stitch.toml`, `stitch.env`, and an owner-only `stitch.key` for a chosen
corridor. The per-OS guides cover it.

## How It Works

Stitch reads `stitch.toml`, polls your configured price feed, signs UniswapX
limit orders, and posts those signed orders to the Textile indexer. The wallet
private key is read from `STITCH_PRIVATE_KEY_FILE`, or from `STITCH_PRIVATE_KEY`
for compatibility. If both are set, `STITCH_PRIVATE_KEY_FILE` takes precedence.

### Signer / wallet backend

By default Stitch signs with the local private key above (the hotwallet). You can
swap that for an MPC wallet by adding a `[signer]` section to `stitch.toml`.
Whichever signer you set handles every signature: the EIP-712 limit orders and
the on-chain fill/approve transactions. Pick one backend for the whole bot.
Secrets always come from the environment, never the config file, and each has a
`_FILE` variant (a path) that takes precedence over the raw value, the same as
`STITCH_PRIVATE_KEY_FILE` vs `STITCH_PRIVATE_KEY`.

The desktop app writes all of this for you: the `stitch-setup` first-run wizard
and its Settings screen have a **Signer** dropdown (hot wallet / Turnkey /
MPCVault) that collects the fields below, drops the secret in an owner-only file,
and points `stitch.env` at it. The manual `[signer]` fields below are for CLI and
server operators editing `stitch.toml` by hand.

- **Local hotwallet** (default): omit `[signer]`, or set `provider = "local"`.
  Uses `STITCH_PRIVATE_KEY` / `STITCH_PRIVATE_KEY_FILE`.
- **Turnkey** (`provider = "turnkey"`): a TEE-backed MPC wallet with no extra
  infra. One synchronous API call per signature, all inside the bot binary. Each
  operator uses their own Turnkey org and API key. Config fields:
  `organization_id`, `sign_with`, `operator_address`, optional `api_base_url` and
  `max_concurrent_signs`. Env vars: `TURNKEY_API_PUBLIC_KEY` (plain), and
  `TURNKEY_API_PRIVATE_KEY` / `TURNKEY_API_PRIVATE_KEY_FILE` (secret). Full setup
  walkthrough: [Turnkey signer setup](docs/signer-turnkey.md).
- **MPCVault** (`provider = "mpcvault"`): an MPC wallet that needs the MPCVault
  `client-signer` sidecar running next to the bot, one sidecar per operator.
  Config fields: `vault_uuid`, `client_signer_pubkey`, `operator_address`,
  optional `api_base_url`, `callback_listen_addr`, `poll_timeout_secs`, and
  `max_concurrent_signs`. Env var: `MPCVAULT_API_TOKEN` /
  `MPCVAULT_API_TOKEN_FILE` (secret). Full setup walkthrough (vault, API token,
  Client Signer, sidecar): [MPCVault signer setup](docs/signer-mpcvault.md).

The operator wallet still needs a little native gas for Permit2 approvals
(`stitch approve`) no matter which signer you use.

For market making, each configured pool can have:

- a **buy side**, where Stitch spends the stable/debt asset to buy the
  soft/collateral asset below the feed price;
- a **sell side**, where Stitch spends the soft/collateral asset to sell above
  the feed price.

With `limit_taker_enabled = true` on a pool, Stitch also checks the corridor's
resting trader limit orders every tick and fills the profitable ones on-chain:
a trader selling the soft asset fills at or below your bid, a trader buying it
at or above your ask, so your side spreads carry the margin. Each candidate's
signature is re-verified locally before anything executes, the protocol's
taker fee is read from the chain and priced into the decision, and fills cost
gas. See the
[limit-order taker reference in ADVANCED.md](docs/ADVANCED.md#limit-order-taker).

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
  `stitch approve` — see your platform's install guide above);
- a small native balance for gas (approvals and, for limit-order taking, the
  on-chain fill transactions).

## Configuration

Start from [stitch.example.toml](stitch.example.toml). A minimal default pool
configuration looks like this:

```toml
chain_id = 56
rpc_url = "https://bsc-rpc.publicnode.com"   # free public RPC; swap for your own if you have one
indexer_url = "https://api.textilecredit.com"
permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3"
reactor = "0x0000000000000000000000000000000000000000"
tick_interval_secs = 5

[feed]
url = "https://your-feed.example/cngn-usdt"
staleness_secs = 30

[[pools]]
collateral = "0xcngn0000000000000000000000000000000000c0"
collateral_decimals = 6
debt = "0xusdc0000000000000000000000000000000000d7"
debt_decimals = 6

buy_offset_bps = 10
buy_total_liquidity_debt = "max"
buy_min_slice_debt = "10000000"
buy_max_orders = 40

sell_offset_bps = 10
sell_total_liquidity_collateral = "max"
sell_min_slice_debt = "10000000"
sell_max_orders = 40

ttl_secs = 120
refresh_threshold_bps = 10
```

Amounts are atomic token units (e.g. 50,000 of a 6-decimal token is
`50000000000`). The default `*_total_liquidity_*` value is `"max"`, which quotes
all currently funded wallet inventory for that side. Use a fixed numeric amount
instead when you want a hard cap below the wallet balance. The total liquidity
fields are targets; if `*_max_orders` is too low to express the full target with the
configured minimum slice, Stitch leaves the remainder unquoted instead of
posting an oversized live book. Configured liquidity is also a ceiling: on each
quote tick, Stitch caps the posted bid or ask size to the operator wallet's
current token balance and Permit2 allowance for that side, so normal fills or
inventory transfers reduce the next ladder instead of causing the indexer to
reject an unfunded batch.
Requotes reuse the same replacement slots, so Stitch can refresh funded depth
without double-counting the ladder it is replacing.
When several corridors spend the same token (for example two pools that both
buy with USDC) and more than one of them is set to `"max"`, Stitch splits the
token's funded balance into even target shares on every tick, so an existing
corridor can't keep the whole wallet after another max side is added. For
the price-feed orientation, spread options, TWAP quoting (centering the
spread on a rolling average of the feed instead of the instantaneous value —
recommended for volatile pairs like WETH), ladder sizing, and the limit-order
taker, see the
[configuration reference in ADVANCED.md](docs/ADVANCED.md#configuration-reference).

## Security Notes

- Keep `STITCH_PRIVATE_KEY` out of `stitch.toml`, shell history, and process
  managers that expose command lines. Prefer `STITCH_PRIVATE_KEY_FILE` pointing
  at a 600-permission key file.
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
