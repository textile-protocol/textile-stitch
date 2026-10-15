# Google Cloud KMS Signer Setup

This backend keeps the operator key in Google Cloud KMS, normally at the `HSM`
protection level, so the private key lives in Cloud HSM and never leaves it.
Every signature the bot makes (RFQ quotes, NAV co-signatures, the venue session
and enrolment, and EIP-1559 transactions) is one synchronous `asymmetricSign`
call. There is no sidecar. The bot computes the 32-byte digest itself, sends it,
and gets back a DER signature. It then checks that the signature recovers to
`operator_address` before using it.

It signs raw digests, so unlike Fireblocks without raw signing, the on-chain legs
(fills, closer, `stitch approve`, withdraw) work too.

With an OperatorVault, this key is the vault's **strategy signer**. The vault
checks that role with plain ECDSA, so the strategy signer has to be a raw key
like this one, not a Safe. The vault owner can (and should) still be a Safe.

The one-screen config reference lives in
[ADVANCED.md](ADVANCED.md#mpc-wallet-signers).

## What you need

| Config / env | Where it comes from |
|---|---|
| `key_version` | the full key version resource name, see step 1 |
| `operator_address` | the EVM address the key controls. The panel's Verify fills it in |
| `GCP_KMS_CREDENTIALS_FILE` (env) | a service-account JSON key. Not needed when the bot runs on GCP |

## 1. Create the key

Cloud KMS offers secp256k1 at the HSM protection level. Put the key ring in the
region the bot will run in, since every quote waits on a round trip to it.

```bash
gcloud services enable cloudkms.googleapis.com

gcloud kms keyrings create stitch --location us-east1

gcloud kms keys create operator \
  --keyring stitch --location us-east1 \
  --purpose asymmetric-signing \
  --default-algorithm ec-sign-secp256k1-sha256 \
  --protection-level hsm
```

The key version the bot signs with is:

```
projects/PROJECT_ID/locations/us-east1/keyRings/stitch/cryptoKeys/operator/cryptoKeyVersions/1
```

That full string, version included, is `key_version`. The bot refuses a key that
isn't `EC_SIGN_SECP256K1_SHA256`: a P-256 key signs fine, but every signature
would recover to a random address.

## 2. Give the bot an identity

Create a service account and let it sign with this one key. Grant the role on
the key, not the project, so the account can't touch any other key.

```bash
gcloud iam service-accounts create stitch-signer

gcloud kms keys add-iam-policy-binding operator \
  --keyring stitch --location us-east1 \
  --member serviceAccount:stitch-signer@PROJECT_ID.iam.gserviceaccount.com \
  --role roles/cloudkms.signerVerifier
```

`signerVerifier` covers both permissions the bot uses: `useToSign` for signing,
and `viewPublicKey` for the startup check that the key controls
`operator_address`.

How the bot authenticates depends on where it runs:

- **On GCP, running the bot yourself** (CLI, your own container or Kubernetes
  deployment on Compute Engine, GKE or Cloud Run): attach `stitch-signer` to the
  VM, pod or service and set nothing else. The bot gets tokens from the metadata
  server, and there is no key file to leak.
- **Anywhere else, and any bot the panel creates**: the panel always stores a
  JSON key for the bot, even on GCP. Create one with
  `gcloud iam service-accounts keys create stitch-signer.json --iam-account stitch-signer@PROJECT_ID.iam.gserviceaccount.com`.
  Anyone holding that file can sign as your operator until you delete the key,
  so store it like the hot-wallet key it replaces.

Workload Identity Federation (`external_account`) and impersonation configs are
not supported yet. The bot says so rather than failing later.

## 3. Configure the bot

**Desktop / panel.** In **Add bot**, at "Set up the operator wallet", pick
**Signer → HSM — Google Cloud KMS**, paste the key version, and load the JSON key.
Click **Verify**. The panel reads the key's public key, signs a throwaway message
under its own EIP-712 domain (it authorises nothing), and fills in the operator
address that signature recovered to. It also shows the key's protection level and
how long one signature took. Saving stores the JSON key in an owner-only
`gcpkms-credentials.json` and points `stitch.env` at it.

**Manual.** Edit `stitch.toml`:

```toml
[signer]
provider         = "gcpkms"
key_version      = "projects/PROJECT_ID/locations/us-east1/keyRings/stitch/cryptoKeys/operator/cryptoKeyVersions/1"
operator_address = "0x<the address the key controls>"
api_base_url     = "https://cloudkms.googleapis.com"   # optional default; or https://us-east1-cloudkms.googleapis.com
max_concurrent_signs = 8                                # optional
```

A regional endpoint (`https://<location>-cloudkms.googleapis.com`) only serves
keys in its own location, so it has to match the location in `key_version`.

Off GCP, point the bot at the JSON key:

```
GCP_KMS_CREDENTIALS_FILE=/path/to/stitch-signer.json   # or GCP_KMS_CREDENTIALS=<the JSON itself>
```

The bot looks for credentials in this order and uses the first it finds:
`GCP_KMS_CREDENTIALS_FILE`, `GCP_KMS_CREDENTIALS`, `GOOGLE_APPLICATION_CREDENTIALS`,
gcloud's application-default file (`gcloud auth application-default login`, handy
for a test on a laptop), then the metadata server. In Docker, an inline
`GCP_KMS_CREDENTIALS` is written to an owner-only file by the entrypoint and
removed from the environment before the bot starts.

Cloud KMS shows a PEM, not an Ethereum address. Without the panel, the quickest
way to learn the address is to start the bot with
`operator_address = "0x0000000000000000000000000000000000000000"`: it refuses to
start, and the error names the address the key version actually controls.

## 4. Validate with a dry run

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

A healthy start logs `Cloud KMS signer ready` with the key version, its
protection level, the identity it signs as, and the address. Signing then costs
one round trip to Cloud KMS per signature.

## Latency and quota

Google doesn't publish Cloud HSM signing latency, which is why Verify measures it.
Run the bot in the key's region, and use the regional endpoint if the global one
measures slow. The panel warns above 400 ms. The access token is fetched at
startup and replaced in the background five minutes before it expires, so a
signature doesn't wait on Google's token endpoint. The venue's reply budget is
set per corridor, and the bot gives up on a signature that would miss it.

Cloud HSM signing is metered per project and region, and the limit is enforced
per second. At the time of writing a secp256k1 sign costs 4,500 units out of
3,000,000 a minute, roughly 11 signatures a second. Check
[Google's quota page](https://cloud.google.com/kms/quotas) for the current
numbers. When the quota runs out Cloud KMS answers 429. The bot retries twice
with a short backoff, and if the reply budget runs out first, that quote is
skipped. Ask Google for more quota if you quote at volume.

## Rotating the key

A new key version is a new address. Create it, run Verify (or the dry run) to
learn the address, then:

1. If the bot trades through an OperatorVault, have the vault owner call
   `setStrategySigner(newAddress)`. That also invalidates every outstanding order.
2. Update `key_version` and `operator_address`.
3. Run `stitch connect` (or Settings → RFQ in the panel) so the venue registers
   the new signing address.
4. Disable the old key version once nothing signs with it.

## Troubleshooting

- **`uses EC_SIGN_P256_SHA256, but an Ethereum key has to be EC_SIGN_SECP256K1_SHA256`**:
  the key was created with the wrong algorithm. Algorithms can't be changed, so
  create a new key as in step 1.
- **`controls 0x…, but [signer].operator_address is 0x…`**: `key_version` points
  at a different key than `operator_address`. Fix whichever is wrong.
- **403 from Cloud KMS**: the identity lacks `roles/cloudkms.signerVerifier` on
  the key, or the Cloud KMS API isn't enabled in the project. Google's message,
  quoted in the error, says which.
- **404 from Cloud KMS**: `key_version` is misspelled, missing the version, or
  names the wrong location.
- **`could not reach Google for an access token … metadata server`**: the bot is
  off GCP and found no credentials. Set `GCP_KMS_CREDENTIALS_FILE`.
- **`not a Google token endpoint`**: the JSON key's `token_uri` was edited. Use
  the file as Google issued it.
