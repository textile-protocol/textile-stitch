# Fireblocks Signer Setup

Fireblocks is one of Stitch's MPC signer backends. The operator key lives in a
Fireblocks vault account instead of a local `stitch.key`, and the bot asks
Fireblocks to sign each order.

Unlike MPCVault there is no sidecar to run. The API Co-Signer that actually
signs is part of your Fireblocks workspace, not something you deploy next to the
bot, so from Stitch's side this is plain HTTPS, the same shape as Turnkey.

The thing that makes this setup different from the other two: **Stitch signs
Fireblocks typed messages, not raw payloads.** Raw Signing is a paid entitlement
Fireblocks only turns on after a conversation with your Customer Success
Manager. Typed message signing isn't. You write one policy rule in the console
and you're done. Everything a quoting bot signs (RFQ quotes, resting ladder
orders, the venue handshake, enrolment) is EIP-712, so typed messages cover it.

What they don't cover is on-chain transactions. A transaction signing hash isn't
typed data, so the taker and closer legs need raw signing. Stitch refuses that
combination at config time rather than failing on the first fill. The ladder
(`book_enabled`) is fine, it only rests signed orders. See [The on-chain
legs](#the-on-chain-legs) below.

This is the full walkthrough. The one-screen config reference lives in
[ADVANCED.md](ADVANCED.md#mpc-wallet-signers).

## What you need from Fireblocks

Two things, and the panel works out the rest:

| Config field | Where it comes from |
|---|---|
| `FIREBLOCKS_API_KEY` (env, not secret) | the API user's key, a UUID |
| `FIREBLOCKS_API_PRIVATE_KEY[_FILE]` (env, secret) | `fireblocks_secret.key`, the RSA PEM |
| `vault_account_id` | **discovered**, the panel lists your vault accounts |
| `operator_address` | **discovered**, proved by a test signature |

Fireblocks has no read-only credential: every request is signed with the RSA
key, so the same pair that will sign your orders is what reads the vault list.
That's what makes the discovery possible.

## 1. Create the API user

**Sort the Co-Signer out before you touch the user.** This is the order you hit
it in, not an afterthought: the **Signer** role is the one that holds an MPC key
share, and Fireblocks only offers it once the workspace has an API Co-Signer to
hold that share. Create the user first and `Signer` simply will not be in the
role dropdown, with nothing on screen explaining why.

If your workspace already signs programmatically it already has a co-signer. If
it doesn't, that is the piece to sort out first. Without one nothing signs
automatically and every request waits for a human in the mobile app, which will
not work for a quoting bot.

On the **Developer Sandbox** you can skip this entirely: sandboxes ship with an
API user already created and wired to Fireblocks' Communal Test Co-signer. Look
under **Settings → Users** for the existing one rather than adding another. See
[Testing against the Fireblocks sandbox](#testing-against-the-fireblocks-sandbox).

With a co-signer in place, go to **Settings → Users → Add user** and create an
**API user** with the **Signer** role. Two roles that look plausible and are not:

- **Editor** / **Viewer**: these read your vaults fine and then fail on the
  first sign. Signing is exactly what they can't do.
- **Embedded Wallet Signer**: a different product. Embedded Wallets (NCW) are
  end-user wallets; this role signs for those, not for the vault account Stitch
  trades from. The console's own description says "for embedded wallets".

Fireblocks asks for a CSR. Generate the key pair locally:

```bash
openssl req -new -newkey rsa:4096 -nodes \
  -keyout fireblocks_secret.key -out fireblocks.csr
```

`openssl req` will prompt for country, organisation and so on; Fireblocks does
not care what you put, so press enter through them, or skip the prompts with
`-subj "/CN=stitch"`. `-nodes` means "don't encrypt the private key", which is
required: the bot reads it unattended and there is nobody to type a passphrase.

That one command writes **both** files. Upload `fireblocks.csr`. It is the
public half and not sensitive. Keep `fireblocks_secret.key`; that's the secret,
Fireblocks never sees it, and it is the file you give the panel. When the user is
approved you get the **API key** (a UUID). That plus the key file is everything.

## 2. Pick (or create) the vault account

Any vault account works. Note that on EVM chains a Fireblocks vault account has
**one address across every EVM network**. Celo, Base, Arbitrum, Ethereum and
BSC all share it, so one vault account covers every corridor you run.

The vault needs at least one **EVM asset wallet** for the panel to read its
address. If the vault is new, add one: **Ethereum** on a mainnet or testnet
workspace, or a testnet asset such as **ETH_TEST5** on a Sandbox, which is
testnet-only and cannot hold mainnet ETH. Any EVM asset does: they all share the
same address, so the panel takes whichever one your vault has. You don't need to
hold the token, or trade that chain. Adding the asset is just how Fireblocks
mints the wallet and its address.

Fund it with a little native gas on each chain you trade. Permit2 approvals
need it.

## 3. Add the Typed Message policy rule

This is the step people miss, and the failure is opaque without it: signing
requests come back `BLOCKED` or `REJECTED`.

In the console, go to **Policies** and add a **Typed Message** rule that allows
your API user to sign for the operator vault account. Scope it to that vault
account. A rule that allows typed-message signing for the whole workspace lets
this key sign for vaults it has nothing to do with.

Make sure the rule **auto-approves**. A rule that routes to a human approver is
technically fine and practically useless: the venue stops listening for a quote
about 750 ms after it asks.

## 4. Configure the bot

**Desktop / panel.** Open the bot's Settings, pick **Signer → Fireblocks**, and
enter the API key. For the private key, drop `fireblocks_secret.key` onto the
field (or **Choose file…**). Pasting the contents still works if you'd rather.
Once a whole key is in, the panel hides it and shows a confirmation; **Replace**
puts the editor back. Then:

1. Pick your **workspace region** if it isn't the global one. An EU or US-East
   workspace answers only on its own host, and the same host is written into
   `stitch.toml` so the bot signs where you verified.
2. **Load vault accounts**, and the dropdown fills from your workspace.
3. Pick the vault account.
4. **Verify**, and the panel signs one throwaway message for real.

Verify is worth doing even though it's optional-looking, because it's the only
thing that checks the whole path at once: the credentials parse, the co-signer
is online, the policy rule exists and auto-approves, the vault resolves to an
address, and how long a signature actually takes. It signs a payload under its
own domain that authorises nothing: no gas, no chain, no order.

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
Communal Test Co-signer, so there's no co-signer to deploy.

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
unsafe, since every signature is verified before use, but whether it's fast
enough depends on your workspace, and that's yours to measure, not ours to
promise.

## The on-chain legs

Typed messages can't sign transactions. That rules out exactly two things: the
taker leg (`limit_taker_enabled`) and the closer, both of which call the reactor
on chain. Stitch rejects those at config time and says so.

Quoting and the ladder are unaffected. RFQ quotes and resting `book_enabled`
orders are signed Permit2 orders, not transactions, and cost no nonce.

**Stay typed-only (recommended to start).** Quote RFQ, rest the ladder, skip the
taker and closer. The one on-chain thing you still need is the Permit2
approval, a plain ERC-20 `approve` per input token once per chain, and the
panel can send that for you. See [Permit2 approvals without raw
signing](#permit2-approvals-without-raw-signing) below.

**Want the taker or the closer? Run them on a second bot with a hot wallet.** A
hot key that only ever holds what that bot trades is cheap to create, cheap to
rotate, and the blast radius if the host is compromised is whatever you funded it
with. Keep the Fireblocks bot doing what it's good at: quoting RFQ and resting
the ladder.

The alternative is Raw Signing, and we don't recommend asking for it. It turns
the vault key from "can sign the EIP-712 structures this bot produces" into "can
sign any 32 bytes", which is arbitrary transaction authority over the vault, the
exact containment this backend exists to give you. Approvals no longer need it at
all, so what's left behind the entitlement is the taker and the closer. If your
workspace already has it, `raw_signing = true` still works and those legs run
from here; you'll need a policy rule for raw signing scoped to your vault account
alongside the Typed Message one. That's a choice for someone who already made it,
not a step in this guide.

## Permit2 approvals without raw signing

`stitch approve` builds a transaction and signs its hash, which is exactly what
typed messages can't do. But `CONTRACT_CALL` is a different Fireblocks
operation from `RAW`: Fireblocks builds, nonces, signs and broadcasts the call
itself, and it needs **none of raw signing's entitlement**. The panel uses it,
so the approvals happen in the wizard instead of by hand.

What you need:

1. A **Contract Call** policy rule in the console, scoped to your operator vault
   account and the API user this key belongs to. This is an ordinary policy
   rule: nothing to buy and no CSM conversation.

   **Don't set this one to auto-approve**, unlike the Typed Message rule. A
   contract call is an arbitrary transaction, so an auto-approving rule lets
   anyone holding the bot's credentials move the vault however they like, which
   is the containment this whole backend exists to give you. Route it to a human
   approver or to an allowlist of the token contracts. Approvals are rare and
   you're at the panel when they happen, and the request waits 3 minutes for the
   call to mine, so there's time to approve it in the console.
2. A little native gas in the vault on the chain you're trading. The approve is
   ~46k gas, so this is cents on every chain except Ethereum.

Then either let the add-bot wizard do it, since it sends one approval per
token on its way to a live bot, or open the bot's **Tools** tab and press
**Approve** next to each token that reads "Not approved".

The panel picks the Fireblocks asset id for the chain by reading
`/v1/blockchains` and then `/v1/assets` from your workspace, so it files the
call under the right network without you configuring anything. A chain your
workspace doesn't have is reported as such rather than guessed at.

The panel always approves an unlimited amount. That's the right default for a
hot wallet and a decision for custodied inventory: a Permit2 order settles
against this allowance, not the vault balance, so it's the cap on what a stolen
signature can take. Bounding it is manual, from the console or the Token
Allowance Manager, and it needs fixed `buy_total_liquidity_debt` /
`sell_total_liquidity_collateral` first, because a side left on `"max"` only
accepts an effectively unlimited allowance and preflight refuses to start
against a smaller one.

The request stays open until the call is mined, which is normally a few
seconds. If it sits there, the usual cause is a policy rule routing to a human
approver rather than auto-approving, the same thing that makes Verify slow.

This covers approvals and nothing else. The taker and closer legs still build
and sign their own transactions on every fill, so they still need raw signing;
see [The on-chain legs](#the-on-chain-legs).

For the operator-facing version, including the threat model and the callback
handler that makes the rest of it mean anything, see [Running Stitch on
Fireblocks](https://docs.textilecredit.com/guides/fireblocks-custody).

## Troubleshooting

- **`Fireblocks refused the typed-message request: BLOCKED`**: no Typed Message
  policy rule, or one that doesn't cover this vault account and API user. Step 3.
- **`Fireblocks refused the raw request: BLOCKED`**: only reachable with
  `raw_signing = true`. The Typed Message rule doesn't cover raw signing; that
  needs its own policy rule scoped to the same vault account and API user.
- **Signing works but takes seconds**: the policy rule is routing to a human
  approver instead of the co-signer. Make it auto-approve.
- **`vault account N has no EVM wallet`**: the vault has no EVM asset wallet at
  all, so there is no address to read. Add one in the console (Ethereum, or a
  testnet asset such as ETH_TEST5 on a Sandbox). Step 2. The panel accepts any
  EVM asset, so you do not have to match whatever `asset_id` is set to.
- **`FAILED (ENV_UNSUPPORTED_ASSET)` when Verify signs**: the signing request
  was filed under an asset this workspace doesn't have. A Sandbox is
  testnet-only and has no mainnet `ETH`. Verify resolves the address from
  whichever EVM wallet the vault does hold and signs under that same asset, so
  this should not happen on a current build; if it does, check `asset_id` in
  `stitch.toml` isn't pinned to something the workspace can't use.
- **`Signer` isn't in the role dropdown when adding the API user**: the
  workspace has no API Co-Signer, so there is no key share for a Signer to hold.
  Sort the co-signer first; the role appears once one is paired. Don't reach for
  **Embedded Wallet Signer** instead. It signs for Embedded Wallets, not vault
  accounts. If you have a co-signer and the role still isn't offered, Fireblocks
  support can enable it.
- **`the Fireblocks API private key is not a usable RSA PEM`**: paste the whole
  `fireblocks_secret.key`, including the `BEGIN`/`END` lines. If you dropped a
  file and it was rejected, check you grabbed the key and not `fireblocks.csr`;
  they land in the same directory from the same command.
- **`api_base_url host ... is not an official fireblocks API host`**: if your
  workspace is in a region, point `api_base_url` at its endpoint
  (`https://eu-api.fireblocks.io` and friends are accepted).
- **`remote signature did not recover to operator address`**: `operator_address`
  doesn't match the vault. Re-run Verify and let it fill the field in.
- **`this signer signs EIP-712 typed messages only`**: the config turns on the
  taker or closer leg. See [The on-chain legs](#the-on-chain-legs).
- **`Fireblocks refused the contract-call request: BLOCKED`**: no Contract Call
  policy rule, or one that doesn't cover this vault account and API user. Note
  this is *not* the raw-signing entitlement: a contract call needs a rule, not a
  purchase. See [Permit2 approvals without raw
  signing](#permit2-approvals-without-raw-signing).
- **`this Fireblocks workspace lists no EVM blockchain with chain id N`**: the
  chain isn't enabled on the workspace, or you're on a Sandbox (testnet-only)
  and the corridor is on a mainnet.
- **Quotes rejected as `Busy` with `signing did not finish inside the reply
  budget` in the log**: signing is slower than the venue's window. Same cause as
  a slow Verify: a policy rule that isn't auto-approving.
