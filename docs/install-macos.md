# Manual install — macOS

Run Stitch from a release binary in a terminal. If you'd rather not touch the
terminal, use the [desktop app](../README.md#option-2--desktop-app) instead — it
does setup and then runs the bot with Start/Stop, logs, approvals, and updates.

## 1. Install the binary

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/textile-protocol/textile-stitch/releases/latest/download/stitch-installer.sh | sh
stitch --version
```

Make sure the install directory is on your `PATH`.

## 2. Write the config

`stitch init` asks which corridor to run, takes the wallet key without echoing
it, and writes `stitch.toml`, `stitch.env`, and an owner-only `stitch.key`. Keep
operator files in `~/Stitch` so they're easy to find.

```bash
mkdir -p ~/Stitch && cd ~/Stitch
stitch init
```

To do it by hand, start from
[stitch.example.toml](https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml),
keep the key in a separate `stitch.key` (never in `stitch.toml`), and point
`STITCH_PRIVATE_KEY_FILE` at it:

```bash
curl -L -o ~/Stitch/stitch.toml \
  https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml
chmod 600 ~/Stitch/stitch.toml

umask 077
printf 'Enter STITCH_PRIVATE_KEY: '; stty -echo; IFS= read -r key; stty echo; printf '\n'
printf '%s\n' "$key" > ~/Stitch/stitch.key; unset key
printf "STITCH_PRIVATE_KEY_FILE='%s'\n" "$HOME/Stitch/stitch.key" > ~/Stitch/stitch.env
chmod 600 ~/Stitch/stitch.key ~/Stitch/stitch.env
```

## 3. Approve Permit2

The operator wallet needs a one-time Permit2 approval for each input token (the
`debt` token on the buy side, the `collateral` token on the sell side). Without
it, orders post but silently fail to fill, and a live start refuses to run.

```bash
set -a; . ~/Stitch/stitch.env; set +a
stitch approve --config ~/Stitch/stitch.toml --dry-run   # preview
stitch approve --config ~/Stitch/stitch.toml             # approve (max allowance)
```

A maximum allowance is the standard market-maker choice: approve once, never
re-approve. You approve the canonical Permit2 contract, and the reactor can only
pull against orders you actually signed. Use `--exact` to cap the allowance
instead (only with fixed numeric liquidity), at the cost of re-approving when it's
spent or you raise liquidity.

## 4. Run

```bash
stitch --config ~/Stitch/stitch.toml --dry-run   # signs/plans, posts nothing
stitch --config ~/Stitch/stitch.toml             # live
```

Stop a foreground run with `Ctrl-C`; Stitch finishes the current tick first, so
it never leaves a half-sent fill or dangling order.

## 5. Keep it running

For 24/7 operation you can run Stitch under a launchd LaunchAgent (a plist in
`~/Library/LaunchAgents/` that runs `stitch --config ~/Stitch/stitch.toml` with
`STITCH_PRIVATE_KEY_FILE` set and `KeepAlive` true). The simplest always-on
option on a desktop Mac is the [desktop app](../README.md#option-2--desktop-app),
which supervises the bot while open.

## 6. Update

```bash
stitch --update    # in-place, for installer-based installs
```

You can also download a new binary from the latest GitHub Release.

## 7. Stop and uninstall

If you set up a LaunchAgent, use the label you installed it under:

```bash
# stop
launchctl bootout gui/$(id -u)/<label>   # older macOS: launchctl unload ~/Library/LaunchAgents/<label>.plist

# uninstall
rm -f ~/Library/LaunchAgents/<label>.plist
rm -f "$(command -v stitch)"             # the installed binary
rm -rf ~/Stitch                          # config + env
```

Removing the binary does **not** revoke on-chain Permit2 approvals. To fully wind
down, revoke each token's Permit2 approval (set its allowance to 0) or retire the
dedicated operator wallet.

For configuration reference and tuning, see [ADVANCED.md](ADVANCED.md).
