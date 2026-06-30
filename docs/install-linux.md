# Manual install — Linux

Run Stitch from a release binary and keep it alive under systemd. For a no-terminal
setup, the [desktop app](../README.md#option-2--desktop-app) also runs on Linux.

To get a menu entry and icon for the desktop app, install the bundled
`stitch.desktop` and `stitch.png` into your XDG dirs (the launcher's `Icon=stitch`
resolves from the icon theme):

```bash
install -Dm644 stitch.png ~/.local/share/icons/hicolor/256x256/apps/stitch.png
desktop-file-install --dir="$HOME/.local/share/applications" stitch.desktop \
  || install -Dm644 stitch.desktop ~/.local/share/applications/stitch.desktop
update-desktop-database ~/.local/share/applications 2>/dev/null || true
gtk-update-icon-cache ~/.local/share/icons/hicolor 2>/dev/null || true
```

Put `stitch-setup` on your `PATH` (or edit `Exec=` to an absolute path) so the
launcher can find it.

## 1. Install the binary

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/textile-protocol/textile-stitch/releases/latest/download/stitch-installer.sh | sh
stitch --version
```

Make sure the install directory is on your `PATH`.

## 2. Write the config

`stitch init` does this for you: it asks which corridor to run, takes the wallet
key without echoing it, and writes `stitch.toml`, `stitch.env`, and an owner-only
`stitch.key` into the current folder (or `--dir <path>`).

```bash
mkdir -p ~/Stitch && cd ~/Stitch
stitch init
```

Prefer to do it by hand? Start from
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
pull against orders you actually signed. To cap the allowance instead (only with
fixed numeric liquidity), use `--exact` — but then you must re-approve every time
the allowance is spent or you raise configured liquidity.

## 4. Run

```bash
stitch --config ~/Stitch/stitch.toml --dry-run   # signs/plans, posts nothing
stitch --config ~/Stitch/stitch.toml             # live
```

Stop a foreground run with `Ctrl-C`; Stitch finishes the current tick first, so
it never leaves a half-sent fill or dangling order.

## 5. Run as a service (systemd)

So it restarts after crashes and reboots. System-wide files live in
`/etc/stitch-bot`, separate from the foreground files in `~/Stitch`.

```bash
curl -L -o stitch.service \
  https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/deploy/stitch.service

printf 'RUST_LOG=info\n' > stitch.env

sudo install -m 0755 "$(command -v stitch)" /usr/local/bin/stitch
sudo mkdir -p /etc/stitch-bot
sudo install -m 0644 ~/Stitch/stitch.toml /etc/stitch-bot/stitch.toml
sudo install -m 0600 ~/Stitch/stitch.key /etc/stitch-bot/stitch.key
sudo install -m 0644 stitch.env /etc/stitch-bot/stitch.env
sudo install -m 0644 stitch.service /etc/systemd/system/stitch.service
sudo systemctl daemon-reload
sudo systemctl enable --now stitch
```

The service template uses `LoadCredential` so the key is injected as a systemd
credential rather than the process environment. Swap in `LoadCredentialEncrypted`
if you manage encrypted credentials.

Approve before the first live start (the service won't run until tokens are
approved), then view logs and restart after config changes:

```bash
sudo STITCH_PRIVATE_KEY_FILE=/etc/stitch-bot/stitch.key \
  stitch approve --config /etc/stitch-bot/stitch.toml
journalctl -u stitch -f
sudo systemctl restart stitch
```

## 6. Update

```bash
stitch --update            # in-place, for installer-based installs
sudo systemctl restart stitch
```

You can also download a new binary from the latest GitHub Release.

## 7. Stop and uninstall

```bash
# stop
sudo systemctl stop stitch
sudo systemctl disable --now stitch   # also stop it restarting on boot

# uninstall
sudo rm -f /etc/systemd/system/stitch.service
sudo systemctl daemon-reload
sudo rm -f "$(command -v stitch)"     # the installed binary
sudo rm -rf /etc/stitch-bot           # config + env
```

Removing the binary does **not** revoke on-chain Permit2 approvals. To fully wind
down, revoke each token's Permit2 approval (set its allowance to 0) or retire the
dedicated operator wallet.

For configuration reference and tuning, see [ADVANCED.md](../ADVANCED.md).
