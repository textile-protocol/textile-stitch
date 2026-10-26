# Install — Stitch (Docker / server)

Stitch is the web UI for running several bots on one host. Add a corridor with a
wizard, Connect the bot to Textile, start and stop it, edit its settings, watch
its logs, run Permit2 approvals — instead of SSHing in and hand-editing
`docker-compose.yml`. New bots quote Swap via RFQ and will not Start until
you Connect.

**Both installs are recommended** — pick by need. On a personal Mac or Windows
machine, the [desktop app](../README.md#option-1--desktop-app) (`Stitch.dmg` /
`stitch-desktop.exe`) runs the same panel locally without Docker. This page is
the Docker / Compose path for servers and always-on hosts. New to server
installs? Start with the shorter
[Install on AWS or any server](install-server.md) walkthrough, then come back
here for pins, reverse proxies, and builds from source.

The product name in the browser is **Stitch**. The binary, container, and image
are still named `stitch-panel` — that's the process underneath.

One Stitch per host. It has no database: the container list plus the config
directories on disk are the whole state, so anything you do by hand still shows
up in the UI, and you can walk away from Stitch at any time and go back to
compose.

**It is root on the host.** Stitch drives the Docker socket, which is
root-equivalent, and it can read every bot's config. Anyone who reaches it owns
the machine and the market-maker wallets. There is no version of this that is
merely somewhat privileged, so the whole install is built around not exposing it:
loopback bind on a local computer, Tailscale in front on a server, and a refusal
to start on a routable address unless you override it explicitly.

## Requirements

- Docker with Compose v2 (`docker compose version`).
- A host that can pull `linux/amd64` or `linux/arm64`. Published panel and bot
  images are multi-arch, so the same `:latest` / `sha-*` tag works on Apple
  Silicon Macs (arm64), Windows Docker Desktop (usually amd64), and Linux
  servers. Docker picks the matching variant; do not pin `platform:` in compose.
- A Tailscale account, for **server** installs (Linux hosts). Free tier is
  enough. On a Mac or Windows PC, use **local** mode instead — password on
  loopback.
- A directory on the host for bot configs (`~/stitch-bots` / `%USERPROFILE%\stitch-bots`
  locally, or `/srv/stitch/bots` on a server).

## Install it

**Windows (PowerShell + Docker Desktop) — local mode:**

```powershell
# Quick path
$env:PANEL_MODE = 'local'
$env:PANEL_PASSWORD = 'choose-a-long-password'
irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex
```

**Windows — recommended pin + checksum:**

```powershell
$TAG = 'vX.Y.Z'   # from https://github.com/textile-protocol/textile-stitch/releases
Invoke-WebRequest "https://raw.githubusercontent.com/textile-protocol/textile-stitch/$TAG/install-panel.ps1" -OutFile install-panel.ps1
Invoke-WebRequest "https://github.com/textile-protocol/textile-stitch/releases/download/$TAG/install-panel.ps1.sha256" -OutFile install-panel.ps1.sha256
$expected = ((Get-Content install-panel.ps1.sha256).Split()[0]).ToLowerInvariant()
$actual = (Get-FileHash install-panel.ps1 -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch for install-panel.ps1 (got $actual, want $expected)" }
$env:STITCH_REF = $TAG
$env:PANEL_IMAGE = 'ghcr.io/textile-protocol/textile-stitch-panel:sha-<commit>'
$env:STITCH_REQUIRE_PINNED = '1'
$env:PANEL_MODE = 'local'
$env:PANEL_PASSWORD = 'choose-a-long-password'
.\install-panel.ps1
```

Tailscale **server** mode is not offered by `install-panel.ps1` — use a Linux
Docker host and `install-panel.sh` for that.

**macOS / Linux — recommended pin + checksum:**

```bash
TAG=vX.Y.Z   # from https://github.com/textile-protocol/textile-stitch/releases
curl -fsSL "https://raw.githubusercontent.com/textile-protocol/textile-stitch/${TAG}/install-panel.sh" -o install-panel.sh
curl -fsSL "https://github.com/textile-protocol/textile-stitch/releases/download/${TAG}/install-panel.sh.sha256" -o install-panel.sh.sha256
# Linux: sha256sum -c install-panel.sh.sha256
# macOS: shasum -a 256 -c install-panel.sh.sha256
sha256sum -c install-panel.sh.sha256 2>/dev/null || shasum -a 256 -c install-panel.sh.sha256
STITCH_REF="$TAG" \
  PANEL_IMAGE="ghcr.io/textile-protocol/textile-stitch-panel:sha-<commit>" \
  STITCH_REQUIRE_PINNED=1 \
  PANEL_MODE=server TS_AUTHKEY=tskey-auth-… PANEL_PASSWORD='choose-a-long-password' \
  sh install-panel.sh
```

On a Mac laptop, use `PANEL_MODE=local` (and `PANEL_PASSWORD=…`) instead of the
Tailscale server variables above.

`PANEL_IMAGE` should be a `sha-*` tag or `@sha256:…` digest from
[GHCR](https://github.com/textile-protocol/textile-stitch/pkgs/container/textile-stitch-panel).
On macOS/Linux server installs, set `STITCH_COMPOSE_SHA256` from the release's
`docker-compose.panel.yml.sha256` asset when you want the fetched server compose
file integrity-checked. Windows/`install-panel.ps1` always uses
`docker-compose.panel.local.yml`, so that env var is skipped there (same as
`install-panel.sh` in local mode); the release also publishes
`docker-compose.panel.local.yml.sha256` for manual checks.

**Quick path** (image defaults to `:latest`, so compose is fetched from `main`
to stay aligned; pin both `STITCH_REF` and `PANEL_IMAGE` for a release install):

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh | sh
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex
```

That is the whole install. It checks Docker, asks whether this is a **local
computer** (password on `http://127.0.0.1:8420`) or a **server** (Tailscale;
Linux/`install-panel.sh` only), writes an owner-only `.env`, and starts the
published image. No checkout, no build. You add bots in the web UI afterward.

To run it unattended, set the answers in the environment and it won't prompt.
Omit `STITCH_REF` here so compose stays on `main` with the default `:latest`
image; for a release install, pin both `STITCH_REF` and `PANEL_IMAGE` as in the
recommended block above.

```bash
# Local computer (Mac or Linux laptop) — password login on loopback
PANEL_MODE=local PANEL_PASSWORD='choose-a-long-password' \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh)"

# Linux server — Tailscale (not the usual path on a Mac or Windows PC)
PANEL_MODE=server TS_AUTHKEY=tskey-auth-… PANEL_PASSWORD='choose-a-long-password' \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh)"
```

```powershell
# Windows — local mode only
$env:PANEL_MODE = 'local'
$env:PANEL_PASSWORD = 'choose-a-long-password'
irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex
```

| Variable | Default | What |
|----------|---------|------|
| `PANEL_MODE` | asked | `local` (password) or `server` (Tailscale). |
| `PANEL_PASSWORD` | asked | Required for fresh installs. Existing identity-login servers can set it in the UI. |
| `TS_AUTHKEY` | asked on server | Non-ephemeral Tailscale enrollment key; use a short expiration. |
| `PANEL_BOTS_DIR` | `~/stitch-bots` / `/srv/stitch/bots` | Where bot configs live on the host. |
| `PANEL_DIR` | `~/stitch-panel` | Where the compose file and `.env` go. |
| `PANEL_IMAGE` | published `:latest` | Pin a `sha-*` tag or digest in production. |
| `STITCH_REF` | `main` when image is `:latest`; else latest release tag | Git ref for compose + serve config. |
| `STITCH_COMPOSE_SHA256` | unset | Expected SHA-256 of `docker-compose.panel.yml` (server/`install-panel.sh` only; skipped for local compose). |
| `STITCH_REQUIRE_PINNED` | off | Require explicit immutable `STITCH_REF` (`vX.Y.Z` / 40-char SHA) and `PANEL_IMAGE` (`sha-*` / `@sha256:…`). |

Re-running it keeps existing settings and reuses the password hash. An older
server install with a saved `PANEL_USERS` allowlist keeps its existing login and
shows **Set panel password** in the UI after the upgrade. No terminal password
step is required. See the [migration path](#upgrading-an-older-server-install).
`PANEL_PASSWORD` can replace an environment hash; a password saved in the UI
takes precedence. See recovery below if that password is lost.

**The rest of this page is the manual route.** You only need it to change
something the installer doesn't ask about — your own reverse proxy, building
from source, or wiring the compose file into an existing stack.

## Manual: Tailscale and password

Two containers: a Tailscale sidecar joins your tailnet and terminates TLS, and the
panel shares its network namespace. The panel listens on `127.0.0.1:8420` inside
that namespace. No host port is published. Tailscale SSH is explicitly disabled;
this sidecar only needs to serve HTTPS.

Everything below runs from a checkout on the Docker host.

1. **Restrict tailnet access.** Limit the panel node's TCP 443 to your operator
   group in the Tailscale policy. Remove any broader rule that also allows access:
   Tailscale grants are additive. Do not enable Funnel, subnet routing or SSH on
   this node. Assign a tag through the admin console or the enrollment key after
   defining its owners in your tailnet policy.

2. **Generate a password hash.** Build the panel image and run its hashing command
   before starting a fresh stack:

   ```bash
   docker build -f Dockerfile.panel -t stitch-panel .
   docker run --rm -it stitch-panel hash-password
   ```

   It prompts twice with no echo and prints an argon2id hash. The password must
   have at least 12 characters. Keep it in your password manager.

3. **Write `.env`** next to `docker-compose.panel.yml` and keep it owner-only:

   ```bash
   TS_AUTHKEY=tskey-auth-...
   PANEL_PASSWORD_HASH='$argon2id$v=19$m=19456,t=2,p=1$...'
   PANEL_BOTS_DIR=/srv/stitch/bots
   ```

   Mint the non-ephemeral auth key in the Tailscale admin console. Use a short
   expiration for enrollment credentials. Quote the hash: it contains `$`.
   Run `chmod 600 .env`.

4. **Start it and sign in.**

   ```bash
   sudo mkdir -p /srv/stitch/bots
   docker compose -f docker-compose.panel.yml up -d --build
   ```

   Open `https://stitch-panel.<your-tailnet>.ts.net` from an allowed device and
   log in with the panel password.

### How the login works

Tailscale policy controls who can connect. Fresh installs and migrated panels
separately require a password session and ignore `Tailscale-User-Login`, even
when Serve supplies it.
A forged proxy identity alone cannot grant control of Docker. Tagged client
nodes also use the password login, subject to the same network policy.

Sessions live in memory for 12 hours; restarting the panel signs everyone out.
State-changing requests and followed log streams from another site are refused.

Custom deployments can still opt into identity-header authentication with
`STITCH_PANEL_TAILNET_USERS` and both trust flags. That makes the proxy part of
the authentication boundary. Adding a password to that mode is an **alternative**
credential, not a second required check. Keep both trust flags off to require a
password independently of the proxy.

### Upgrading an older server install

Existing operators can migrate entirely in the browser:

1. Click **Update panel** in the existing panel. Image updates preserve the
   current login settings and mounted fleet directory. Existing operators retain
   access; bot containers keep running.
2. The updated panel shows a persistent **Secure your panel with a password**
   warning above the fleet and bot pages. Click **Set panel password** and enter
   a password twice (at least 12 characters). **Not now** closes the form but
   leaves the warning visible. There is no automatic migration deadline.
3. Click **Save password**. A success message confirms password sign-in is now
   required. The submitting operator stays signed in. Other sessions are revoked,
   and all operators need the shared password on their next sign-in. Save it in
   your team's password manager.

Only an already authenticated operator can set the password. There is no
anonymous setup page. Merely having a fallback password alongside identity login
still shows the warning, because the header remains sufficient until migration.

The panel stores only an Argon2id hash in
`PANEL_BOTS_DIR/.stitch-panel-password.json`, with owner-only permissions. This
file overrides old environment authentication immediately and on subsequent
starts, including image updates. Keep it when backing up or moving the fleet
root. Invalid or unreadable saved credentials stop startup; they do not restore
identity login. Bot wallets and running containers are unchanged.

Re-running the updated installer is another upgrade path. It recognizes an
existing server `.env` with `PANEL_USERS`, preserves that allowlist, and sets
`PANEL_LEGACY_IDENTITY_AUTH=1` so the UI can offer the same migration. This marker
has no effect once a UI password is saved. Fresh installs still require a
password. For a manual Compose upgrade before migration, retain `PANEL_USERS`
and explicitly set `PANEL_LEGACY_IDENTITY_AUTH=1` in `.env`; after migration it
can be set to `0`.

**Image updates and deployment settings are separate.** The UI's Update button
updates the panel image, not the sidecar or Compose file. Installing the updated
Compose file keeps `TS_AUTH_ONCE=true`, so an enrolled node reuses its persisted
identity even after the enrollment key expires. First enrollment uses
`TS_EXTRA_ARGS=--reset --ssh=false`. On every boot, the bundled
`deploy/panel-tailscale.sh` CLI shim adds SSH/routing restrictions to
containerboot's `tailscale set` call before Serve is configured. This disables
SSH, route acceptance/advertisement, exit-node use/advertisement and app-connector
advertisement without logging in again. If that command fails, sidecar startup
fails too. Manual upgrades must copy the shim alongside the Compose and Serve
files; the installer downloads it automatically. Arbitrary old `TS_EXTRA_ARGS`
values are ignored; assign tags through the admin console or enrollment key
instead. Restrict the panel's TCP 443 to operators in tailnet policy, removing
broader allow rules too. A revoked or expired node identity still needs a valid
enrollment key; this is separate from expiration of the original enrollment key.

**Recovery and rollback:** if the shared UI password is lost, a host administrator
must stop the panel, configure a new `PANEL_PASSWORD_HASH`, disable both identity
trust flags (or set `PANEL_LEGACY_IDENTITY_AUTH=0` for this Compose file), remove
only `.stitch-panel-password.json`, then restart. Never delete the saved file
while the panel runs. An image predating this migration ignores the saved file
and can restore identity login from its old environment. Disable both trust flags
and supply an environment password hash before downgrading.

Verify the running configuration without printing the environment or credentials:

```bash
docker compose -f docker-compose.panel.yml exec tailscale tailscale version
docker compose -f docker-compose.panel.yml exec tailscale tailscale debug prefs | python3 -c 'import json,sys; print("RunSSH:", json.load(sys.stdin).get("RunSSH"))'
docker compose -f docker-compose.panel.yml exec tailscale tailscale serve status
curl -sS 'https://stitch-panel.<your-tailnet>.ts.net/api/session'
```

Expect `RunSSH: False`, one HTTPS proxy to `127.0.0.1:8420`, and an unauthenticated
session response after migration with `passwordLogin: true`, `tailnetLogin: false`
and `passwordSetupRequired: false`. Before migration the last two are true. An
allowlisted `Tailscale-User-Login` sent without a session after migration must get a 401 on
`/api/bots`. Confirm an excluded device cannot connect at all using your tailnet
policy tests and a real excluded device. Repository defaults do not prove that
an existing deployment or its policy has changed.

## Without Tailscale

The installer already covers a local password install via
`docker-compose.panel.local.yml` (`PANEL_MODE=local`). Use the snippet below
only if you already have an authenticated reverse proxy, or you're wiring the
panel into an existing stack by hand — skip the sidecar and publish only to
loopback:

```yaml
services:
  panel:
    build:
      context: .
      dockerfile: Dockerfile.panel
    container_name: stitch-panel
    ports:
      - "127.0.0.1:8420:8420"
    environment:
      STITCH_PANEL_BIND: 0.0.0.0:8420
      STITCH_PANEL_ALLOW_INSECURE_BIND: "1"
      STITCH_PANEL_BOTS_DIR: /data/bots
      STITCH_PANEL_HOST_BOTS_DIR: /srv/stitch/bots
      STITCH_PANEL_PASSWORD_HASH: ${PANEL_PASSWORD_HASH:?}
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - /srv/stitch/bots:/data/bots
    restart: unless-stopped
```

`0.0.0.0` inside the container is fine here *because* the published port is
pinned to `127.0.0.1` on the host — that's what `ports:` is doing above, and it's
why the insecure-bind override is needed: the panel can't see your host's port
mapping, only its own listener.

If your proxy authenticates users and sets `Tailscale-User-Login` itself, add
`STITCH_PANEL_TRUST_IDENTITY_HEADER=1`, `STITCH_PANEL_IDENTITY_PROXY_ONLY=1`, and
the tailnet allowlist. Only do that if your proxy strips the inbound header first
*and* nothing can bypass the proxy to reach the panel's port directly.

## Adopting an existing compose fleet

Nothing to migrate and nothing to restart. Point the panel at the directory
holding your existing configs and it finds the bots.

Set `PANEL_BOTS_DIR` to wherever your `stitch.*.toml` files live — the directory
you run `docker compose` from, if you followed `docker-compose.example.yml`.
Start the panel and your bots are in the list, with their corridor, chain,
operator address, and container state.

Discovery works two ways:

- **Panel-created** bots carry a `com.textile.stitch.bot` label.
- **Adopted** bots are containers with a `com.docker.compose.service` label
  running a `textile-stitch` image. The panel reads the container's mount table
  to find where its `stitch.toml` lives on the host, then parses it.

Editing settings works on adopted bots too, because config is bind-mounted rather
than baked into the image: the panel writes the file on disk and restarts the
container, which is exactly the manual workflow.

Bots the panel creates carry no compose project label, so
`docker compose down --remove-orphans` won't touch them. The reverse isn't true —
see "two cooks" below.

### The layout warning

If your compose file mounts the two config files individually — the layout
`docker-compose.example.yml` used to ship, and which plenty of running fleets
still use:

```yaml
volumes:
  - ./stitch.bot1.toml:/home/stitch/run/stitch.toml:ro
  - ./stitch.bot1.key:/home/stitch/run/stitch.key:ro
```

then everything else the bot writes lives on the container's own filesystem —
including the Permit2 slot-nonce ledger. Recreate that container, whether to
upgrade the image or change a setting, and the ledger is gone. The bot restarts
its nonce sequence and the orders it signs next collide with ones still resting on
the book.

The panel flags those bots and offers a one-click fix on the bot's page. Migrating
stops the bot (or leaves it alone if it was already stopped), moves `stitch.toml`,
the signer secret and any sibling `stitch.env` into `/<bots dir>/<name>/`, copies
the existing ledger out of the container before it's destroyed — preferring that
over any stale copy already on the host — recreates the container with the whole
directory mounted read-write, and starts it again only if it was running. The env
file matters for Turnkey: the API public key lives there, not in the TOML, and a
migration that left it behind would start a container that exits immediately.
Expect a few seconds without quotes.

The bot comes back on the image it was running, not on
`STITCH_PANEL_BOT_IMAGE` — this changes the layout and nothing else. Use
**Recreate** when you want to pick up a new image.

If anything fails before the old container is removed — including the stop itself,
if Docker refuses it — the migration undoes itself and the bot is started again, so
you can fix the cause and click again. The cleanup removes only what that attempt
wrote, and only deletes the directory if it created it, so a file you'd already put
there by hand survives. Once the old container is gone there's no going back; a
failure past that point leaves the bot stopped with its files in the new layout,
and **Recreate** finishes the job.

**Failing to read the ledger counts as a failure.** That read is the last chance to
get it: the next step deletes the container holding it. So the migration rolls back
instead of pressing on, and a daemon hiccup costs you a retry rather than the
nonces for every order you have live. If it keeps failing — an adopted bot on a
custom image with no `/home/stitch/run`, or a run directory too large to pull
through the archive API — a second button appears to go ahead without it. That one
is honest about the price: orders live at that moment stay on the book until they
expire, because the replacement can't replace them. Only the panel's own
`acceptLedgerLoss=true` sets it; the default never does.

A bot must also be plainly running or plainly stopped to migrate. `paused` is
refused because a frozen process can't act on SIGTERM, so the graceful stop would
become a kill and the in-flight post's nonce might never reach the ledger —
`docker unpause` it first. `removing` and any state the panel doesn't recognise are
refused for the same reason: it won't guess whether something is still writing.

Do the bots one at a time and watch the log tail after each.

**Update your compose file afterwards.** The old file still describes the old
mounts, so the next `docker compose up -d` would recreate the pre-migration
container. Grab a fresh one from **Export compose** in the Stitch menu (or
`GET /api/compose-export`), which describes the fleet as it is now — both adopted
and panel-created bots.

That export is generated, never round-tripped: the panel doesn't read your
compose file, so your comments and unrelated services aren't preserved. Treat it
as a disaster-recovery artifact you diff against, not as a file the panel owns.

Two things about it worth knowing before you run it:

- **A bot that was stopped when you exported stays stopped.** Those services carry
  `profiles: [stopped]`, which compose skips on a plain `up -d`, so restoring the
  fleet doesn't put a bot you stopped deliberately back on the book. Start one with
  `docker compose --profile stopped up -d <bot-name>`. A bot Docker was *restarting*
  counts as up and comes back in the default set — it was meant to be running, and
  this file is often how you recover onto a host where it finally will be.
- **A bot still on the flat layout is exported as it actually runs** — its
  `stitch.bot1.toml` and key mounted individually, no writable directory. That's
  faithful to the running container, and it means the ledger stays inside it. The
  export says so inline. Migrate the bot and re-export to fix it.
- **Mounts name the directory each bot actually uses, not its name.** An adopted
  service called `foo` can mount `<bots root>/custom`, and the export says `./custom`.
  A config living outside the bots root is written as an absolute host path with a
  comment: correct on this host, but the one thing in the file that won't survive
  being copied to another one.
- **A bot whose signer the panel can't read is left out, with a comment saying so.**
  It won't guess. Exporting a Turnkey or MPCVault bot as a hot wallet would produce a
  service block that looks right, restores fine, and then can't start — and you'd
  find out on the host you were recovering onto. A `# <bot> was not exported:` line
  is a problem you can see before you need it.

### Two cooks

If someone runs `docker compose up -d` from the old file after the panel stopped a
bot, compose starts it again. Your hand-written file has no idea the bot was
paused. (A fresh export does — see the `stopped` profile above — but only as of
the moment you exported it.) The panel can see which bots are compose-managed and
says so, but it doesn't try to prevent it. Pick one tool per fleet and stick to
it; the compose export is there for when you want to go back.

## What changes about the manual workflow

| Task | Before | With Stitch |
|------|--------|----------------|
| Add corridor | copy a service block, write a toml, chmod the key | wizard: corridor, name, key |
| Change a spread | ssh, edit toml, `docker compose restart` | edit the field, Save (restarts for you) |
| Permit2 approve | `docker compose run --rm bot1 stitch approve …` | Approve allowances, output streamed |
| Dry run | same, with `--dry-run` | Dry run button |
| Logs | `docker compose logs -f bot1` | live tail with level colouring |
| Upgrade a bot | edit the image tag, `up -d` | **Update** when a newer digest is available (pulls `STITCH_PANEL_BOT_IMAGE` and recreates that bot). **Recreate** rebuilds on the configured image for recovery, keeping a pinned bot's own build. |
| Downgrade a bot | edit the image tag back, `up -d` | **Tools → Roll back** — pick one of the last 10 published builds |
| Upgrade the panel | `docker compose pull && up -d` | **Update panel** in the header when a newer `textile-stitch-panel` digest is published |

**Approve needs the operator wallet to itself.** It runs in a throwaway container
with its own copy of the key, and it broadcasts. So does a bot's taker or closer
leg — and both pick their nonce the same way, by asking the node for the pending
count. Two of them overlapping can sign the same nonce, and one of the two
transactions is then dropped: the approval, or a fill the bot had already committed
to. The panel refuses rather than racing, and the button tells you which bot to
stop before you click.

It compares wallets, not names, so a second bot running the same key blocks it too
— running one key in two config directories is an ordinary way to quote two
corridors on one chain. Two approvals on one wallet can't overlap either, and the
reservation lasts until the throwaway container is actually gone, not just until you
close the tab watching it. A maker-only bot is fine to approve alongside: it signs
Permit2 orders offchain and never touches an account nonce.

It works the other way round too. **Start, Restart, Recreate and Migrate are refused
while an approval holds that wallet** — every bot approves its allowances at live
start, so launching one mid-approval is the same collision from the other side. Wait
for the approval to finish. Saving settings counts as a launch when it bounces the
bot: the save goes through, but the restart is skipped and the response says why,
because a save can be the thing that turns a maker into a taker.

Both sides take the same reservation rather than checking each other, so two requests
racing on one wallet can't each pass its own check inside the other's window. A launch
also re-checks the fleet after reserving: a bot that is *already* running holds no
reservation, so starting a second bot on its wallet is refused on the strength of what
the fleet says, not what the reservation set does. Two bots that are already both live
on one wallet are a configuration to fix — Restart and Recreate stay available there,
because refusing them wouldn't remove the overlap.

Creating a bot with **start on** goes through the same protocol. If the wallet is busy
the bot is still created, just not started, and the response says so.

**Restart only works on a bot that's up.** `docker restart` on a stopped container
starts it, so an unguarded Restart would be a second Start button — including for a
bot fresh out of the wizard that's deliberately waiting on its allowance. Use Start
when you mean to put it on the book.

Anything that creates a container pulls the image first if the host doesn't have
it. Recreate always asks the registry even when the tag is already cached, so a
mutable `:latest` actually picks up a new release — migrate and one-shots leave
the cached binary alone. Recreate removes before it creates, so a
`STITCH_PANEL_BOT_IMAGE` you typo'd fails on the pull and leaves the running bot
alone, rather than deleting it and then discovering there's nothing to start.
Recreate is also how you recover a config-only bot — config on disk, no
container — which is what you get when the wizard writes the files and then
create fails, or when someone removes the container and keeps the directory. Add
Bot refuses that name; Recreate is the button. To drop it instead, use **Delete**
on the fleet row (or on the bot page): that wipes the config and private key and
takes the row off the fleet. **Remove** on a live bot deletes container + config
in one step; Cancel aborts entirely.

What doesn't change: **saving settings on a running bot restarts it.** Stitch reads
its config once at startup, so there's no way to apply a spread change in place.
The UI says so before you save. Orders already signed under the old settings stay
fillable until their TTL expires — stopping a bot does not cancel them. Saving on
a stopped bot writes the file and leaves it stopped; it picks the change up when
you start it.

The settings page is scoped to one pool. On a multi-pool config, editing the price
feed writes a `feed_url` override under that pool rather than the shared
`[feed].url`, because the pools price different corridors and repointing all of
them from one form would be wrong. Expect a new `feed_url` line in the toml. With
a single pool and no existing override it writes `[feed].url`, which is the same
thing.

**Approvals and dry runs** run in a throwaway container, on the bot's own image and
its own config and key — the same thing `docker compose run --rm` did, including
for a bot still on the flat layout, whose `stitch.bot1.toml` is mounted where the
binary expects it. The bot itself keeps running. The container is removed when the
command exits, and also if you close the tab or navigate away mid-run, which
matters because `stitch --dry-run` polls forever and would otherwise sit there
hitting your RPC. Those containers are labelled and never appear in the fleet list.

Secrets are write-only. No endpoint returns key material; the UI shows the derived
operator address and nothing more.

## Fills, PnL, holdings

Not here. That lives in the Textile dashboard, backed by the
`settlementMakerStats` query. The panel is container health, config, and logs.

## Environment reference

Everything is read from the environment at startup. Nothing is stored.

| Variable | Default | What |
|----------|---------|------|
| `STITCH_PANEL_BIND` | `127.0.0.1:8420` | Listen address. Loopback or a tailnet address, else it refuses. |
| `STITCH_PANEL_BOTS_DIR` | `/data/bots` | Config root as the panel sees it. |
| `STITCH_PANEL_HOST_BOTS_DIR` | same as above | The same directory on the host. Required for a containerised panel. |
| `STITCH_PANEL_DOCKER_SOCKET` | `/var/run/docker.sock` | Docker Engine API socket. |
| `STITCH_PANEL_BOT_IMAGE` | `ghcr.io/…/textile-stitch:latest` | Image new bots run. Pin a `sha-*` tag in production. |
| `STITCH_PANEL_BOT_UID` | `1000` | The uid the bot image runs as. The panel gives every bot directory it writes to this uid, because the bot's entrypoint can't lock down a directory it doesn't own and exits. Only change it for a custom image built with a different user. |
| `STITCH_PANEL_TAILNET_USERS` | — | Legacy or custom identity-header deployments; ignored after UI password setup. |
| `STITCH_PANEL_PASSWORD_HASH` | — | argon2 hash from `stitch-panel hash-password`; overridden by a saved UI password. |
| `STITCH_PANEL_ALLOW_INSECURE_BIND` | off | Permit a routable bind. You own the consequences. |
| `STITCH_PANEL_TRUST_IDENTITY_HEADER` | off | Believe `Tailscale-User-Login`. Required for tailnet-login auth, never inferred from the bind. Also requires `STITCH_PANEL_IDENTITY_PROXY_ONLY=1`. |
| `STITCH_PANEL_IDENTITY_PROXY_ONLY` | off | Attest that an authenticated reverse proxy is the sole peer on the listener (sets and strips the identity header). Required with `TRUST_IDENTITY_HEADER`. Off by default in Compose; retained only for explicit legacy migration. |
| `STITCH_PANEL_CORRIDOR_API` | `https://api.textilecredit.com` | Where the Add-a-bot wizard reads the corridor list from. Set to `off` to pin the panel to the corridors compiled into the binary and never call out. |
| `RUST_LOG` | `info` | Log verbosity. |

Fresh Compose installs require `STITCH_PANEL_PASSWORD_HASH`. Existing panels can
save a password through the UI instead. Legacy identity auth requires
`STITCH_PANEL_TAILNET_USERS` and both trust flags. There is no anonymous mode.

The corridor list is Textile's, not the binary's: the wizard asks
`STITCH_PANEL_CORRIDOR_API` which corridors are listed and writes the config that
comes back with the one you pick, so a corridor listed on the site is selectable
on your next page load with no Stitch upgrade. The request is unauthenticated and
carries nothing about you. When it fails the panel falls back to the corridors
built into this release and says so above the picker — the wizard still works,
it's just older. Corridors Stitch ships a tuned preset for (cNGN/USDT and
friends) keep that preset's spreads rather than the generic ones the API renders.

`STITCH_PANEL_HOST_BOTS_DIR` matters more than it looks: bind mounts are resolved
by the Docker daemon on the host, so a path that's correct inside the panel
container is wrong in a mount spec. Get it wrong and new bots come up mounting an
empty directory.

## Troubleshooting

**"couldn't talk to the Docker daemon"** — the socket isn't mounted. Check the
`/var/run/docker.sock` volume, or point `STITCH_PANEL_DOCKER_SOCKET` at the right
path (Docker Desktop and rootless Docker put it elsewhere).

**"too many sign-in attempts are being checked right now"** — password verification
is deliberately memory-hard, so the panel caps how many run at once and sheds the rest
rather than letting a guessing attacker exhaust the host. Wait a moment and retry.

**Zero bots on a host that has some** — `STITCH_PANEL_BOTS_DIR` is pointing
somewhere else, or the bind mount doesn't include your configs. The startup log
prints the count; the fleet page prints the directory it read.

**"refusing to bind the panel to 0.0.0.0"** — working as intended. Bind
`127.0.0.1` and put `tailscale serve` or your own proxy in front. The override
exists but read the message first.

**"nothing has told it the `Tailscale-User-Login` header is trustworthy"** — you
set `STITCH_PANEL_TAILNET_USERS` without both trust flags, so every request would
be rejected and the panel says so at startup instead. Use a password hash and
keep both trust flags off, as the shipped Compose files do. Only custom proxy
deployments should enable identity-header authentication.

**"`STITCH_PANEL_IDENTITY_PROXY_ONLY` is not"** — you set
`STITCH_PANEL_TRUST_IDENTITY_HEADER=1` without the proxy-only attestation. TRUST
alone is refused on purpose: on the host's loopback that header is forgeable.
Either add `STITCH_PANEL_IDENTITY_PROXY_ONLY=1` for a real proxy-only deploy, or
drop TRUST and use a password.

**A bot's config "isn't readable" or "is outside the bots root"** — an adopted bot
whose `stitch.toml` lives somewhere the panel can't see. Either bind-mount that
path into the panel too, or move the config under the bots root and recreate the
container.

**"giving … to uid 1000" when adding a bot** — the panel writes the config as
whoever it runs as, then hands the directory to the bot's uid, because the bot's
entrypoint locks its run directory down to `0700` and a non-owner can't do that —
the container exits before it reads a line of config. That handover needs root,
which the shipped compose file gives it. If you run the panel as a normal
user instead, either run it as the same uid as the bot or do what the error says:
`chown -R 1000 <bots-dir>/<bot>`, then retry.

**"is running and would not stop … so the panel left it alone"** — Delete and
Recreate shut the bot down first and give up if that fails, instead of
force-killing a container that could be signing right then. Check the log tail,
stop it by hand with `docker stop <container>`, then retry.

**"More than one container claims this bot name"** — usually a panel-created bot
and a compose service that were never reconciled. Editing is blocked, and so are
start, stop, restart, delete and migrate: the fleet shows the two containers as one
entry, so an action would hit whichever one discovery picked, and a delete with the
config would pull the files out from under the other. Rename or remove one with
`docker` and reload.

**Tailscale sidecar won't start** — `/dev/net/tun` missing or `NET_ADMIN` denied.
Set `TS_USERSPACE: "true"` in the compose file to use userspace networking
instead; it's slower but needs no host device.

**No password login prompt** — on an upgraded legacy panel, use the persistent
**Set panel password** action. Saving it disables identity login. Afterwards,
`/api/session` reports `passwordLogin: true`, `tailnetLogin: false` and
`passwordSetupRequired: false`. On a fresh install, check the environment hash.

## Image updates

The panel checks the registry (GHCR) for newer digests of the configured bot
image and of its own panel image. Results are cached for about 15 minutes; pass
`?refresh=1` on `/api/updates` (or use the UI after an update) to force a recheck.

- **Per-bot Update** pulls `STITCH_PANEL_BOT_IMAGE` and recreates that bot only.
  Config and key stay on disk. Expect a brief gap in quoting. If the bot still
  uses the flat layout, migrate first so the nonce ledger isn't lost on recreate.
- **Recreate** is the same Docker action without the "you're behind" nudge — use
  it for recovery (missing container, stuck state). It rebuilds on
  `STITCH_PANEL_BOT_IMAGE`, except for a bot already pinned to one build of that
  repository (a `sha-*` / digest ref, or a rollback), which keeps its own image —
  recovering a stuck container must not quietly move it onto another release.
- **Update panel** pulls a newer `textile-stitch-panel` image (pinned `sha-*`
  tags resolve to `:latest` of the same repo) and schedules a self-recreate via a
  short-lived helper on the Docker socket. The UI disconnects briefly; bots keep
  running. Local-only images (`stitch-panel` with no registry path) can't
  self-update — rebuild or set `PANEL_IMAGE` to the published GHCR image.

Offline or private registries that reject anonymous pulls soft-fail: the UI
simply doesn't show an update, rather than erroring the fleet page.

### Rolling back a bot

**Tools → Roll back to an earlier version** lists the ten most recent published
builds of `STITCH_PANEL_BOT_IMAGE`'s repository with the commit date and subject
behind each. Picking one recreates that bot on it — the same Docker action as
Update, aimed backwards.

The order comes from the commits behind the tags, not from the registry's tag
list (the Distribution spec orders that lexically, which for `sha-<hex>` means
nothing). That needs the image to be published from a public GitHub repository
of the same name — true for `ghcr.io/textile-protocol/textile-stitch`.

The card only claims "newest first" when every build on it was placed that way.
If any row couldn't be — built off a branch that isn't the default, or older
than the last hundred commits — the "newest" marker disappears and the card says
so, because an unplaced build sorts last while possibly being the newest of the
lot. Same when nothing at all could be placed: the list still works for rolling
back, it just isn't a ranking.

Read this before using it:

- It runs older code. Every fix published since that build goes with it,
  security ones included. It's for getting out from under a bad release, not for
  staying put.
- **The config is not rolled back.** `stitch.toml` keeps whatever it says today,
  so a build that predates a setting in it will refuse to start. Watch the logs
  straight after.
- The bot pins to that exact tag and stops picking up releases until you press
  Update again. Recreate keeps the pin, so recovering a stuck container won't
  put the release you just left back on.
- Only per-commit `sha-…` tags are offered. Channel tags like `latest` move on
  the next release, so pinning to one wouldn't be a pin — the API refuses them.
- Flat-layout bots are refused: the recreate would drop the in-container nonce
  ledger and leave live orders on the book with nothing able to replace them.
  Migrate first.
- Desktop (process runtime) has no per-bot image, so there's nothing to roll
  back — install an earlier release from the menu bar instead.

## Hardening, once it works

- **Restrict the tailnet policy.** Assign a panel tag in the admin console or on
  the enrollment key and allow only operators to reach TCP 443. Broad allow rules
  must be removed too. Keep SSH, Funnel, app-capability mounts and routing off.
- **Complete password setup.** Existing panels retain identity login until an
  operator saves a password in the UI. The warning stays visible until then.
  Saved passwords disable both identity trust flags. Anyone authenticated to the
  panel can control Docker and operator wallets.
- **Review image updates.** Pin panel and bot images to reviewed `sha-*` tags or
  digests. The Tailscale sidecar currently tracks `stable`; record the running
  version and digest, review vendor bulletins and apply updates promptly. An
  update is not proof that an unconfirmed report has been fixed.
- **Limit credentials.** Use short-lived enrollment keys, restrict who can enroll
  nodes, and keep signer permissions and funded inventory limited to each bot's
  needs. If a node is compromised, revoke it and rotate exposed credentials.
