# Manual install — Windows

Three paths:

1. **Desktop app** — [download](../README.md#option-1--desktop-app) and run
   `stitch-desktop.exe`. Menu / tray controller with a control window; open the
   local panel in the browser when you're ready. No Docker. Recommended for a
   personal PC.
2. **Stitch web UI (Docker Desktop)** — recommended for always-on / server-style
   hosts; see [install-panel.md](install-panel.md) / `install-panel.ps1`.
3. **Standalone bot binary** — this page (`stitch` CLI only).

All commands below are PowerShell.

## Stitch web UI (Docker)

Needs [Docker Desktop for Windows](https://docs.docker.com/desktop/setup/install/windows-install/)
with the Linux engine running.

```powershell
$env:PANEL_MODE = 'local'
$env:PANEL_PASSWORD = 'choose-a-long-password'
irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex
```

Open `http://127.0.0.1:8420`, log in, add a bot in the UI. Tailscale server mode
is Linux-only — use `install-panel.sh` on a Linux host for that.

## Standalone bot binary

## 1. Install the binary

```powershell
irm https://github.com/textile-protocol/textile-stitch/releases/latest/download/stitch-installer.ps1 | iex
stitch --version
```

Make sure the install directory is on your `PATH` (the installer prints it).

## 2. Write the config

`stitch init` asks which corridor to run, takes the wallet key without echoing
it, and writes `stitch.toml`, `stitch.env`, and an owner-only `stitch.key`. Keep
operator files in `%USERPROFILE%\Stitch`.

```powershell
mkdir "$env:USERPROFILE\Stitch" -Force | Out-Null
cd "$env:USERPROFILE\Stitch"
stitch init
```

To do it by hand, download
[stitch.example.toml](https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml),
keep the key in a separate `stitch.key` (never in `stitch.toml`), and point
`STITCH_PRIVATE_KEY_FILE` at it:

```powershell
$dir = "$env:USERPROFILE\Stitch"
irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml `
  -OutFile "$dir\stitch.toml"
$key = Read-Host -AsSecureString "Enter STITCH_PRIVATE_KEY"
[Runtime.InteropServices.Marshal]::PtrToStringAuto(
  [Runtime.InteropServices.Marshal]::SecureStringToBSTR($key)) |
  Set-Content -NoNewline "$dir\stitch.key"
"STITCH_PRIVATE_KEY_FILE=$dir\stitch.key" | Set-Content "$dir\stitch.env"
# lock the key file to your user only
icacls "$dir\stitch.key" /inheritance:r /grant:r "$($env:USERNAME):F" | Out-Null
```

## 3. Connect to Textile

New bots quote Swap via RFQ and do not rest orders on the public book, so they
need a maker credential before they can quote anything. `stitch connect` signs a
registration message with the wallet in your env, registers with Textile, and
writes `rfq-api.key` next to the config.

```powershell
$env:STITCH_PRIVATE_KEY_FILE = "$env:USERPROFILE\Stitch\stitch.key"
stitch connect --config "$env:USERPROFILE\Stitch\stitch.toml"
```

Skipping this leaves a bot that starts, logs, and serves nothing. If Textile has
no corridor seated for your pair yet it says so and keeps the credential — re-run
once they seat you. Moving an existing ladder bot across? See the
[migration guide](migrate-book-to-rfq.md#standalone-cli).

## 4. Approve Permit2

The operator wallet needs a one-time Permit2 approval for each input token (the
`debt` token on the buy side, the `collateral` token on the sell side). Without
it, orders post but silently fail to fill, and a live start refuses to run.

```powershell
$env:STITCH_PRIVATE_KEY_FILE = "$env:USERPROFILE\Stitch\stitch.key"
stitch approve --config "$env:USERPROFILE\Stitch\stitch.toml" --dry-run   # preview
stitch approve --config "$env:USERPROFILE\Stitch\stitch.toml"             # approve (max allowance)
```

A maximum allowance is the standard market-maker choice: approve once, never
re-approve. Use `--exact` to cap the allowance instead (only with fixed numeric
liquidity), at the cost of re-approving when it's spent or you raise liquidity.

## 5. Run

```powershell
stitch --config "$env:USERPROFILE\Stitch\stitch.toml" --dry-run   # signs/plans, posts nothing
stitch --config "$env:USERPROFILE\Stitch\stitch.toml"             # live
```

Stop a foreground run with `Ctrl-C`; Stitch finishes the current tick first, so
it never leaves a half-sent fill or dangling order.

## 6. Keep it running

For 24/7 operation, register Stitch with Task Scheduler (run at logon/startup,
restart on failure) or install it as a Windows service with a wrapper like
[NSSM](https://nssm.cc/). The simplest always-on option on a desktop is the
[desktop app](../README.md#option-2--desktop-app), which supervises the bot while
open.

## 7. Update

```powershell
stitch --update    # in-place, for installer-based installs
```

You can also download a new binary from the latest GitHub Release.

## 8. Stop and uninstall

Use the task or service name you created:

```powershell
# stop — Task Scheduler:
schtasks /End /TN "Stitch"
# or, if installed as a service with NSSM:
nssm stop Stitch

# uninstall — remove the task or service first:
schtasks /Delete /TN "Stitch" /F                        # Task Scheduler
nssm remove Stitch confirm                              # or the NSSM service
Remove-Item -Force (Get-Command stitch).Source          # the installed binary
Remove-Item -Recurse -Force "$env:USERPROFILE\Stitch"   # config + env
```

Removing the binary does **not** revoke on-chain Permit2 approvals. To fully wind
down, revoke each token's Permit2 approval (set its allowance to 0) or retire the
dedicated operator wallet.

For configuration reference and tuning, see [ADVANCED.md](ADVANCED.md).
