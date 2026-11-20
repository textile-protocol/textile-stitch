# Production panel access

Keep this panel private through Tailscale Serve. Do not enable Funnel or publish
ports 8420/443 on the host. Tailnet policy controls who can reach the panel;
new installs also require a panel password. Existing identity-only installs
retain their allowlisted access while a persistent **Set panel password** warning
offers setup in the browser. Saving a password disables identity-header login,
revokes other panel sessions and keeps bots running. Complete that migration
before treating an existing panel as hardened. Follow
[the panel upgrade guide](install-panel.md#upgrading-an-older-server-install).

Protect AWS, GitHub, your identity provider and Tailscale administration with
hardware-backed passkeys/security keys. Approve only dedicated administration
devices; remove unused devices and tightly control tag ownership. Device approval
and Tailnet Lock are alternatives, not a combination. Keep recovery material
separate. An already compromised approved laptop can still act as its owner.

## Tailnet policy

Adapt `deploy/tailnet-policy.example.json` in the Tailscale policy editor. Replace
the example admin with your actual login and tag this server `tag:stitch-panel`.
The sample is a standalone restrictive policy; merge deliberately with your
existing tailnet. Grants are additive: retaining a broad allow-all grant can
undo the intended restriction. Test both permitted and denied users before
saving. If your plan supports device-posture conditions, require your approved
administration-device posture on this grant too. A user allowlist alone permits
that user's other enrolled devices; test the device restriction independently.
Do not give the panel node production subnet routes, exit-node access, or SSH.
Review tagged-device key expiry, which is normally disabled.

## Enrollment without a persistent reusable key

Create a short-lived, one-use, non-ephemeral key scoped to the panel tag. Supply
it only for first enrollment. Persist `tailscale-state`; `TS_AUTH_ONCE=true`
reuses that node identity on restart. The bundled CLI shim reapplies SSH/routing
restrictions with `tailscale set` on every boot, independently of enrollment.
After `tailscale status --json` confirms `BackendState=Running`, ensure `.env`
contains `PANEL_MODE=server`, revoke the enrollment key, delete `TS_AUTHKEY` from
`.env`, unset it in your shell and recreate the sidecar with the empty default.
Recreating the sidecar network namespace also requires recreating the panel:

```sh
docker compose -f docker-compose.panel.yml up -d --force-recreate tailscale panel
```

Confirm identity and panel access after recreation and a planned reboot. A
lost/deleted state volume requires a new one-use key; do not silently fall back
to a reusable enrollment secret. The persisted node state is itself a credential:
restrict its host/backup access. The installer may initially write the enrollment
key to `.env`; finish this cleanup before treating enrollment as complete.

## Acceptance and recovery

From an approved device, confirm an operator can reach HTTPS and sign in with
the panel password, and an unapproved device/user cannot reach HTTPS. On legacy
panels, first complete password setup in the browser. Check that direct internet
access to 22/8420 is closed, an allowlisted `Tailscale-User-Login` header without
a session cannot access `/api/bots`, and removing the authorized user/device
from tailnet policy revokes network access even with a valid panel session.
Repeat after restart with no enrollment key configured. Use the scoped
logged SSM session from the infrastructure runbook for emergency administration;
never reopen public SSH as routine recovery.

Tailscale policy controls network access after password setup; `PANEL_USERS` no
longer authorizes requests. Preserve `.stitch-panel-password.json` in fleet
backups. Password recovery and rollback are documented in the panel upgrade guide.
A fully compromised HTTPS proxy can still steal passwords or session cookies.

This repository cannot apply or verify your actual tailnet/identity-provider
policy. Record the live checks in the deployment review.

## Hosted profile

This section covers hosts Textile runs for a client on the
`packages/infra/terraform/stitch-bots` stack, with the panel running from
`docker-compose.panel.hosted.yml` and `STITCH_PANEL_PROFILE=hosted`. Legacy
and self-hosted installs are unaffected by it and must not switch to the
hosted profile until their own migration. What the profile allows is in
[install-panel.md](install-panel.md#hosted-profile).

### What exists, and what you still do by hand

The stack deploys the hosted panel when `hosted_panel_enabled` is on. Here's
where each piece comes from:

- **#2777 (merged):** the stitch-bots stack gives the host its SSM instance
  role, the `<prefix>-logged-shell` document and the `<prefix>-panel` port
  document, which forwards to host port 8420 and nothing else.
- **#2778 (merged):** the hosted profile and the runtime broker in the panel image,
  and `docker-compose.panel.hosted.yml` with its `deploy/` files in this repo.
- **#2832 (merged):** bot keys come from Secrets Manager into tmpfs at
  `/run/stitch/<bot>`, mounted read-only into each bot, and the bot image is
  pinned by digest.
- **`hosted_panel_enabled` (stitch-bots stack):** installs this compose file
  and its `deploy/` files in `/opt/stitch-panel`, writes the `.env` values, the
  envelope and a digest pin for the Tailscale image, and runs it from
  `stitch-panel.service`. The auth key and password hash come from Secrets
  Manager, never Terraform. Its plan, and the start script on the host, refuse
  a compose file whose panel service mounts the bot folders, anything above
  them or `/run/stitch`. This file passes: only the broker mounts
  `PANEL_BOTS_DIR` (#3091). With the panel on, the bot folders live in
  `~/stitch/bots/<bot>`, so `PANEL_BOTS_DIR` holds them and nothing else. See
  the stack's README, "Hosted panel". The tailnet, its policy and the key
  itself are still yours.
- **Image attestations:** Update and Rollback only move a bot onto a digest
  with a valid build provenance attestation, checked by the broker. See
  [Image attestations](#image-attestations).

On a stack without `hosted_panel_enabled`, bring the panel up by hand. The
panel mounts no bot directory: the runtime broker mounts `PANEL_BOTS_DIR`,
hands the panel only each enrolled bot's `stitch.toml` and change log, and
takes quoting edits it re-checks against the envelope itself. A key left beside
a config (`rfq-api.key` is there by default) never reaches the panel, though
moving it to `/run/stitch/<bot>` with `STITCH_RFQ_API_KEY_FILE` still keeps it
off the broker's mount. What the panel can and can't reach is in
[install-panel.md](install-panel.md#hosted-profile). Over the logged SSM shell:

1. Copy `docker-compose.panel.hosted.yml`, `deploy/panel-serve.json` and
   `deploy/panel-tailscale.sh` from the reviewed commit into a root-owned
   directory, for example `/opt/stitch-panel`.
2. Write `/opt/stitch-panel/.env` (mode 0600) with `PANEL_IMAGE` pinned by
   `@sha256:` digest, `PANEL_PASSWORD_HASH` (quoted, from
   `stitch-panel hash-password`), `PANEL_PUBLIC_ORIGIN`, `PANEL_BOTS_DIR` (a
   directory holding bot folders and nothing else, like `~/stitch/bots`; never
   `~/stitch`, which holds the bots' compose file), `STITCH_BROKER_BOTS` (the
   bot container names from the stack's compose file, such as `stitch-bot-a`)
   and a one-use `TS_AUTHKEY`.
   The compose file's header lists every variable.
3. Write `envelope.json` next to it with the bounds agreed with the client,
   in the shape [install-panel.md](install-panel.md#hosted-profile) shows.
   The panel and the broker both mount it read-only. Without it, or with `{}`
   (what `deploy/hosted-envelope.example.json` holds), no quoting edits are
   allowed.
4. `docker compose -f docker-compose.panel.hosted.yml up -d`, then do the
   enrollment cleanup below and record the checks at the end of this section.

**Daily: the tailnet.** The host joins a dedicated, hardened tailnet through
the Tailscale sidecar (`stitch-panel-tailscale`), and `tailscale serve`
proxies HTTPS on 443 to the panel on `127.0.0.1:8420` inside the sidecar's
network namespace. Open `https://<hostname>.<tailnet>.ts.net` from an approved
device, phone included, and sign in with the panel password. Set up the
tailnet as in [Tailnet policy](#tailnet-policy) and enroll as in
[Enrollment](#enrollment-without-a-persistent-reusable-key), with one
difference: the hosted file is `docker-compose.panel.hosted.yml`, so recreate
with

```sh
docker compose -f docker-compose.panel.hosted.yml up -d --force-recreate tailscale panel
```

**Break-glass: SSM.** The hosted compose file also publishes the panel on host
loopback only: `127.0.0.1:8420:8420` on the sidecar, which owns the network
namespace. Nothing off the box can reach it. The stitch-bots stack's
`<prefix>-panel` Session Manager document (from #2777) forwards to exactly that
port:

```bash
aws ssm start-session --target <instance-id> --document-name <prefix>-panel \
  --parameters localPortNumber=8420
```

Then open `http://localhost:8420`. Use local port 8420: the panel accepts
changes only from `http://localhost:8420` and from its `PANEL_PUBLIC_ORIGIN`
(the tailnet HTTPS origin), and refuses any other `Host` or `Origin` with a
403. A random local port shows the page but every change is refused.

Both paths need the panel password. The hosted panel never trusts
`Tailscale-User-Login`: with the loopback publish, any process on the host can
reach the listener and could send that header, so the panel refuses to start
if `STITCH_PANEL_TRUST_IDENTITY_HEADER` is set.

Checks to record when bringing a host up: the tailnet URL works from an
approved device and not from one outside the ops group; the SSM forward on
local port 8420 reaches the login page and accepts a change; a forward on
another local port is refused on change; `ss -ltn` on the host shows 8420 on
`127.0.0.1` only; the panel container has no `/var/run/docker.sock`, no
`/home/stitch/secrets` and no bind mount of the bot directories
(`docker inspect stitch-panel` lists only the envelope file and the broker
socket volume); a quoting save from the panel shows up in the bot's
`change-log.jsonl` on the host.

### Image attestations

On a hosted host, every bot image swap (Update and Rollback are the same call)
needs a build provenance attestation for the exact digest. The runtime broker
verifies it before it pulls or stops anything, and refuses on any failure.
The policy is fixed in `src/panel/attestation/mod.rs`, not in config:

- signed through Sigstore's public-good instance: the certificate chains to
  the pinned Fulcio roots, its SCT and the Rekor inclusion proof, checkpoint
  and signed entry timestamp verify against the pinned keys, and the
  certificate was valid when Rekor logged it
- certificate identity exactly
  `https://github.com/textile-protocol/textile-stitch/.github/workflows/container.yml@refs/heads/main`,
  issuer `https://token.actions.githubusercontent.com`, source repository
  `textile-protocol/textile-stitch`, source ref `refs/heads/main`, runner
  environment `github-hosted`
- the signed statement is SLSA provenance v1 with one subject,
  `ghcr.io/textile-protocol/textile-stitch` at the requested digest

The panel image has its own identity (`container-panel.yml`, subject
`textile-stitch-panel`), so its attestation never passes for a bot and the
other way round. The broker only ever moves bots onto the bot image: if
`STITCH_BOT_REPOSITORY` in `.env` names any other repository, every swap is
refused. Leave it unset.

The broker has no network, so it can't fetch anything. The panel downloads the
bundles from GHCR (`ghcr.io`, through the OCI fallback tag `sha256-<hex>`,
since GHCR has no referrers API) and passes them to the broker over the
socket. That's safe because a bundle proves itself: a compromised panel can
withhold one or send garbage, which refuses the swap, but it can't produce a
valid one for an image our CI didn't build. Verification is offline: no TUF
refresh, no Rekor lookup, no `api.github.com` and no token. The host needs
outbound HTTPS to `ghcr.io:443` and `pkg-containers.githubusercontent.com:443`
(where GHCR redirects blob downloads), the same two hosts the Docker daemon
already uses to pull the image. If egress is restricted to those, swaps still
work. If GHCR is unreachable, swaps are refused, which is the point.

Both the panel and the broker log the verified workflow, commit and digest.
To see why a swap was refused, read the broker's log:

```sh
docker logs stitch-runtime-broker 2>&1 | grep -E 'unattested|verified the image'
```

**Trust root.** The Sigstore trust root (`trusted_root.json`: Fulcio CAs,
Rekor and CT log keys) is compiled into the panel image from
`src/panel/attestation/sigstore_trusted_root.json`. It never updates at
runtime. Sigstore rotates keys rarely and announces it; when it does, or when
`sigstore/root-signing` publishes a new `targets/trusted_root.json`, refresh it
with a reviewed PR: replace the file with the new one from that repo, check it
byte for byte against the `trusted_root.json` target the TUF repository
(`tuf-repo-cdn.sigstore.dev`) serves, run the attestation tests, and roll out
the new panel image. Until a
host gets the new image, attestations signed under keys it doesn't know are
refused, so roll the panel before the old keys stop being used.

Images published before attestations started (the mirror's first attested
build was on 19 November 2026) have none, so rollback can only go back to
digests built since then.
