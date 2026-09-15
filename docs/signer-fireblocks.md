# Fireblocks Signer Setup

Fireblocks is one of Stitch's MPC signer backends. The operator key lives in a
Fireblocks vault account instead of a local `stitch.key`, and the bot asks
Fireblocks to sign each order.

Unlike MPCVault there is no sidecar to run. The API Co-Signer that actually
signs is part of your Fireblocks workspace, not something you deploy next to the
bot, so from Stitch's side this is plain HTTPS — same shape as Turnkey.

The thing that makes this setup different from the other two: **Stitch signs
Fireblocks typed messages, not raw payloads.** Raw Signing is a paid entitlement
Fireblocks only turns on after a conversation with your Customer Success
Manager. Typed message signing isn't — you write one policy rule in the console
and you're done. Everything a quoting bot signs (RFQ quotes, resting ladder
orders, the venue handshake, enrolment) is EIP-712, so typed messages cover it.

What they don't cover is on-chain transactions. A transaction signing hash isn't
typed data, so the taker and closer legs need raw signing. Stitch refuses that
combination at config time rather than failing on the first fill. The ladder
(`book_enabled`) is fine — it only rests signed orders. See [The on-chain
legs](#the-on-chain-legs) below.

This is the full walkthrough. The one-screen config reference lives in
[ADVANCED.md](ADVANCED.md#mpc-wallet-signers).

## What you need from Fireblocks

Two things, and the panel works out the rest:

| Config field | Where it comes from |
|---|---|
| `FIREBLOCKS_API_KEY` (env, not secret) | the API user's key, a UUID |
| `FIREBLOCKS_API_PRIVATE_KEY[_FILE]` (env, secret) | `fireblocks_secret.key`, the RSA PEM |
| `vault_account_id` | **discovered** — the panel lists your vault accounts |
| `operator_address` | **discovered** — proved by a test signature |

Fireblocks has no read-only credential: every request is signed with the RSA
key, so the same pair that will sign your orders is what reads the vault list.
That's what makes the discovery possible.

## 1. Create the API user

In the Fireblocks console, go to **Settings → Users → Add user** and create an
**API user** with the **Signer** role. Signer is the role that can actually
produce signatures; an Editor or Viewer key will read your vaults fine and then
fail on the first sign.

Fireblocks asks for a CSR. Generate the key pair locally:

```bash
openssl req -new -newkey rsa:4096 -nodes \
  -keyout fireblocks_secret.key -out fireblocks.csr
```

Upload `fireblocks.csr`. Keep `fireblocks_secret.key` — that's the secret, and
Fireblocks never sees it. When the user is approved you get the **API key**
(a UUID). That plus the key file is everything.

The API user has to be attached to a **Co-Signer**. If your workspace already
signs programmatically, it already has one. If not, this is the piece to sort
out first — without a co-signer nothing signs automatically and every request
waits for a human in the mobile app, which will not work for a quoting bot.

## 2. Pick (or create) the vault account

Any vault account works. Note that on EVM chains a Fireblocks vault account has
**one address across every EVM network** — Celo, Base, Arbitrum, Ethereum, BSC
all share it — so one vault account covers every corridor you run.

The vault needs an **ETH asset wallet** for the panel to read its address. If
the vault is new, add the ETH asset to it. (You don't need ETH the token, or an
Ethereum corridor; this is how Fireblocks exposes the EVM address.)

Fund it with a little native gas on each chain you trade — Permit2 approvals
need it.

## 3. Add the Typed Message policy rule

This is the step people miss, and the failure is opaque without it: signing
requests come back `BLOCKED` or `REJECTED`.

In the console, go to **Policies** and add a **Typed Message** rule that allows
your API user to sign for the operator vault account. Scope it to that vault
account — a rule that allows typed-message signing for the whole workspace lets
this key sign for vaults it has nothing to do with.

Make sure the rule **auto-approves**. A rule that routes to a human approver is
technically fine and practically useless: the venue stops listening for a quote
about 750 ms after it asks.

## 4. Configure the bot

**Desktop / panel.** Open the bot's Settings, pick **Signer → Fireblocks**, and
paste the API key and the contents of `fireblocks_secret.key`. Then:

1. Pick your **workspace region** if it isn't the global one — an EU or US-East
   workspace answers only on its own host, and the same host is written into
   `stitch.toml` so the bot signs where you verified.
2. **Load vault accounts** — the dropdown fills from your workspace.
3. Pick the vault account.
4. **Verify** — the panel signs one throwaway message for real.

Verify is worth doing even though it's optional-looking, because it's the only
thing that checks the whole path at once: the credentials parse, the co-signer
is online, the policy rule exists and auto-approves, the vault resolves to an
address, and how long a signature actually takes. It signs a payload under its
own domain that authorises nothing — no gas, no chain, no order.

The address it reports is the one written to `stitch.toml`. It's not read off an
API response; it's the address the signature recovered to.

**Manual.** Edit `stitch.toml` directly:

```toml
[signer]
provider         = "fireblocks"
vault_account_id = "0"
operator_address = "0x<the EVM address that vault account resolves to>"
asset_id         = "ETH"                        # optional default
api_base_url     = "https://api.fireblocks.io"  # optional default; regional endpoints (eu-api, eu2-api, us-east-1-api, sandbox-api) are also accepted
poll_interval_ms = 50                           # optional
max_concurrent_signs = 4                        # optional
```

Then set the env (secrets never go in the config file):

```
FIREBLOCKS_API_KEY=<the API key UUID>
FIREBLOCKS_API_PRIVATE_KEY_FILE=/path/to/fireblocks_secret.key
```

## Testing against the Fireblocks sandbox

Worth doing before you point this at a real workspace. The [Developer
Sandbox](https://www.fireblocks.com/developer-sandbox-sign-up) is free and
testnet-only, ships with an API user already created, and uses Fireblocks'
Communal Test Co-signer — so there's no co-signer to deploy.

Set the region picker to **Sandbox** (or `api_base_url =
"https://sandbox-api.fireblocks.io"`), then run Verify as normal.

Two things the sandbox **cannot** tell you, both because it auto-approves every
transaction and its policies are not editable:

- Whether your production Typed Message policy rule is correct. Sandbox has no
  rule to write, so a sandbox pass says nothing about step 3 above.
- What signing actually costs you. Auto-approval is the best case; a production
  TAP rule adds latency on top, so treat a sandbox latency number as a floor,
  not a forecast.

## 5. Validate with a dry run

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

A healthy run logs the maker address (it must equal your `operator_address`) and
one `Fireblocks typed-message signature` line per signature with its elapsed
time. The bot verifies every signature recovers to `operator_address` before
using it.

## Latency

Worth being blunt about: Fireblocks signing is asynchronous. The bot creates a
transaction and polls it, where Turnkey is a single synchronous call. The venue
gives a maker about 750 ms to answer a quote request, and makers on the live
venue answer in ~22 ms.

Measure yours with Verify before committing to it. Under ~400 ms per signature
you have room; well above that and you'll quote unreliably, winning less than
your prices deserve. If Verify reports a slow signature, the usual cause is a
policy rule that isn't auto-approving.

This is why Fireblocks is marked experimental in the panel. Nothing about it is
unsafe — every signature is verified before use — but whether it's fast enough
depends on your workspace, and that's yours to measure, not ours to promise.

## The on-chain legs

Typed messages can't sign transactions. That rules out exactly two things: the
taker leg (`limit_taker_enabled`) and the closer, both of which call the reactor
on chain. Stitch rejects those at config time and says so.

Quoting and the ladder are unaffected — RFQ quotes and resting `book_enabled`
orders are signed Permit2 orders, not transactions, and cost no nonce.

Two ways forward:

**Stay typed-only (recommended to start).** Quote RFQ, rest the ladder, skip the
taker and closer. The one on-chain thing you still need is the Permit2 approval —
a plain ERC-20 `approve` per input token, once per chain. Send it from the
Fireblocks console; `stitch approve` won't do it for you without raw signing.

**Enable Raw Signing.** Only needed for the taker and closer legs (and to let
`stitch approve` run from here). Ask Fireblocks to turn it on for the workspace,
add a policy rule for raw signing scoped to your vault account, and set:

```toml
raw_signing = true
```

Then everything works as it does on the other backends, including `stitch
approve`, the taker and the closer.

## Troubleshooting

- **`Fireblocks refused the typed-message request: BLOCKED`** — no Typed Message
  policy rule, or one that doesn't cover this vault account and API user. Step 3.
- **Signing works but takes seconds** — the policy rule is routing to a human
  approver instead of the co-signer. Make it auto-approve.
- **`vault account N has no ETH wallet yet`** — add the ETH asset to the vault
  in the console. Step 2.
- **`the Fireblocks API private key is not a usable RSA PEM`** — paste the whole
  `fireblocks_secret.key`, including the `BEGIN`/`END` lines.
- **`api_base_url host ... is not an official fireblocks API host`** — if your
  workspace is in a region, point `api_base_url` at its endpoint
  (`https://eu-api.fireblocks.io` and friends are accepted).
- **`remote signature did not recover to operator address`** — `operator_address`
  doesn't match the vault. Re-run Verify and let it fill the field in.
- **`this signer signs EIP-712 typed messages only`** — the config turns on the
  taker or closer leg. See [The on-chain legs](#the-on-chain-legs).
- **Quotes rejected as `Busy` with `signing did not finish inside the reply
  budget` in the log** — signing is slower than the venue's window. Same cause as
  a slow Verify: a policy rule that isn't auto-approving.
