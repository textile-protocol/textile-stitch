# Install Stitch on AWS or any server

Use this when you want Stitch running 24/7 on a Linux host — Amazon EC2,
Lightsail, DigitalOcean, Hetzner, a bare-metal box, whatever you can SSH into.
Same product as the desktop app; the panel just lives on the server instead of
your laptop.

You install once with Docker. After that you add bots, wallets, and approvals in
the browser. You do not hand-edit compose files for day-to-day work.

For the Mac / Windows app on the machine in front of you, see
[install-desktop.md](install-desktop.md). For every advanced deploy option
(reverse proxy, build from source, pin digests), see
[install-panel.md](install-panel.md).

## What you need

- A Linux host with Docker Engine and Compose v2 (`docker compose version`).
- Outbound HTTPS (RPC, indexer, price feed, container registry). Do **not**
  open port 8420 to the public internet.
- A [Tailscale](https://tailscale.com) account (free tier is enough). Server
  mode puts Stitch behind your tailnet so you can open it from your other
  devices without publishing a port.
- A small instance is plenty to start — roughly `t3.small` / 2 vCPU / 2 GB RAM.
  Bump later if you run many bots on one box.

Stitch drives the Docker socket. Anyone who can reach the panel owns the host
and the operator wallets. That is why the install binds through Tailscale and
never exposes the UI on a public address.

## 1. Prepare the host

SSH in as a user that can run Docker (or use `sudo`).

Install Docker if it is not already there. On Amazon Linux 2023, install the
engine and the Compose v2 plugin:

```bash
sudo dnf install -y docker
sudo systemctl enable --now docker
sudo usermod -aG docker "$USER"
# Compose plugin (Amazon Linux's docker package does not ship it)
COMPOSE_VERSION=v2.32.4
sudo mkdir -p /usr/local/lib/docker/cli-plugins
sudo curl -fsSL \
  "https://github.com/docker/compose/releases/download/${COMPOSE_VERSION}/docker-compose-linux-$(uname -m)" \
  -o /usr/local/lib/docker/cli-plugins/docker-compose
sudo chmod 0755 /usr/local/lib/docker/cli-plugins/docker-compose
# log out and back in so the docker group applies
docker compose version
```

On Ubuntu / Debian, follow Docker’s
[Engine install guide](https://docs.docker.com/engine/install/) (it includes the
Compose plugin), then confirm `docker compose version`.

Create the bots directory (the installer defaults here on servers):

```bash
sudo mkdir -p /srv/stitch/bots
sudo chown "$USER":"$USER" /srv/stitch/bots
```

## 2. Mint a Tailscale auth key

In the Tailscale admin console: **Settings → Keys → Generate auth key**.

- Make it **reusable**, not ephemeral (the panel should keep its identity across
  restarts).
- Copy the key (`tskey-auth-…`). You will paste it into the installer once.

Note the login you use on the tailnet (the email under **Users**). That becomes
`PANEL_USERS`.

## 3. Install Stitch

**Quick path** (pulls `:latest`):

```bash
PANEL_MODE=server \
  TS_AUTHKEY='tskey-auth-…' \
  PANEL_USERS='you@example.com' \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh)"
```

**Recommended for production** — pin a release tag and image digest (see the
[releases page](https://github.com/textile-protocol/textile-stitch/releases)):

```bash
TAG=vX.Y.Z
curl -fsSL "https://raw.githubusercontent.com/textile-protocol/textile-stitch/${TAG}/install-panel.sh" -o install-panel.sh
curl -fsSL "https://github.com/textile-protocol/textile-stitch/releases/download/${TAG}/install-panel.sh.sha256" -o install-panel.sh.sha256
sha256sum -c install-panel.sh.sha256

STITCH_REF="$TAG" \
  PANEL_IMAGE="ghcr.io/textile-protocol/textile-stitch-panel:sha-<commit>" \
  STITCH_REQUIRE_PINNED=1 \
  PANEL_MODE=server \
  TS_AUTHKEY='tskey-auth-…' \
  PANEL_USERS='you@example.com' \
  sh install-panel.sh
```

The installer writes an owner-only `.env`, starts the published panel image with
a Tailscale sidecar, and prints the URL. No checkout, no compile.

Re-running the installer is safe: an existing `.env` is left alone, so it also
works as “pull the current image and bring Stitch back up.”

## 4. Open the panel and add a bot

1. Join the same Tailscale network on your laptop or phone.
2. Open the URL the installer printed
   (`https://stitch-panel.<your-tailnet>.ts.net`).
3. Click **Add a bot**, pick a corridor, set the operator wallet, approve
   Permit2 allowances, **Connect** the bot to Textile on Settings, then
   **Start**. New bots quote Swap via RFQ and will not Start without that.

That is the whole operator path. Logs, settings, start/stop, and approvals all
live in the browser.

## Day-to-day

```bash
# on the server
cd ~/stitch-panel   # or whatever PANEL_DIR you used
docker compose -f docker-compose.panel.yml ps
docker compose -f docker-compose.panel.yml logs -f panel
docker compose -f docker-compose.panel.yml pull && docker compose -f docker-compose.panel.yml up -d
```

Bots you create in the UI keep their own config under `/srv/stitch/bots`. You
can still SSH in and inspect files; the panel will keep reflecting what is on
disk.

## Security checklist

- Dedicated operator wallet per bot. Fund only the inventory that bot may quote.
- Keep SSH locked down (your IP `/32`, key auth). Never open 8420 publicly.
- Prefer a pinned `sha-*` panel image in production.
- Secrets live in owner-only files and env — never commit keys or paste them
  into public tickets.
- Review spreads, sizes, and Permit2 allowances before the first live start.
  Use dry-run after every pricing change.

## If something goes wrong

| Symptom | Likely fix |
|---------|------------|
| Installer can't pull the image | Check outbound HTTPS / registry access; confirm `docker pull` works. |
| Can't open the panel URL | Confirm Tailscale is up on both the server and your client; check `PANEL_USERS` matches your tailnet login. |
| Bot won't start / ledger errors | See the layout notes in [install-panel.md](install-panel.md#the-layout-warning). |
| Need a password instead of Tailscale | Use `PANEL_MODE=local` only on a host you sit in front of, or put an authenticated reverse proxy in front — covered in [install-panel.md](install-panel.md). |

## Related guides

- [install-panel.md](install-panel.md) — full Docker panel reference (manual
  Tailscale, reverse proxy, build from source).
- [install-docker.md](install-docker.md) — run a single bot with Compose, no
  panel.
- [install-linux.md](install-linux.md) — release binary + systemd, no Docker.
- [install-desktop.md](install-desktop.md) — menu bar / tray app on your own
  machine.
