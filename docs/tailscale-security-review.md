# Tailscale report review — October 26, 2026

The [CyberSecurityNews article](https://cybersecuritynews.com/tailscale-ssh-flaws/)
points to [Tailworm, the original researcher report](https://www.tailworm.com/).
It describes test results against Tailscale 1.104.2 and headscale 0.29.5.
There is source evidence for the SSH forwarding concern. This review did not
reproduce the exploit or verify production exposure. The report mixes exploit
claims with configuration hazards and criticisms of the trust model; those
should not all be treated as remotely exploitable vulnerabilities.

## What we checked

In [Tailscale 1.104.2's SSH implementation](https://github.com/tailscale/tailscale/blob/v1.104.2/ssh/tailssh/tailssh.go),
Unix forwarding callbacks run in the daemon process, rather than the user's
session process. Its pinned
[gliderssh forwarding implementation](https://github.com/tailscale/gliderssh/blob/1a0f895faf28/streamlocal.go)
checks allowed paths before separately connecting, unlinking or binding. Those
operations support the reported privilege and check/use race concern.
Exploitation still depends on SSH being enabled, policy allowing the attacker
an SSH session, and forwarding permissions. The simple Docker demonstration
uses a socket in `/tmp`; the claimed stock Docker socket takeover needs the
separate race exploit, which we have not reproduced.

The reported Serve HTTP/2 authorization issue needs multiple mounts with
different `AcceptAppCaps` and concerns capabilities of the same peer across
mounts. It does not demonstrate arbitrary user identity impersonation.

The [vendor bulletin page](https://tailscale.com/security-bulletins), checked on
October 26, has no matching Tailworm advisory or announced fixed version. The
latest listed bulletin is TS-2026-013, dated October 21, about compromised TLS
intermediate certificates. A public issue search in `tailscale/tailscale` for
`tailworm` also returned no matches. This does not establish that the vendor
has not received a private report.

TS-2026-004 and TS-2026-005 acknowledge earlier Unix socket forwarding issues,
fixed in 1.98.9. Those fixes do not establish that these newer claims are fixed.

## Textile's relevant configuration

The relevant use found in this repository is Stitch's server panel Compose
stack. It runs Tailscale Serve in a sidecar, shares the network namespace with
the panel and exposes a single HTTPS mount to `127.0.0.1:8420`. The checked-in
configuration has no Funnel, `AcceptAppCaps` mounts, headscale or app connector
setup. SSH was not enabled by default, but arbitrary `TS_EXTRA_ARGS` could
enable it, and saved node preferences needed checking.

The panel mounts the host Docker socket and can access bot configuration. Its
previous default accepted an allowlisted `Tailscale-User-Login` as sufficient
authentication. A password hash was an alternative credential, not a second
required check. Someone able to forge that trusted header on the shared
loopback could gain the panel's Docker privileges. That is an architectural
trust dependency, not proof that Tailworm compromises this deployment.

The main API uses ECS Fargate and ECS Exec in the infrastructure definitions
reviewed. No Tailscale dependency was found there. Live hosts and tailnet
policies were not inspected; repository searches cannot exclude manually
installed agents or configuration outside this repository.

## Changes and rollout

Fresh server installs require password sessions, with both identity trust flags
off. Existing identity-login operators retain access through a compatibility
path, with a persistent **Set panel password** warning in the UI. Only an
already authenticated operator can save a password. Saving it publishes an
owner-only Argon2id hash in the mounted fleet root, disables header login,
revokes other sessions, and keeps the submitting operator signed in. The saved
file takes precedence over legacy environment flags on every startup. No bots
restart. There is no automatic migration deadline: legacy exposure remains
until an operator completes setup.

The UI Update button preserves current auth settings and storage, so operators
can install this panel image and migrate without a terminal. It does not update
Compose or the sidecar. Re-running the installer also preserves recognized
legacy allowlists and enables the same browser migration. A fresh install with
no credentials is refused. A fallback password alone does not disable the old
identity bypass; the migration warning still applies.

The updated Compose file keeps `TS_AUTH_ONCE=true`: an enrolled node must not
try to authenticate again with an expired enrollment key. First enrollment uses
`--reset --ssh=false`. Since containerboot does not pass `TS_EXTRA_ARGS` to its
enrolled-node `tailscale set` call, a bundled CLI shim adds explicit settings
that disable SSH, accepting/advertising routes, using/advertising an exit node
and advertising an app connector. This runs on every boot before Serve is
configured; a failure propagates to containerboot and stops startup. This
ordering was checked against Tailscale 1.104.2's
[containerboot implementation](https://github.com/tailscale/tailscale/blob/v1.104.2/cmd/containerboot/main.go).
Assign enrollment tags through the admin console or auth key instead of arbitrary
extra arguments. Apply the updated Compose file, `deploy/panel-tailscale.sh` and
Serve config, then recreate the sidecar to receive this part of the hardening.

Limit the panel node's TCP 443 to operators in tailnet policy and remove broader
allow rules. `PANEL_USERS` is retained for legacy login until UI setup; it no
longer authorizes requests after a password is saved. Follow the
[upgrade and live checks](install-panel.md#upgrading-an-older-server-install) to
verify SSH is off, Serve has one mount and, after migration, a header alone
cannot authenticate. Preserve the password file in fleet backups and follow the
documented recovery/rollback steps; older images do not read that file.

Password sessions protect against identity-header spoofing alone. A fully
compromised TLS proxy could still intercept passwords or session cookies. This
patch does not contain a full host compromise or remove the panel's intentional
Docker privileges. Limit signer permissions and bot inventory, review Tailscale
updates and revoke compromised nodes and credentials when necessary. The sidecar
still tracks `stable`; its running version and image digest need operational
review. No version is claimed here to fix the new report.
