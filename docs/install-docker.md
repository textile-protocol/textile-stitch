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
- `STITCH_CONFIG_FILE`, `STITCH_PRIVATE_KEY_FILE`, `STITCH_RUNTIME_DIR` — override
  the default paths if you mount files instead.

### Option A — mount files

```bash
docker run --rm \
  -v "$PWD/stitch.toml:/home/stitch/run/stitch.toml:ro" \
  -v "$PWD/stitch.key:/home/stitch/run/stitch.key:ro" \
  -e STITCH_PRIVATE_KEY_FILE=/home/stitch/run/stitch.key \
  ghcr.io/textile-protocol/textile-stitch:latest
```

### Option B — inject via environment (e.g. from a secrets manager)

```bash
docker run --rm \
  -e STITCH_CONFIG_TOML="$(cat stitch.toml)" \
  -e STITCH_PRIVATE_KEY="$(cat stitch.key)" \
  ghcr.io/textile-protocol/textile-stitch:latest
```

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

For a managed cloud deployment on AWS ECS Fargate, see
[install-cloud.md](install-cloud.md). For configuration reference and tuning, see
[ADVANCED.md](../ADVANCED.md).
