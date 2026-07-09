# Manual install — Windows

Run Stitch from a release binary in PowerShell. If you'd rather not touch the
terminal, use the [desktop app](../README.md#option-2--desktop-app)
(`stitch-setup.exe`) instead — it does setup and then runs the bot with
Start/Stop, logs, approvals, and updates.

All commands below are PowerShell.

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

## 3. Approve Permit2

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

## 4. Run

```powershell
stitch --config "$env:USERPROFILE\Stitch\stitch.toml" --dry-run   # signs/plans, posts nothing
stitch --config "$env:USERPROFILE\Stitch\stitch.toml"             # live
```

Stop a foreground run with `Ctrl-C`; Stitch finishes the current tick first, so
it never leaves a half-sent fill or dangling order.

## 5. Keep it running

For 24/7 operation, register Stitch with Task Scheduler (run at logon/startup,
restart on failure) or install it as a Windows service with a wrapper like
[NSSM](https://nssm.cc/). The simplest always-on option on a desktop is the
[desktop app](../README.md#option-2--desktop-app), which supervises the bot while
open.

## 6. Update

```powershell
stitch --update    # in-place, for installer-based installs
```

You can also download a new binary from the latest GitHub Release.

## 7. Stop and uninstall

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
