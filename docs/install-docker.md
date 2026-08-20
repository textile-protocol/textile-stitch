# Install — Docker

Run Stitch as a container. A prebuilt image is published to GitHub Container
Registry, or you can build it from source.

## Image

```bash
docker pull ghcr.io/textile-protocol/textile-stitch:latest
```

Or build locally from the repo root:

```bash
docker build -t stitch .
```

## Provide config and key

The container entrypoint can take the config and key as either mounted files or
environment variables. The runtime directory defaults to `/home/stitch/run`, and
the entrypoint writes injected secrets as `0600` before starting.

Environment variables the entrypoint understands:

- `STITCH_CONFIG_TOML` — the full `stitch.toml` contents (written to the runtime dir).
- `STITCH_PRIVATE_KEY` — the operator key (written to `stitch.key`, then
  `STITCH_PRIVATE_KEY_FILE` is exported automatically).
- `STITCH_RFQ_API_KEY` — the maker credential from `stitch connect` (written to
  `rfq-api.key`, then `STITCH_RFQ_API_KEY_FILE` is exported automatically).
- `STITCH_CONFIG_FILE`, `STITCH_PRIVATE_KEY_FILE`, `STITCH_RUNTIME_DIR` — override
  the default paths if you mount files instead.

### Option A — mount files

```bash
docker run --rm \
  -v "$PWD/stitch.toml:/home/stitch/run/stitch.toml:ro" \
  -v "$PWD/stitch.key:/home/stitch/run/stitch.key:ro" \
  -v "$PWD/rfq-api.key:/home/stitch/run/rfq-api.key:ro" \
  -e STITCH_PRIVATE_KEY_FILE=/home/stitch/run/stitch.key \
  ghcr.io/textile-protocol/textile-stitch:latest
```

### Option B — inject via environment (e.g. from a secrets manager)

```bash
docker run --rm \
  -e STITCH_CONFIG_TOML="$(cat stitch.toml)" \
  -e STITCH_PRIVATE_KEY="$(cat stitch.key)" \
  -e STITCH_RFQ_API_KEY="$(cat rfq-api.key)" \
  ghcr.io/textile-protocol/textile-stitch:latest
```

## Connect to Textile

New bots quote Swap via RFQ and do not rest orders on the public book, so they
need a maker credential before they can quote anything. `stitch connect` signs a
registration message with the wallet, registers with Textile, and writes
`rfq-api.key` beside the config.

This is the one step that needs the config **writable** — Connect rewrites
`stitch.toml` and drops the key next to it. Mount the directory rather than the
two files, and without `:ro`, or the write lands inside the container and
disappears with it.

The container runs as uid 1000, and a bind mount keeps its host ownership, so
that directory has to be owned by 1000 first. The entrypoint locks the run dir
down (`chmod 700`) before it runs anything, and that fails — taking the whole
container with it — on a directory owned by someone else:

```bash
sudo chown -R 1000 "$PWD"   # skip on macOS/Windows Docker Desktop, which maps ownership for you

docker run --rm \
  -v "$PWD:/home/stitch/run" \
  -e STITCH_PRIVATE_KEY_FILE=/home/stitch/run/stitch.key \
  ghcr.io/textile-protocol/textile-stitch:latest \
  stitch connect --config /home/stitch/run/stitch.toml
```

You now have `rfq-api.key` next to `stitch.toml` on the host. Every run below
mounts it read-only (Option A) or injects it as `STITCH_RFQ_API_KEY` (Option B).
Skipping this leaves a container that starts, logs, and serves nothing. If
Textile has no corridor seated for your pair yet it says so and keeps the
credential — re-run once they seat you. Moving an existing ladder bot across?
See the [migration guide](migrate-book-to-rfq.md#standalone-cli).

## Approve Permit2 first

Approvals are a one-time on-chain step and must be done before a live start (the
bot refuses to run live without them). Run the `approve` command in a one-off
container against the same config and key:

```bash
docker run --rm \
  -v "$PWD/stitch.toml:/home/stitch/run/stitch.toml:ro" \
  -v "$PWD/stitch.key:/home/stitch/run/stitch.key:ro" \
  -e STITCH_PRIVATE_KEY_FILE=/home/stitch/run/stitch.key \
  ghcr.io/textile-protocol/textile-stitch:latest \
  stitch approve --config /home/stitch/run/stitch.toml
```

Add `--dry-run` to preview without sending.

## Dry run, then live

Override the command with `--dry-run` to validate before going live:

```bash
docker run --rm \
  -v "$PWD/stitch.toml:/home/stitch/run/stitch.toml:ro" \
  -v "$PWD/stitch.key:/home/stitch/run/stitch.key:ro" \
  -e STITCH_PRIVATE_KEY_FILE=/home/stitch/run/stitch.key \
  ghcr.io/textile-protocol/textile-stitch:latest \
  stitch --config /home/stitch/run/stitch.toml --dry-run
```

The default command runs live against `/home/stitch/run/stitch.toml`. Stitch
shuts down cleanly on `SIGTERM` (what `docker stop` sends), finishing the current
tick first.

## Run several bots with Docker Compose

Each bot is one container with its own directory: config, wallet key, and the
maker credential Connect writes. `docker-compose.example.yml` in the repo root
does this for two bots:

```bash
# From the repo root. Copy the example, or use it directly with -f.
sudo chown -R 1000 ./bots                                  # Linux only; see below

docker compose -f docker-compose.example.yml run --rm bot1 \
  stitch connect --config /home/stitch/run/stitch.toml    # once per bot, before anything else
docker compose -f docker-compose.example.yml run --rm bot1 \
  stitch approve --config /home/stitch/run/stitch.toml    # once per bot
docker compose -f docker-compose.example.yml run --rm bot1 \
  stitch --config /home/stitch/run/stitch.toml --dry-run   # validate
docker compose -f docker-compose.example.yml up -d          # go live
```

Connect comes first and is not optional: a new bot quotes Swap over RFQ and
rests no public ladder, so a container that never enrolled starts and serves
nothing. It writes `rfq-api.key` into the bot's directory, and the bot picks it
up from beside its config on every run after — nothing to mount or inject.

It expects `bots/bot1/stitch.toml` and `bots/bot2/stitch.toml` (copied from
`stitch.example.toml`) with `stitch.key` beside each. Copy a service block to
add a third bot.

One directory per bot rather than a shared folder of `stitch.<name>.toml` files,
because `stitch connect` writes `rfq-api.key` next to the config it was given —
two bots sharing a directory would overwrite each other's maker credential. The
container also runs as uid 1000 and a bind mount keeps host ownership, which is
what the `chown` above is for; Docker Desktop on macOS and Windows maps that for
you.

Two things about this layout to know before your fleet grows:

- Mounting the whole directory is what keeps the Permit2 slot-nonce ledger. It
  lives beside the config, so mounting the two files individually would leave it
  on the container's filesystem — recreating the container loses it, the bot
  restarts its nonce sequence, and it collides with orders still resting on the
  book. That is also why the mount is read-write rather than `:ro`: Connect
  rewrites `stitch.toml` and writes `rfq-api.key` into it, and the running bot
  keeps the ledger there.

  The container runs as uid 1000 and has to own that directory. The entrypoint
  locks the run directory down to `0700` before it writes anything, which a
  non-owner can't do, so a root-owned directory means the container exits at
  startup — before Connect or the bot ever runs.
- Past two or three bots, editing YAML over SSH stops being fun. The
  [Stitch web UI](install-panel.md) is for the same fleet: add, start, stop,
  configure, tail logs, run approvals. It adopts the containers this compose file
  already started, without restarting them.

For configuration reference and tuning, see [ADVANCED.md](ADVANCED.md).
