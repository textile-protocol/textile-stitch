# AWS KMS Signer Setup

This backend keeps the operator key in AWS KMS, which generates and uses it
inside its own FIPS 140-validated HSMs. The private key never leaves them. Every
signature the bot makes (RFQ quotes, NAV co-signatures, the venue session and
enrolment, and EIP-1559 transactions) is one `Sign` call. There is no sidecar.
The bot computes the 32-byte digest itself and sends it with `MessageType=DIGEST`,
so KMS signs it without hashing it again. It then checks that the signature
recovers to `operator_address` before using it.

It signs raw digests, so the on-chain legs (fills, closer, `stitch approve`,
withdraw) work too.

With an OperatorVault, this key is the vault's **strategy signer**. The vault
checks that role with plain ECDSA, so the strategy signer has to be a raw key
like this one, not a Safe. The vault owner can (and should) still be a Safe.

A dedicated AWS CloudHSM cluster is a different product. KMS custom key stores
backed by CloudHSM only hold symmetric keys, so a secp256k1 key in your own
cluster would need PKCS#11, which this backend doesn't speak.

The one-screen config reference lives in
[ADVANCED.md](ADVANCED.md#mpc-wallet-signers).

## What you need

| Config / env | Where it comes from |
|---|---|
| `key_id` | the key's ARN, see step 1 |
| `operator_address` | the EVM address the key controls. The panel's Verify fills it in |
| `AWS_KMS_CREDENTIALS_FILE` (env) | an AWS credentials file. Not needed when the bot runs on AWS with a role |

## 1. Create the key

Create it in the region the bot will run in, since every quote waits on a round
trip to it.

```bash
aws kms create-key --region us-east-1 \
  --key-spec ECC_SECG_P256K1 --key-usage SIGN_VERIFY \
  --description "Stitch operator key"

aws kms create-alias --region us-east-1 \
  --alias-name alias/stitch-operator --target-key-id <KeyId from above>
```

`KeyMetadata.Arn` in the first command's output is `key_id`:

```
arn:aws:kms:us-east-1:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab
```

The bot refuses a key that isn't `ECC_SECG_P256K1` with `SIGN_VERIFY` usage: a
P-256 key signs fine, but every signature would recover to a random address.

## 2. Give the bot an identity

The bot needs two permissions on this one key:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": ["kms:Sign", "kms:GetPublicKey"],
    "Resource": "arn:aws:kms:us-east-1:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab"
  }]
}
```

`kms:Sign` is for signing, and `kms:GetPublicKey` is for the startup check that
the key controls `operator_address`. The default key policy lets IAM policies
like this one grant access. If you replaced it, the key policy has to allow these
two actions itself.

Where that policy goes depends on where the bot runs:

- **On AWS, running the bot yourself** (CLI, your own container on EC2 or ECS,
  or EKS): attach it to the instance role, task role or service account role and
  set nothing else. The SDK picks the role up. For a container on EC2 using
  IMDSv2, the instance needs a metadata hop limit of 2.
- **Anywhere else, and any bot the panel creates**: the panel always stores an
  access key for the bot. Create an IAM user with only this policy, then run
  `aws iam create-access-key --user-name <user>`. Anyone holding that key can sign
  as your operator until you deactivate it, so store it like the hot-wallet key
  it replaces.

## 3. Configure the bot

**Desktop / panel.** In **Add bot**, at "Set up the operator wallet", pick
**Signer → HSM — AWS KMS** and fill in the key ARN, the access key ID and the
secret access key. Click **Verify**. The panel reads the key's public key, signs
a throwaway message under its own EIP-712 domain (it authorises nothing), and
fills in the operator address that signature recovered to. It also shows how
long one signature took. Saving stores the access key in an owner-only
`awskms-credentials.ini` and points `stitch.env` at it.

**Manual.** Edit `stitch.toml`:

```toml
[signer]
provider         = "awskms"
key_id           = "arn:aws:kms:us-east-1:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab"
operator_address = "0x<the address the key controls>"
# region         = "us-east-1"                                  # only if key_id isn't an ARN
# api_base_url   = "https://kms-fips.us-east-1.amazonaws.com"   # optional FIPS or VPC endpoint
max_concurrent_signs = 8                                         # optional
```

The bot looks for credentials in this order and uses the first it finds:

1. `AWS_KMS_CREDENTIALS_FILE`, or the same file's contents in
   `AWS_KMS_CREDENTIALS`. This is a standard AWS credentials file, read from its
   `[default]` profile and nothing else.
2. Otherwise, the SDK's default chain: `AWS_ACCESS_KEY_ID` /
   `AWS_SECRET_ACCESS_KEY`, `~/.aws` profiles (`AWS_PROFILE`), web identity
   (EKS), the ECS task role, and the EC2 instance role.

In Docker, an inline `AWS_KMS_CREDENTIALS` is written to an owner-only file by
the entrypoint and removed from the environment before the bot starts.

The bot always sets the KMS endpoint itself: `https://kms.<region>.amazonaws.com`,
or `api_base_url` if you give one. So an `AWS_ENDPOINT_URL` left in the
environment can't send signing requests somewhere else.

AWS shows a public key, not an Ethereum address. Without the panel, the quickest
way to learn the address is to start the bot with
`operator_address = "0x0000000000000000000000000000000000000000"`: it refuses to
start, and the error names the address the key actually controls.

## 4. Validate with a dry run

```bash
RUST_LOG=info,stitch=debug,stitch_bot=debug \
  stitch --config ~/Stitch/stitch.toml --dry-run
```

A healthy start logs `AWS KMS signer ready` with the key, region, where the
credentials came from, and the address. Signing then costs one round trip to
KMS per signature.

## Latency and quota

Verify measures one signature, and the panel warns above 400 ms. Run the bot in
the key's region. The venue's reply budget is set per corridor, and the bot gives
up on a signature that would miss it.

Sign requests on ECC keys share one quota per account and region, 1,000 a second
by default, and other workloads in the account count against it too. When it
runs out, KMS answers `ThrottlingException`. The SDK retries twice with a short
backoff, and if the reply budget runs out first, that quote is skipped. The quota
can be raised through Service Quotas.

## Rotating the key

AWS KMS doesn't rotate asymmetric keys, so rotation means a new key and a new
address. Create it, run Verify (or the dry run) to learn the address, then:

1. If the bot trades through an OperatorVault, have the vault owner call
   `setStrategySigner(newAddress)`. That also invalidates every outstanding order.
2. Update `key_id` and `operator_address`.
3. Run `stitch connect` (or Settings → RFQ in the panel) so the venue registers
   the new signing address.
4. Disable the old key once nothing signs with it.

## Troubleshooting

- **`is ECC_NIST_P256, but an Ethereum key has to be ECC_SECG_P256K1`**: the key
  was created with the wrong spec. Key specs can't be changed, so create a new
  key as in step 1.
- **`controls 0x…, but [signer].operator_address is 0x…`**: `key_id` points at a
  different key than `operator_address`. Fix whichever is wrong.
- **`AccessDeniedException`**: the identity lacks `kms:Sign` or
  `kms:GetPublicKey` on the key, or the key policy doesn't let IAM grant them.
- **`UnrecognizedClientException` / `InvalidSignatureException`**: the access
  key is wrong, inactive, or doesn't match the secret.
- **`NotFoundException`**: the ARN is mistyped, or `region` disagrees with the
  key's own region.
- **`no credentials found in chain`**: the bot is off AWS and found no
  credentials. Set `AWS_KMS_CREDENTIALS_FILE`.
