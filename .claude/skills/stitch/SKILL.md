---
name: stitch
description: Operate Stitch, the Textile operator bot — start, stop, restart, check status, tail logs, upgrade, change pool parameters (spreads, liquidity), and manage Permit2 approvals. Falls back to install if Stitch isn't set up yet. Use when asked to "start/stop/restart stitch", "run the bot", "check the bot", "stitch logs", "upgrade stitch", "change the spread/liquidity", or "install stitch".
---

# Operate Stitch

Stitch is the Textile operator bot (binary `stitch`): per-pool market making plus
settlement closing. This skill both runs an existing Stitch and installs one that
isn't set up yet.

## Always start here

1. **Find the run layout and whether it's installed** — the next section.
2. **If it's not installed**, don't show the menu. Tell the operator, then with a
   single `AskUserQuestion` confirm they want to install now. On yes, install by
   reading `AI_INSTALL_PROMPT.md` and following it in full — see
   [Not installed yet](#not-installed-yet).
3. **If it's installed**, ask the operator what they want to do with
   `AskUserQuestion`, **one question at a time**. Never guess the action from a
   vague request — ask. Wait for each answer before the next question or any
   command.

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

- **Start / resume live** → [Start](#start) (cloud: [AWS cloud](#aws-cloud-ecs-fargate))
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

Figure out which layout is in use *before* doing anything. A missing local
binary only means "not installed" for the local layouts — the cloud layout has no
local binary by design, so don't treat a missing `stitch` as not-installed until
you've ruled out cloud.

Three standard layouts:

- **Foreground / manual** (macOS, foreground Linux, Windows): config lives in
  `~/Stitch/` (`%USERPROFILE%\Stitch\` on Windows) — `stitch.toml`, `stitch.key`,
  `stitch.env`. Started by hand, logs to its terminal.
- **Local service**: Linux systemd uses `/etc/stitch-bot/` with the same
  filenames and service name `stitch`. macOS launchd / Windows Task Scheduler run
  the agent or task you installed.
- **AWS cloud** (ECS Fargate): the operator-owned `deploy/aws` stack. No local
  binary — Stitch runs as a one-task ECS service, key + config live in Secrets
  Manager. See [AWS cloud](#aws-cloud-ecs-fargate) below.

Detect it: for the local layouts, `stitch --version` plus `systemctl status
stitch` (Linux), `launchctl list | grep -i stitch` (macOS), or Task Scheduler
(Windows); for cloud, an ECS service named `<bot>-stitch` in the operator's AWS
account (the request itself usually tells you — `aws`/ECS talk means cloud).
Operate against whichever is real. Only if it's a local layout and `stitch` isn't
on PATH is it genuinely not installed — then see [Not installed yet](#not-installed-yet).

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
   ladder, TTL, or the closer fields. Amounts are atomic token units. Full field
   reference is in `ADVANCED.md`.
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

## AWS cloud (ECS Fargate)

For the operator-owned AWS stack (`deploy/aws`), there's no local binary —
everything is `aws` CLI against the operator's account. Substitute the bot name
you deployed under (default `stitch-operator`; the README example uses
`stitch-operator-a`). The full runbook is `deploy/aws/README.md`.

- **Start / resume live**: set the service to one task.

  ```bash
  aws ecs update-service --cluster <bot>-cluster --service <bot>-stitch --desired-count 1
  ```

- **Pause** (no infra teardown): desired count `0`. The stack also ships at `0`,
  so a fresh deploy is paused until you do this.

  ```bash
  aws ecs update-service --cluster <bot>-cluster --service <bot>-stitch --desired-count 0
  ```

- **Logs**: `aws logs tail /ecs/<bot>/stitch --follow`.

- **Change parameters**: the config is the `STITCH_CONFIG_TOML` field of a JSON
  secret that *also* holds `STITCH_PRIVATE_KEY`, and `put-secret-value` replaces
  the **whole** value — so read the current secret and swap only that one field,
  or you'll wipe the key and the next task start fails. Pause to `0`, then merge:

  ```bash
  cur="$(aws secretsmanager get-secret-value --secret-id "$secret_arn" --query SecretString --output text)"
  aws secretsmanager put-secret-value --secret-id "$secret_arn" \
    --secret-string "$(jq -n --argjson cur "$cur" --rawfile cfg stitch.toml '$cur + {STITCH_CONFIG_TOML: $cfg}')"
  ```

  Then resume to `1`. Keep `DesiredCount=0` while changing config or rotating keys.
  `$secret_arn` is the stack's `SecretArn` output (see `deploy/aws/README.md`).

- **Approvals**: a one-off ECS task with an `approve` command override (not
  `systemctl`/local). See the `aws ecs run-task` block in `deploy/aws/README.md`;
  add `--exact` to the override for capped approvals.

- **Upgrade**: bump the pinned `ContainerImage` (use an immutable `sha-*` tag for
  production) and redeploy the stack; the service pulls the new image on next task
  start.

Same golden rules apply: never put the key in a file or on a command line (it's a
Secrets Manager value), and dry-run/pause around any pricing or sizing change.

## Not installed yet

If `stitch` isn't on PATH, install it first. Don't reconstruct the steps from
memory — read `AI_INSTALL_PROMPT.md` and follow it in full (OS/arch detection,
the operator interview, release install, safe key handling, dry run, and the
confirmation gate). Prefer a local copy — `AI_INSTALL_PROMPT.md` at the repo root,
or `packages/stitch-bot/AI_INSTALL_PROMPT.md` in the Textile monorepo — otherwise
fetch the canonical one:

```bash
curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/AI_INSTALL_PROMPT.md
```

Once it's installed and dry-run-clean, come back here to operate it.
