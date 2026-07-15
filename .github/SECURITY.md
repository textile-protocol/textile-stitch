# Security Policy

Stitch is an operator bot: it holds a wallet private key and signs and submits
transactions on your behalf. A bug here can move real funds, so if you've found a
vulnerability we want to hear from you before anyone else does.

## Reporting a vulnerability

**Do not open a public GitHub issue, pull request, or discussion for a security
bug.** Public disclosure before a fix is released puts operators' funds at risk.

Report privately through one of:

- **Email** — security@textilecredit.com. This is a monitored inbox and the
  reliable way to reach us.
- **GitHub private vulnerability reporting** — if enabled on this repo, use the
  "Report a vulnerability" button on the [Security tab](https://github.com/textile-protocol/textile-stitch/security).

Please include:

- A description of the vulnerability and its impact.
- The affected version (`stitch --version`), OS, and how you're running it
  (desktop app, Docker, systemd, cargo build).
- Step-by-step reproduction, ideally with a failing test, a minimal config, or a
  PoC. Redact any private keys, RPC URLs with credentials, and wallet addresses
  you don't want public.
- Your assessment of severity and any suggested fix.

## What to expect

- **Acknowledgment** within 48 hours.
- **Initial assessment** (severity, in/out of scope) within 5 business days.
- We'll keep you posted as we work on a fix and agree a coordinated disclosure
  date with you. Please don't disclose publicly until a fixed release is out and
  operators have had a reasonable window to upgrade.
- Credit in the release notes and advisory if you want it — say so in your report.

## Scope

This policy covers **the Stitch operator bot in this repository only** — the
`stitch` binary, its source, and the official release artifacts. It does not
cover the rest of the Textile protocol.

**In scope**

- The `stitch` bot source in this repository (market-making and limit-order
  taking, quoting/pricing, position closing, funding).
- Handling of operator secrets: `STITCH_PRIVATE_KEY` / `STITCH_PRIVATE_KEY_FILE`
  and any path where a key or seed could leak (logs, error output, memory,
  config files, process arguments).
- Transaction and signing logic: EIP-712 payload construction, Permit2
  approvals, nonce/tx handling — anything that could sign or submit something the
  operator didn't intend, or drain the operator wallet.
- Trust boundaries around external inputs: RPC responses, indexer/feed data, and
  config parsing that could be manipulated to make the bot mis-price, over-spend,
  or crash.
- Integrity of the official release artifacts published from this repo (binaries,
  Docker image, desktop installers) and the release/signing workflows in
  `.github/workflows/`.

**Out of scope**

- The Textile smart contracts, subgraphs, web app, and backend APIs — these live
  outside this repository and are not covered here.
- Losses caused by operator misconfiguration that the docs warn against: keys
  committed to `stitch.toml` or shell history, over-broad approvals, running live
  without `--dry-run` after a pricing change, or funding more inventory than
  intended. See the "Security Notes" in the README.
- The security of third-party RPC providers, block explorers, or signer services
  (e.g. MPCVault) — report those to the respective vendor.
- Vulnerabilities only reachable with a compromised operator machine or a
  private key the attacker already controls.
- Gas-optimization or code-quality suggestions with no security impact.
- Automated scanner output without a demonstrated, exploitable impact.

## Bug bounty

<!-- TODO: pick one and delete the other -->
There is no formal bug bounty at this time. We may still reward high-impact
reports at our discretion.

<!-- OR, once a program exists:
Rewards are handled through [Immunefi](https://immunefi.com/bounty/textile);
severity and payouts follow the tiers published there. -->

## Supported versions

Only the latest published release receives security fixes. Because Stitch signs
and submits transactions, running an outdated version is a risk in itself — the
desktop app's Update button and `stitch update` exist to keep you current. Always
upgrade to the latest release before reporting to confirm the issue still exists.

## Safe harbor

We consider good-faith security research conducted under this policy to be
authorized. We won't pursue legal action against researchers who follow it, avoid
privacy violations and disruption of other operators, only ever test against
wallets and funds they control, and give us reasonable time to fix issues before
disclosing. If you're unsure whether something is authorized, ask first via the
channels above.
