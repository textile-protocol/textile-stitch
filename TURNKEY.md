# Turnkey Signer Setup

Turnkey is one of Stitch's MPC signer backends. The operator key lives in
Turnkey's TEE custody instead of a local `stitch.key`, and every signature the
bot makes (EIP-712 limit orders and EIP-1559 fill/approve transactions) goes
through one synchronous Turnkey API call. There is no sidecar and no inbound
server: the call is made from inside the bot binary, authenticated by stamping
each request with your Turnkey API key.

Turnkey signs the raw 32-byte digest the bot computes (`HashFunction::NoOp`).
Signing is chain-agnostic, so the same setup works for every corridor the bot
runs (Celo, Base, Arbitrum, Ethereum). The bot builds the per-chain digest and
broadcasts the transaction itself; Turnkey only produces the signature.

This is the full walkthrough. The one-screen config reference lives in
[ADVANCED.md](ADVANCED.md#mpc-wallet-signers).

## What you need from Turnkey

Four values go into the bot's config, plus one secret in the environment:

| Config field | Where it comes from |
|---|---|
| `organization_id` | Turnkey org id (user dropdown, top-right of the dashboard) |
| `sign_with` | the wallet account address (or a private-key id) |
| `operator_address` | the EVM `0x` address that key resolves to |
| `TURNKEY_API_PUBLIC_KEY` (env, not secret) | the API key's public half |
| `TURNKEY_API_PRIVATE_KEY[_FILE]` (env, secret) | the API key's private half |

For a standard wallet account, `sign_with` and `operator_address` are the **same
`0x` address**. They only differ if you sign with a raw Turnkey private key,
where `sign_with` is the private-key id (a UUID) and `operator_address` is the
address it controls.

## 1. Create the organization

Sign in at [app.turnkey.com](https://app.turnkey.com). Your **Organization ID**
is in the user dropdown at the top-right. Copy it; that's `organization_id`.

## 2. Create the wallet and an Ethereum account

Create a wallet (the standard **Company Wallet** is what you want for an operator
bot), then add an account.

When picking the account type you'll see a list of address formats (Ethereum,
Bitcoin, Solana, ...). There is **no BSC or Celo entry, and you don't need one.**
That list picks the address format, not the network. Choose **Ethereum**: the
resulting secp256k1 address is valid across all EVM chains and L2s (Celo, Base,
Arbitrum, and the rest), so one account signs for every corridor. Turnkey's own
docs: "This address format is valid across all EVM chains and L2s."

Copy the account's `0x` address. That single value is both `sign_with` and
`operator_address`.

## 3. Create an API key

Under your user, create an **API key**. Turnkey generates a P-256 key pair:

- The **public key** is not secret; it becomes `TURNKEY_API_PUBLIC_KEY`.
- The **private key** is secret and is shown once. Copy it immediately; it
  becomes `TURNKEY_API_PRIVATE_KEY` (or a file the `_FILE` var points at).

The bot stamps every request with this key. Nothing else authenticates the call.

## 4. Let the API key sign

The API key's user needs permission to run the `sign_raw_payload` activity
(`ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2`, exactly what the bot calls).

**Simplest:** put the API key on a **root user**. Root users bypass all
policies, so signing just works with no policy to write. Fine for a
single-operator bot.

**Least privilege (recommended for production):** make the API key a non-root
user that can *only* sign for the operator key, and grant it with one policy. Get
the user's id from the dashboard (Users, click the user, copy the User ID), then
add:

```json
{
  "policyName": "Allow Stitch operator to sign raw payloads for the operator key",
  "effect": "EFFECT_ALLOW",
  "consensus": "approvers.any(user, user.id == '<API_KEY_USER_ID>')",
  "condition": "activity.type == 'ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2' && wallet_account.address == '<OPERATOR_ADDRESS>'"
}
```

`consensus` is who may act (your API-key user); `condition` scopes it to raw-
payload signing **for the operator wallet account only.** Don't drop the
`wallet_account.address` clause: if your org holds other Turnkey wallets, a policy
that checks only `activity.type` lets this API user sign arbitrary payloads for
any of them, not just the operator key. Set `<OPERATOR_ADDRESS>` to the same `0x`
address you configured as `operator_address`.

If you sign with a raw Turnkey **private key** instead of a wallet account (the
`sign_with` = private-key id case), target it with `private_key.id ==
'<PRIVATE_KEY_ID>'` instead of `wallet_account.address`. Use one or the other, not
both: Turnkey evaluates the whole condition (no short-circuit), so referencing the
resource keyword that doesn't match the activity's target errors the policy.

Add it on the dashboard Policies page or with `turnkey policies create`.

## 5. Configure the bot

**Desktop app.** In `stitch-setup`, pick **Signer -> Turnkey** and fill
Organization ID, Sign with, Operator address, API public key, and API private
key. Saving writes the `[signer]` section, stores the private key in an
owner-only `turnkey-api.key`, and points `stitch.env` at it. The dropdown no
longer shows an "Experimental" marker for Turnkey.

**Manual.** Edit `stitch.toml` directly:

```toml
[signer]
provider         = "turnkey"
organization_id  = "<turnkey org id>"
sign_with        = "0x<wallet account address or private key id>"
operator_address = "0x<the EVM address sign_with resolves to>"
api_base_url     = "https://api.turnkey.com"   # optional, this is the default
max_concurrent_signs = 8                        # optional
```

Then set the env (secrets never go in the config file):

```
TURNKEY_API_PUBLIC_KEY=<public key>
TURNKEY_API_PRIVATE_KEY_FILE=/path/to/turnkey-api.key   # or TURNKEY_API_PRIVATE_KEY=<hex>
```

## 6. Validate with a dry run

You don't need gas or approvals to check signing. A dry run exercises the full
Turnkey round trip without posting orders or broadcasting:

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

A healthy run logs the maker address (it must equal your `operator_address`),
does one round trip to `api.turnkey.com` per signature, and returns a signature.
The bot verifies every signature recovers to `operator_address` before using it.

## Troubleshooting

- **`remote signature did not recover to operator address`** — `sign_with` points
  at a different key than `operator_address`. For a wallet account both are the
  same `0x` address; fix the config so they match.
- **Turnkey permission / denied error on the first sign** — the API-key user has
  no permission to sign. Either it isn't a root user and the step-4 policy is
  missing, or the `user.id` in the policy's `consensus` is wrong.
- **`invalid Turnkey API key pair`** — the public/private values don't parse or
  don't match. Re-copy both halves from the API key you created.

Once the dry run signs and recovers to your operator address, Turnkey is working
end to end. For a live run, fund the operator wallet with a little native gas and
run `stitch approve` before starting without `--dry-run`.
