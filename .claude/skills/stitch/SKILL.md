---
name: stitch
description: Operate Stitch, the Textile operator bot — start, stop, restart, check status, tail logs, upgrade, change pool parameters (spreads, liquidity), and manage Permit2 approvals. Falls back to installing the Stitch panel (web UI) if nothing is set up yet. Use when asked to "start/stop/restart stitch", "run the bot", "check the bot", "stitch logs", "upgrade stitch", "change the spread/liquidity", or "install stitch".
---

# Operate Stitch

Stitch is the Textile operator bot: per-pool market making. New installs use the
**Stitch panel** (Docker web UI). This skill operates an existing layout and, if
nothing is set up yet, installs the panel only — bot config happens in the browser.

## Always start here

1. **Find the run layout and whether it's installed** — the next section.
2. **If it's not installed**, don't show the menu. Tell the operator, then with a
   single `AskUserQuestion` confirm they want to install the Stitch panel now. On
   yes, install by reading `docs/AI_INSTALL_PROMPT.md` and following it in full —
   see [Not installed yet](#not-installed-yet).
3. **If the Stitch panel is installed**, prefer pointing the operator at the web
   UI for add/start/stop/settings/approvals/logs. Only drive the API or Docker
   CLI if they ask you to do it from the terminal.
4. **If another layout is installed** (desktop app, local service, compose-only),
   ask what they want to do with `AskUserQuestion`, **one question at a time**.
   Never guess the action from a vague request — ask. Wait for each answer
   before the next question or any command.

Question-tool rules (same as the install prompt): one question per call, multiple
choice, most-likely option first, and the tool always adds a free-form answer so
the operator can type something else. In Codex use `request_user_input` (Plan
mode); if no question tool is available, ask the same thing in chat, still one at
a time.

**The menu** — keep every `AskUserQuestion` to **at most three options**,
most-likely first (so the same flow also renders on Codex's `request_user_input`,
which caps at 2–3). The tool always adds a free-form answer, so the operator can
type anything else.

First question — "What do you want to do with Stitch?":

- **Start / resume live** → [Start](#start) (Docker fleet: [Docker fleet](#docker-fleet))
- **Stop / pause** → [Stop](#stop)
- **Inspect or change it** — status/logs, parameters, approvals, or upgrade

If they pick **Inspect or change it**, ask a second `AskUserQuestion`:

- **Status and logs** → [Status and logs](#status-and-logs)
- **Change parameters** → [Change parameters](#change-parameters)
- **Approvals or upgrade**

If they pick **Approvals or upgrade**, ask a third:

- **Run Permit2 approvals** → [Permit2 approvals](#permit2-approvals)
- **Upgrade** → [Upgrade](#upgrade)

Then carry out the chosen action from the matching section, against the layout you
detected, honoring the golden rules throughout.

## First: find the install and how it runs

Figure out which layout is in use *before* doing anything. Check for the panel
first — that is the default install path now. A missing local `stitch` binary
does **not** mean "not installed" if `stitch-panel` is running.

Layouts, in the order to check:

- **Stitch panel** (`stitch-panel` container, often with a Tailscale sidecar):
  web UI for one or many bots on a Docker host. Local installs use password login
  at `http://127.0.0.1:8420`; server installs use Tailscale
  (`https://stitch-panel.<tailnet>.ts.net`). See [Docker fleet](#docker-fleet).
- **Docker fleet without panel**: bot containers only, driven with
  `docker compose`. See [Docker fleet](#docker-fleet).
- **Desktop app** (`stitch-desktop` — macOS `Stitch.app`, Windows
  `stitch-desktop.exe`, Linux `stitch-desktop`): menu-bar / tray controller that
  runs the local panel without Docker. Tray **Start at login** registers an OS
  login item (`--autostart`); the panel restores bots that were left running.
- **Foreground / manual** or **local service**: `~/Stitch/` (or
  `/etc/stitch-bot/` on Linux systemd) with `stitch.toml` / key / env.

Detect it: `docker ps --filter name=stitch-panel` (or image
`textile-stitch-panel`) means the panel is installed. Bot containers show under
`ghcr.io/textile-protocol/textile-stitch`. Local layouts: `stitch --version`,
`systemctl status stitch`, `launchctl list | grep -i stitch`, or a running
`stitch-desktop`/`Stitch` / `stitch-panel` process. Cloud: ECS service
`<bot>-stitch`. Only if none of those exist is it genuinely not installed — then
see [Not installed yet](#not-installed-yet).

## Golden rules — every operation

- Never print, echo, or pass the private key on a command line. It lives in a
  `chmod 600` key file; the process reads it via `STITCH_PRIVATE_KEY_FILE` from
  the env file. To run in the foreground, load the env file
  (`set -a; . <env>; set +a`) — never `KEY=$(...) stitch ...`.
- Stitch reads config only at startup. Any `stitch.toml` change needs a restart
  to take effect.
- After any change to pricing or sizing (spreads, liquidity, feed), run a
  `--dry-run` before going live again.
- Don't start live operation or install a service without the operator's
  explicit go-ahead.

## Start

Foreground — load env, dry-run, then live:

```bash
set -a; . ~/Stitch/stitch.env; set +a
stitch --config ~/Stitch/stitch.toml --dry-run
stitch --config ~/Stitch/stitch.toml
```

Service:

- Linux systemd: `sudo systemctl start stitch` (`enable --now` to also start on boot).
- macOS launchd / Windows Task Scheduler: start the agent or task you installed.

## Status and logs

- systemd: `systemctl status stitch`, and `journalctl -u stitch -f` to tail.
- Foreground: it logs to the terminal it runs in. `RUST_LOG` sets verbosity
  (default `info`).

## Stop

- Foreground: `Ctrl-C` (or `SIGTERM`). Stitch finishes the current tick first, so
  it never leaves a half-sent fill or a dangling order.
- systemd: `sudo systemctl stop stitch` (add `disable` to stop it restarting on boot).
- launchd: `launchctl bootout gui/$(id -u)/<label>`.
- Task Scheduler: `schtasks /End /TN "<name>"`.

## Change parameters

1. Edit the active `stitch.toml`: spreads (`buy_offset_bps` / `sell_offset_bps`),
   liquidity (`buy_total_liquidity_debt` / `sell_total_liquidity_collateral`),
   ladder, or TTL. Amounts are atomic token units. Full field reference is in
   `docs/ADVANCED.md`.
2. If you *raised* liquidity and approved an **exact** Permit2 allowance, re-run
   `stitch approve` (below) — otherwise the added depth posts but silently fails
   to fill.
3. Dry-run, then restart:
   - Foreground: `Ctrl-C`, re-run with `--dry-run`, then live.
   - systemd: dry-run by hand first for any pricing/sizing change, then
     `sudo systemctl restart stitch`.

## Permit2 approvals

The operator wallet must approve Permit2 for each token Stitch spends (debt on the
buy side, collateral on the sell side), or orders post but never fill and a live
start refuses to run. Preview, then approve:

```bash
stitch approve --config <path> --dry-run
stitch approve --config <path>          # maximum (recommended — approve once)
stitch approve --config <path> --exact  # cap to configured liquidity
```

Idempotent (skips already-approved tokens), one gas-paying tx per token. Under
systemd, pass the key file explicitly:

```bash
sudo STITCH_PRIVATE_KEY_FILE=/etc/stitch-bot/stitch.key \
  stitch approve --config /etc/stitch-bot/stitch.toml
```

## Upgrade

```bash
stitch --update      # installer-based installs only
stitch --version
```

Then restart: `sudo systemctl restart stitch`, or restart your foreground run. If
`--update` reports "no install receipt found", it was an archive install — grab
the latest binary from the GitHub Release instead.

## Docker fleet

Several bots on one host, one container each. Two ways it's driven, and it matters
which:

- **Stitch is running** (`docker ps` shows a `stitch-panel` container). The
  operator has a web UI at `https://<host>.<tailnet>.ts.net` that does start/stop,
  settings, approvals, dry runs and logs. **Point them at it instead of doing it
  from the CLI.** Racing Stitch isn't dangerous, but it's confusing: Stitch
  shows container state live, so an operator watching it will see you fight them.
  Everything Stitch does is also reachable over its API if they'd rather you
  drive: `GET /api/bots`, `POST /api/bots/<name>/{start,stop,restart}`,
  `PATCH /api/bots/<name>/settings`. The full guide is `docs/install-panel.md`.
  If you're restoring a host from Stitch's compose export
  (`GET /api/compose-export`), note that bots which were stopped when it was
  exported carry `profiles: [stopped]` and won't come up on a plain
  `docker compose up -d` — that's deliberate, so don't "fix" it. Start one with
  `docker compose --profile stopped up -d <bot-name>` if the operator asks.
- **Compose only.** Operate the compose file directly, per bot:

  ```bash
  docker compose ps                       # which bots exist and their state
  docker compose logs -f <service>        # tail one bot
  docker compose stop <service>           # SIGTERM, finishes the tick
  docker compose restart <service>        # after any stitch.toml edit
  docker compose run --rm <service> stitch approve --config /home/stitch/run/stitch.toml
  docker compose run --rm <service> stitch --config /home/stitch/run/stitch.toml --dry-run
  ```

Each bot's config is the `stitch.toml` bind-mounted into it. Find it with
`docker inspect -f '{{range .Mounts}}{{.Source}} -> {{.Destination}}{{"\n"}}{{end}}' <container>`
rather than guessing which `stitch.<name>.toml` belongs to which service. Edit
that file on the host, then restart just that container — the usual dry-run rule
applies for pricing or sizing changes.

**One trap worth knowing.** If a bot's compose service mounts `stitch.toml` and
the key as two individual files rather than mounting a whole directory, the Permit2
slot-nonce ledger lives on the container filesystem and is destroyed by
`docker compose up -d --force-recreate` or an image bump. The bot then restarts its
nonce sequence and the orders it signs collide with ones still resting on the book.
Tell the operator before you recreate anything in that layout. The panel detects it
and offers a one-click migration; by hand it means moving the two files into a
per-bot directory and mounting that directory read-write, with the config and key
remounted read-only on top.

## Not installed yet

If nothing above is present, install the **Stitch panel** (web UI). Don't
reconstruct the steps from memory — read `docs/AI_INSTALL_PROMPT.md` and follow
it in full. That prompt asks whether this is a local computer (password) or a
server (Tailscale), runs `install-panel.sh` from disk, opens the web app, and
stops. Bot config stays in the browser.

Prefer a local copy — `docs/AI_INSTALL_PROMPT.md` in the repo, or
`packages/stitch-bot/docs/AI_INSTALL_PROMPT.md` in the Textile monorepo —
otherwise fetch the canonical one:

```bash
curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/docs/AI_INSTALL_PROMPT.md
```

Other paths only if the operator explicitly prefers them (do not offer these as
the default install):

- **Desktop app** (no terminal): download the release for their OS and open Stitch
  — macOS `Stitch.dmg`, Windows `stitch-desktop.exe`, Linux `stitch-desktop`.
- **Manual / CLI**: the per-OS guides under `docs/install-*.md`, or `stitch init`
  for a single foreground bot.

Once the panel is up, tell the operator to finish in the web UI (Add a bot →
corridor → wallet → approve → dry run → start). Run `/stitch` again later only if
they want terminal help against an existing layout.
