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
