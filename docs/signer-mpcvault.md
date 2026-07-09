# MPCVault Signer Setup

MPCVault is one of Stitch's MPC signer backends. The operator key lives in an
MPCVault vault instead of a local `stitch.key`, and every signature the bot makes
(EIP-712 limit orders and EIP-1559 fill/approve transactions) is produced by the
vault.

Unlike Turnkey, MPCVault is a **two-process setup**. Automated signing needs a
co-located `client-signer` Docker sidecar (it holds an MPC key share) plus a
callback HTTP server the bot runs for the sidecar to approve each request. So on
one host you run the bot **and** the sidecar next to it.

MPCVault signs the raw 32-byte digest the bot computes, the same as Turnkey, so
signing is chain-agnostic and one vault wallet covers every EVM corridor (Celo,
Base, Arbitrum, Ethereum). The bot builds the per-chain digest and broadcasts the
transaction itself.

> **Paid plan required.** MPCVault gates all API access behind a paid plan. The
> free tier can't create an API user or use the API at all, so both "API
> transaction creation" and "API account provisioning and transaction signing"
> need at least MPCVault's first paid tier. Confirm your plan covers API access
> before starting.

## How MPCVault's pieces map to the config

MPCVault splits what the bot needs across three separate objects. Getting these
straight up front avoids the usual confusion (an "API user" is not a wallet):

| MPCVault object | What it is | Feeds |
|---|---|---|
| **Vault** | the MPC wallet; has a UUID and one EVM address | `vault_uuid`, `operator_address` |
| **API user** | a programmatic identity that holds the API token | `MPCVAULT_API_TOKEN` (env secret) |
| **Client Signer** | the headless co-signer (the Docker sidecar) | `client_signer_pubkey` |

The API token authorizes REST calls (creating and executing signing requests).
The Client Signer actually co-signs. You need all three: the token gets the
request in the door, the sidecar produces the signature.

There is **no separate "create API wallet" step**, and this trips people up. In
MPCVault an "API wallet" just means a vault whose signing is driven by the API
Client Signer, so attaching the Client Signer (step A.4) is what turns your
existing vault into one. The only things you actively create are the **API user**
(for the token) and the **Client Signer**; the wallet is the vault you already
have.

## Prerequisites

- An MPCVault account on a **paid plan** with API access.
- A vault holding the operator wallet, funded with a little native gas for
  Permit2 approvals (`stitch approve`).
- Docker on the same host as the bot.

## Part A: configure MPCVault (the product)

### 1. The vault

Use (or create) the vault that holds the operator wallet. Note two things:

- The **vault UUID**, that's `vault_uuid`.
- The vault's **EVM wallet address** (`0x…`), that's `operator_address`. It's a
  standard EVM address, valid across every EVM chain, so one vault wallet signs
  all corridors.

### 2. Create the API user / token

In the MPCVault console, go to **Settings → API** and generate an API token. This
is the `x-mtoken` the bot sends on every REST call; it becomes
`MPCVAULT_API_TOKEN`. Copy it now, MPCVault typically shows it once.

**Grant the API user access to your operator vault** (`vault_uuid`). MPCVault
scopes access per vault, and the API-user setup asks which vaults it can reach, so
include the operator vault. If the token's user can't access that vault, the bot's
first `createSigningRequest` fails with a permission error even though the token
itself is valid. You can adjust vault access later under the vault's Team &
policies.

The token is a secret and never goes in `stitch.toml`, it's set via the
environment (or a file the `_FILE` variant points at).

### 3. Generate the sidecar's key

The Client Signer authenticates to MPCVault with an ed25519 key you generate
locally:

```bash
ssh-keygen -t ed25519 -C "mpcvault-client-signer" -f ./client-signer-key -N ""
```

No passphrase. The **public** key (`client-signer-key.pub`) goes into MPCVault in
the next step and into `client_signer_pubkey`. The **private** key stays on the
host and is mounted into the sidecar container (Part B). Do not register a key you
don't hold the private half of.

### 4. Register the Client Signer

In the console, open the **vault → Team & policies** page and click **+ New
Client Signer**. This page is per-vault, not org-level, and you need to be a vault
manager to see it. Fill in:

- a **name** (e.g. `stitch-operator`),
- the **public key** you generated (paste `client-signer-key.pub`),
- an optional **IP whitelist**.

Continue, then **approve the two requests in the MPCVault app**: a *Vault setting
update* (creates the signer) and a *Key grant access* (grants it signing
permission). Both must be approved or the sidecar can't sign.

A Client Signer is bound to one vault. If you ever run multiple vaults you need a
separate signer (and sidecar) per vault.

## Part B: run the client-signer sidecar

### 1. Write `config.yml`

```yaml
http-health:
  listening-addr: 0.0.0.0:8080
vault-uuid: "<vault uuid>"
ssh:
  private-key: |
    -----BEGIN OPENSSH PRIVATE KEY-----
    ...contents of client-signer-key...
    -----END OPENSSH PRIVATE KEY-----
  password: ""
callback-url: "http://<bot host or container>:8088/callback"   # must reach the bot's callback_listen_addr
```

The `private-key` must be the mate of the public key you registered in step A.4.
The `callback-url` must point at the bot's callback server (`callback_listen_addr`,
default port `8088`).

`config.yml` embeds the Client Signer private key, so lock it down before mounting
it, otherwise a default `umask 022` leaves it world-readable and any other local
user or process can copy the co-signer credential:

```bash
chmod 600 config.yml
```

### 2. Start it next to the bot

```bash
docker run -d --name mpcvault-signer --restart unless-stopped \
  --add-host=host.docker.internal:host-gateway \
  -p 8080:8080 -v "$(pwd)/config.yml:/config.yml:ro" \
  ghcr.io/mpcvault/client-signer:latest --config-path=/config.yml
docker logs -f mpcvault-signer   # confirm it connects to the vault
```

The `--add-host=host.docker.internal:host-gateway` flag maps that hostname to the
host so the container can reach the bot's callback. It's required on Linux (Docker
Engine doesn't provide the name by default) and harmless on Docker Desktop
(macOS/Windows), which resolve it anyway. The `-p 8080:8080` is only the sidecar's
own health endpoint; the callback flows **out** of the container to the host, so
it needs no inbound port mapping.

### 3. Make the callback reachable

The sidecar must be able to reach the bot's `callback_listen_addr`. This is the
most common thing to get wrong:

- Bot on the host, sidecar in Docker: use
  `callback-url: http://host.docker.internal:8088/callback`, not `localhost`
  (which points at the container itself). On **Linux** this only resolves if you
  pass `--add-host=host.docker.internal:host-gateway` to the `docker run` above;
  Docker Desktop on macOS/Windows resolves it automatically.
- Both in Docker: share a network or use host networking so `callback-url`
  resolves to the bot.

## Part C: configure Stitch

### Desktop app

Open the Stitch setup app (**Stitch.app** on macOS, `stitch-setup.exe` on
Windows, the `stitch-setup` binary or `stitch.desktop` entry on Linux). Pick
**Signer → MPCVault** and fill Vault UUID, Client-signer public key, Operator
address, and API token. Saving writes the `[signer]`
section, stores the token in an owner-only `mpcvault-api.token`, and points
`stitch.env` at it. The form shows the EXPERIMENTAL notice and a reminder that the
sidecar must be running.

### Manual

Edit `stitch.toml` directly:

```toml
[signer]
provider             = "mpcvault"
vault_uuid           = "<mpcvault vault uuid>"
client_signer_pubkey = "ssh-ed25519 AAAA..."   # the sidecar's public key
operator_address     = "0x<the vault wallet EVM address>"
api_base_url         = "https://api.mpcvault.com"   # optional, this is the default
callback_listen_addr = "0.0.0.0:8088"               # optional, default; where the bot serves the approval callback
poll_timeout_secs    = 30                            # optional; per-request HTTP timeout (min 5)
max_concurrent_signs = 4                             # optional
```

Then set the secret in the environment (never in the config file). Export it so
`stitch` inherits it, a bare `NAME=value` only sets a variable in your current
shell and the bot won't see it:

```bash
export MPCVAULT_API_TOKEN_FILE=/path/to/mpcvault-api.token   # or: export MPCVAULT_API_TOKEN=<token>
```

Equivalently, keep it in a `stitch.env` file and `set -a; source stitch.env; set +a`
before running, or prefix it onto the command itself
(`MPCVAULT_API_TOKEN_FILE=... stitch --config ... --dry-run`).

## Validate with a dry run

A dry run exercises the full signing path (create request → sidecar → callback →
execute) without posting orders or broadcasting:

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

A healthy run logs:

- `MPCVault callback approval server listening` at `callback_listen_addr`,
- for each signature: the sidecar hitting the callback, then `MPCVault callback
  approved (signed payload matches an in-flight request)`,
- an execute response with a signature.

Quick reachability check: `curl http://<bot>:8088/health` returns `200`.

The bot verifies every signature recovers to `operator_address`. If you see
`remote signature did not recover to operator address`, your `operator_address`
doesn't match the vault wallet.

## How signing works (and the callback security model)

Per signature the bot:

1. `POST /v1/createSigningRequest` with the 32-byte digest as the raw-message
   `content`, tagged with the sidecar's public key so MPCVault routes the approval
   to it.
2. MPCVault forwards the request to the sidecar, which calls the bot's callback.
3. The bot's callback **fails closed**: it returns `200` (approve) only when the
   request's actual signed payload (the raw-message `content`) is a digest the bot
   currently has in flight, and `403` otherwise. It correlates on the signed field
   specifically, not a substring of the body, so another vault user can't get the
   bot to approve a different payload by copying an in-flight digest into a decoy
   field. A request the bot didn't create (another vault user, a stolen token) is
   rejected.
4. `POST /v1/executeSigningRequests` returns the ECDSA `{R, S, V}`, which the bot
   normalizes (low-s) and verifies recovers to `operator_address`.

You can sanity-check the gate: `POST` a bogus body to `/callback` and you should
get `403` with `MPCVault callback rejected: signed payload is not an in-flight
digest` in the log.

## Troubleshooting

- **Callback rejects a legitimate in-flight request** — the bot couldn't decode
  the sidecar's callback body. It accepts the protobuf `SigningRequest`
  (`application/octet-stream`) and JSON. If a real sidecar's body doesn't
  correlate, the protobuf field numbers may differ by client-signer version (see
  [Known limitations](#known-limitations)).
- **Sidecar can't reach the callback** — fix `callback-url` / networking (Part
  B.3). The sidecar logs the failed callback.
- **`remote signature did not recover to operator address`** — `operator_address`
  isn't the vault wallet address.
- **REST 401 / auth errors** — the API token is missing, wrong, or your plan
  doesn't include API access.
- **Signing timeouts under load** — MPCVault defaults to `max_concurrent_signs =
  4`; raise `poll_timeout_secs` or lower concurrency if the vault is slow.

## Known limitations

- **In active testing.** MPCVault's live client-signer callback flow is in the
  process of being validated end to end against a live vault. The correlation and
  fail-closed logic are already covered by unit tests, and we're confirming the
  exact protobuf wire format the real sidecar sends on a running setup. We
  recommend starting with a smaller allocation while that wraps up.
- **No Fargate path.** MPCVault needs the sidecar co-located, so it suits an
  EC2/docker-compose or systemd host rather than the single-container Fargate
  Quick Create. The provided CloudFormation template covers local and Turnkey
  directly; MPCVault additionally needs the sidecar running alongside.
- **One sidecar per operator.** MPCVault binds one client-signer per vault and
  each operator needs their own vault and funds, so it can't be run as one shared
  service.

## Config reference

| Field | Required | Default | Notes |
|---|---|---|---|
| `vault_uuid` | yes | — | the vault holding the operator wallet |
| `client_signer_pubkey` | yes | — | the sidecar's ssh ed25519 public key |
| `operator_address` | yes | — | the vault wallet's EVM address |
| `api_base_url` | no | `https://api.mpcvault.com` | override only if MPCVault gives you a different endpoint |
| `callback_listen_addr` | no | `0.0.0.0:8088` | where the bot serves the approval callback |
| `poll_timeout_secs` | no | `30` | per-request HTTP timeout, clamped to a 5s minimum |
| `max_concurrent_signs` | no | `4` | how many signatures the poster requests at once |
| `MPCVAULT_API_TOKEN` / `MPCVAULT_API_TOKEN_FILE` (env) | yes | — | the API token; the `_FILE` path variant takes precedence |
