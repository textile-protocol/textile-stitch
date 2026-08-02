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

Two recommended paths — pick by need:

1. **[Desktop app](#option-1--desktop-app)** — menu bar / system tray (plus a
   Dock icon and control window on macOS) on your computer. No Docker, no
   terminal. Opens the Stitch panel in your browser.
2. **[Server / Docker](#option-2--server--docker)** — always-on host or Tailscale
   access from other devices. One command, then the same web UI.

### Option 1 — Desktop app

No Docker and no terminal. [Download the release for your OS](https://github.com/textile-protocol/textile-stitch/releases) and open Stitch:

- **macOS**: download `Stitch.dmg`, open it, drag Stitch into Applications, and
  launch it from there. It appears in the menu bar and Dock, opens a small
  control window, starts the local panel (process runtime), and opens
  `http://127.0.0.1:8420`. Copy the panel password from the menu or window if
  you need it again. **Hide Dock icon** is available in the tray / window and
  is off by default.
- **Windows**: unzip the release and double-click `stitch-desktop.exe`. It sits
  in the system tray, opens the control window, and opens the same local panel.
- **Linux**: extract and run `stitch-desktop` (or the bundled `stitch.desktop`
  entry). For a headless server, prefer Option 2.

In the browser: **Add a bot**, pick a corridor, paste your operator wallet key,
approve allowances, dry-run, then Start. Use **Start at login** in the menu /
tray / window if you want the panel (and any bots left running) to come back
after a reboot — login starts stay in the tray and skip opening a browser tab
and the control window. Quit Stitch from the menu / tray / window to stop the
panel. For unattended 24/7 quoting on a server, use Option 2.

### Option 2 — Server / Docker

For a server or any Docker host that should stay up. One command:

```bash
# macOS / Linux — prefer a release tag + checksum (see docs/install-panel.md). Quick path:
curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh | sh
```

```powershell
# Windows PowerShell + Docker Desktop (local mode):
irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex
```

It asks whether you're installing on a **local computer** (password login at
`http://127.0.0.1:8420`) or a **server** (Tailscale on Linux, so you can open it
from your other devices). Then it starts the web UI. Add a bot with a wizard,
start and stop it, edit settings, approve allowances, tail logs — all in the
browser.

You need Docker with Compose v2. Server mode also needs a Tailscale account
(the free tier is enough) and a Linux Docker host. Nothing is compiled: it runs
the published image.

Then open the URL it prints, click **Add a bot**, pick a corridor, paste your
operator wallet key, and approve the router allowance from the bot's page before
starting it.

Already running bots from your own `docker-compose.yml`? Point Stitch at that
directory and it adopts them as they are — nothing is restarted or rewritten.

Everything else — custom reverse proxy, building from source — is in
[install-panel.md](docs/install-panel.md). You shouldn't need it for the above.

## Other ways to install

- [Install with an AI agent](#install-with-an-ai-agent) — your coding agent
  installs the panel; you finish bot setup in the web UI.
- [Docker](docs/install-docker.md) — the Stitch on its own, without Stitch panel.
- [Stitch (web)](docs/install-panel.md) — the advanced and manual routes for
  Option 2 above.
- [Desktop app (menu bar / tray)](docs/install-desktop.md) — Option 1 in detail.
- [Manual install — macOS](docs/install-macos.md)
- [Manual install — Windows](docs/install-windows.md)
- [Manual install — Linux](docs/install-linux.md) — includes the systemd service setup.

### Install with an AI agent

Your coding agent installs the Stitch panel and opens the web UI. You finish
setup in the browser (add a bot, wallet, approvals, dry run, start). Later the
agent can help operate an existing install on request.

- **Claude Code** — paste:

  > `curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/.claude/skills/stitch/SKILL.md --create-dirs -o ~/.claude/skills/stitch/SKILL.md` — run that as-is (don't WebFetch the URL). After it succeeds, tell me to run `/stitch`.

  Then run `/stitch`. With the repo checked out, Claude Code finds the skill
  automatically.

- **Codex** — paste:

  > Install the stitch skill from
  > https://github.com/textile-protocol/textile-stitch/tree/main/.codex/skills/stitch
  > After it succeeds, tell me to restart Codex and ask: `Use the stitch skill to install Stitch.`

  Restart Codex, then ask: `Use the stitch skill to install Stitch.`

<details>
<summary>Using a different agent?</summary>

Paste this into Claude, GPT, Gemini, or any agent with terminal access to the
machine where Stitch should run:

```text
Help me install Textile Stitch (the web UI panel only).

Read the full install prompt and follow it in full:
https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/docs/AI_INSTALL_PROMPT.md
If you can't fetch that URL, read docs/AI_INSTALL_PROMPT.md directly from the
textile-protocol/textile-stitch repo (main branch) instead. Don't guess from
other sources.
Ask whether I'm installing on a local computer (password) or a server
(Tailscale), install the panel, open the web app, and stop. I configure bots
in the browser myself.
```

The full copyable prompt is in [AI_INSTALL_PROMPT.md](docs/AI_INSTALL_PROMPT.md).

</details>

Running the binary directly without the panel? `stitch init` writes `stitch.toml`,
`stitch.env` and an owner-only `stitch.key` for a chosen corridor. The per-OS
guides cover it.

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

The desktop / panel UI writes all of this for you: Add a bot and Settings have a
**Signer** dropdown (hot wallet / Turnkey / MPCVault) that collects the fields
below, drops the secret in an owner-only file, and points `stitch.env` at it. The
manual `[signer]` fields below are for CLI and server operators editing
`stitch.toml` by hand.

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
