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

## sigstore-verify for hosted image attestations

Added 19 November 2026 for the hosted runtime broker's image attestation check
(docs/production-access.md#image-attestations). Panel feature only: the bot
binary and image don't link it.

`sigstore-verify 0.16.0` is the Sigstore project's own Rust verifier
(github.com/sigstore/sigstore-rust, Apache-2.0), the successor line to the
older `sigstore` crate. It's built with no default features plus `aws-lc-rs`,
so the TUF client, the Rekor/Fulcio/TSA network clients and their HTTP stack
aren't compiled in, and nothing in it can reach the network. aws-lc-rs was
already in the graph through rustls and the AWS SDK. Verification runs against
our own pinned copy of Sigstore's `trusted_root.json`
(src/panel/attestation/sigstore_trusted_root.json), not the one the crate
embeds, so a crate bump can't change the trust root without a visible diff.
On 19 November 2026 the two were byte-identical to `sigstore/root-signing`'s
`targets/trusted_root.json` (sha256 `6494e21e…bc0b66`).

There is no published third-party security audit of sigstore-rust that we
could find. What we checked instead: the bundles `actions/attest-build-provenance`
writes (v0.3, DSSE, Rekor v1 entries with inclusion proof and signed entry
timestamp) verify with it; it binds the in-toto subject digest to the
requested digest, checks the certificate at the Rekor-signed time, and refuses
a wrong identity, untrusted CA, CA outside its validity window, missing log
entry, rewritten statement and swapped certificate (all covered by
src/panel/attestation/tests.rs against real bundles). Our policy on top of it
(identity, repository, ref, runner, predicate type, subject name) is our code
and tested separately.

It adds 27 crates to the lockfile and moves none that were there: the
sigstore-* family (bundle, crypto, merkle, rekor, trust-root, tsa, types,
verify), RustCrypto's cms, x509-cert, der_derive, pem-rfc7468, tls_codec(_derive)
and flagset, jiff and its tz crates, serde_json_canonicalizer, ryu-js, pem 4,
base64 0.23, untrusted 0.7, portable-atomic(-util). OSV had no advisories for
any of them, and `cargo audit` reports the same 22 advisories with and without
the change (all pre-existing and covered above or desktop-only). Re-run both
when bumping sigstore-verify, and treat a change to its certificate, SCT or
tlog checks as a security review, not a routine update.
