# Security dependency update — 24 October 2026

Rebuild and deploy both bot and panel images from this change. A lockfile update
does not patch running containers. Record the reviewed release digests; do not
use the panel Update action to move a pinned deployment to an unreviewed tag.

The Rust lockfile updates webpki-roots to 1.0.11, rustls-webpki to 0.103.16,
rustls to 0.23.46, Tokio to 1.54.1, h2 to 0.4.20, ctutils to 0.4.5 and
webpki-root-certs to 1.0.11, plus the required AWS-LC changes. This removes the
matched TLS trust, rustls handshake, Tokio I/O-safety, h2 empty-frame and ctutils
advisories from the updated lockfile. RustSec RUSTSEC-2026-0381/0382 explain the
urgent certificate-trust changes. The frontend moves to React Router 7.18.4 in
its existing declarative BrowserRouter mode and updates nanoid/source-map-js.

## Remaining advisories are not a clean security bill

An OSV recheck of 911 unique package/version pairs still matches 13 packages,
including maintenance notices, desktop-only dependencies and aliases.

`crypto-bigint 0.5.5` remains in the enabled Linux graph through k256/elliptic-curve
and other cryptographic parents (RUSTSEC-2026-0387). Its fixed 0.7.6 series is not
semver-compatible with those parents. Do not override the transitive version or
suppress the warning: migrating the signing/derivation SDKs needs separate
cryptographic compatibility testing. Practical key recovery is not established.
Until that migration, prefer isolated remote signing and prevent untrusted local
co-tenancy; those mitigations do not make the dependency advisory disappear.

`jsonwebtoken 9.3.1` matches GHSA-h395-gr6q-cpjc. The reviewed Fireblocks integration
only creates RS256 JWTs; it does not validate attacker-supplied JWTs. Panel login
does not use JWTs. A future validation call must revisit this assessment.
`paste` is an enabled maintenance notice. quinn-proto, old rand, GLib, zbus_names,
zvariant, derivative, instant and proc-macro-error require separate build/desktop
triage; they are absent from the checked normal Linux panel tree.

Before production, run `cargo test --locked --features panel`, frontend build/tests,
and built-image scanning. On staging use test wallets/assets to check RFQ
reconnection, signer startup, quote/transaction signatures, panel login,
start/stop/restart and persistence of the nonce ledger across container recreation.
Verify actual running image digests after rollout and retain a compatible rollback.
These source tests do not establish a live production deployment's security.

Sources: https://rustsec.org/advisories/RUSTSEC-2026-0381.html,
https://rustsec.org/advisories/RUSTSEC-2026-0382.html,
https://rustsec.org/advisories/RUSTSEC-2026-0387.html and https://osv.dev/.
