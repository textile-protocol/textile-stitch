#!/usr/bin/env bash
# Sets up an AWS KMS operator key for one Stitch bot: the key, an IAM user that
# can only sign with it, and that user's access key. Those are the three values
# Add bot → Signer → HSM — AWS KMS asks for. Same steps as the panel's guide and
# docs/signer-awskms.md, without the console.
#
#   scripts/aws-kms-signer.sh --region eu-west-1 my-bot
#
# Run it with your own AWS credentials (a profile, an SSO session or env vars)
# that can create KMS keys and IAM users. The secret access key is never printed,
# and only written to disk if you pass --save: it goes onto the clipboard for you
# to paste, and the clipboard is cleared afterwards.
#
# The key's own policy lets only the bot's IAM user sign with it. The account
# keeps every other permission on the key, so admins can still manage, rotate
# the policy of, or delete it, but an IAM grant of kms:Sign elsewhere in the
# account doesn't reach it.
#
# If any step fails before you have the secret, everything this run created is
# removed again (the key is scheduled for deletion after 7 days, the shortest
# window KMS allows).
#
# Written for the bash 3.2 macOS ships with. Needs the AWS CLI (v2).
set -euo pipefail

PREFIX=stitch-operator
POLICY_NAME=stitch-kms-sign
TAG_KEY=stitch-bot

usage() {
  cat <<EOF
Usage: $(basename "$0") --region <region> [options] <bot-name>

Creates, in <region>:
  KMS key     ECC_SECG_P256K1, sign and verify, alias alias/$PREFIX-<bot-name>.
              Its key policy lets only the IAM user below sign with it
  IAM user    $PREFIX-<bot-name>, no console access, one inline policy
              ($POLICY_NAME) allowing kms:Sign and kms:GetPublicKey on that key only
  Access key  for that user

Then copies the key ARN, access key ID and secret to the clipboard one at a
time, for you to paste into Stitch Fleet.

Options:
  --region <region>  Required. The region the bot will run in. Every quote waits
                     on a round trip to it.
  --profile <name>   AWS CLI profile to create everything with. Defaults to your
                     usual credentials.
  --save <file>      Also write the access key to <file> (owner-only), as an AWS
                     credentials file the bot reads through AWS_KMS_CREDENTIALS_FILE.
                     Only for bots you run by hand: the panel stores its own copy.
                     Required where there's no clipboard (pbcopy).
  --yes              Don't ask before creating anything.
  -h, --help         Show this.
EOF
}

say() { printf '%s\n' "$*"; }
warn() { printf '! %s\n' "$*" >&2; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}
step() { printf '\n==> %s\n' "$*"; }

# ---------------------------------------------------------------- arguments

REGION=
NAME=
SAVE=
YES=
while (($#)); do
  case $1 in
    --region) REGION=${2:-} && shift ;;
    --region=*) REGION=${1#*=} ;;
    --profile) export AWS_PROFILE=${2:-} && shift ;;
    --profile=*) export AWS_PROFILE=${1#*=} ;;
    --save) SAVE=${2:-} && shift ;;
    --save=*) SAVE=${1#*=} ;;
    --yes | -y) YES=1 ;;
    -h | --help) usage && exit 0 ;;
    -*) die "unknown option $1 (see --help)" ;;
    *)
      [[ -z $NAME ]] || die "one bot name at a time (got '$NAME' and '$1')"
      NAME=$1
      ;;
  esac
  shift
done

[[ -n $NAME ]] || {
  usage >&2
  exit 2
}
[[ -n $REGION ]] || die "--region is required: create the key where the bot runs"
[[ $REGION =~ ^[a-z]{2}(-[a-z]+)+-[0-9]+$ ]] || die "'$REGION' doesn't look like an AWS region (e.g. eu-west-1)"
# IAM user names stop at 64 characters, and the prefix takes 16 of them.
[[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,47}$ ]] ||
  die "bot name: letters, digits, - and _ only, 48 characters at most"

USER_NAME=$PREFIX-$NAME
ALIAS=alias/$PREFIX-$NAME

if [[ -n $SAVE ]]; then
  [[ ! -e $SAVE ]] || die "$SAVE already exists, and this won't overwrite it"
  [[ -d $(dirname "$SAVE") ]] || die "the directory for $SAVE doesn't exist"
fi

# Nothing below should end up in a pager waiting for a keypress.
export AWS_PAGER=""
command -v aws >/dev/null 2>&1 ||
  die "the AWS CLI isn't installed. On a Mac: brew install awscli (then aws configure or aws sso login)"

# AWS shows the secret once, and it's never printed to the terminal. So decide
# up front how it gets handed over: the clipboard, in a terminal, or a file.
CLIPBOARD=
command -v pbcopy >/dev/null 2>&1 && [[ -t 0 ]] && CLIPBOARD=1
if [[ -z $CLIPBOARD && -z $SAVE ]]; then
  die "no clipboard to hand the secret over with (needs pbcopy and a terminal): pass --save <file>"
fi
[[ -t 0 || -n $YES ]] || die "not running in a terminal: pass --yes to create without asking"


# ---------------------------------------------------------------- rollback

KEY_ARN=
ALIAS_MADE=
USER_MADE=
POLICY_MADE=
FINISHED=
SECRET_ON_CLIPBOARD=
SECRET=
ACCESS_KEY_ID=
TMP=

# Undoes whatever this run created if it stops before the secret has been handed
# over (saved, or put on the clipboard). A half-built signer is only clutter, and
# an access key whose secret nobody has is a live credential nobody can use or
# will remember to remove.
rollback() {
  local status=$?
  set +e
  [[ -n $TMP ]] && rm -f "$TMP"
  [[ -n $SECRET_ON_CLIPBOARD ]] && printf '' | pbcopy
  SECRET=
  if [[ -z $FINISHED && -n $KEY_ARN$USER_MADE ]]; then
    warn "stopped before finishing. Removing what this run created..."
    if [[ -n $USER_MADE ]]; then
      # The user is new, so every access key on it is one this run made, even
      # one whose ID never got read back.
      local id
      for id in $(aws iam list-access-keys --user-name "$USER_NAME" \
        --query 'AccessKeyMetadata[].AccessKeyId' --output text 2>/dev/null); do
        aws iam delete-access-key --user-name "$USER_NAME" --access-key-id "$id" >/dev/null &&
          warn "  deleted access key $id"
      done
      [[ -n $POLICY_MADE ]] &&
        aws iam delete-user-policy --user-name "$USER_NAME" --policy-name "$POLICY_NAME" >/dev/null
      aws iam delete-user --user-name "$USER_NAME" >/dev/null &&
        warn "  deleted IAM user $USER_NAME" ||
        warn "  couldn't delete IAM user $USER_NAME. Delete it in the IAM console"
    fi
    if [[ -n $ALIAS_MADE ]]; then
      aws kms delete-alias --region "$REGION" --alias-name "$ALIAS" >/dev/null
    fi
    if [[ -n $KEY_ARN ]]; then
      aws kms schedule-key-deletion --region "$REGION" --key-id "$KEY_ARN" \
        --pending-window-in-days 7 >/dev/null &&
        warn "  scheduled $KEY_ARN for deletion in 7 days" ||
        warn "  couldn't schedule $KEY_ARN for deletion. Do it in the KMS console"
    fi
  fi
  exit "$status"
}
trap rollback EXIT
trap 'exit 130' INT TERM

# ---------------------------------------------------------------- helpers

confirm() {
  [[ -n $YES ]] && return 0
  local answer
  read -r -p "$1 [y/N] " answer
  [[ $answer == [yY] || $answer == [yY][eE][sS] ]]
}

# python3 on a Mac without the command line tools is a stub that pops an
# installer dialog, so only use it when it's real.
have_python() {
  command -v python3 >/dev/null 2>&1 || return 1
  [[ $(uname) != Darwin || $(command -v python3) != /usr/bin/python3 ]] && return 0
  xcode-select -p >/dev/null 2>&1
}

# The EVM address a secp256k1 public key controls, from the base64 DER that
# kms:GetPublicKey returns: the last 20 bytes of keccak256(X || Y), EIP-55
# checksummed. Keccak is written out because nothing on a stock Mac has it
# (hashlib's sha3_256 is the NIST variant, which pads differently).
address_from_public_key() {
  python3 -I - "$1" <<'PY'
import base64, sys

def keccak256(data):
    rc = [0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
          0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
          0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
          0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
          0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
          0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008]
    rot = [[0, 36, 3, 41, 18], [1, 44, 10, 45, 2], [62, 6, 43, 15, 61],
           [28, 55, 25, 21, 56], [27, 20, 39, 8, 14]]
    mask = (1 << 64) - 1
    rol = lambda v, n: ((v << n) | (v >> (64 - n))) & mask if n else v
    rate = 136
    msg = bytearray(data) + b"\x01"
    msg += b"\x00" * (-len(msg) % rate)
    msg[-1] |= 0x80
    a = [[0] * 5 for _ in range(5)]
    for off in range(0, len(msg), rate):
        for i in range(rate // 8):
            a[i % 5][i // 5] ^= int.from_bytes(msg[off + 8 * i:off + 8 * i + 8], "little")
        for c_round in rc:
            c = [a[x][0] ^ a[x][1] ^ a[x][2] ^ a[x][3] ^ a[x][4] for x in range(5)]
            d = [c[(x - 1) % 5] ^ rol(c[(x + 1) % 5], 1) for x in range(5)]
            a = [[a[x][y] ^ d[x] for y in range(5)] for x in range(5)]
            b = [[0] * 5 for _ in range(5)]
            for x in range(5):
                for y in range(5):
                    b[y][(2 * x + 3 * y) % 5] = rol(a[x][y], rot[x][y])
            a = [[b[x][y] ^ (~b[(x + 1) % 5][y] & b[(x + 2) % 5][y]) for y in range(5)]
                 for x in range(5)]
            a[0][0] ^= c_round
    return b"".join(a[i % 5][i // 5].to_bytes(8, "little") for i in range(4))

der = base64.b64decode(sys.argv[1])
# A secp256k1 SubjectPublicKeyInfo ends in the 65-byte uncompressed point.
if len(der) != 88 or der[-65] != 0x04:
    sys.exit("not an uncompressed secp256k1 public key")
addr = keccak256(der[-64:])[-20:].hex()
check = keccak256(addr.encode()).hex()
print("0x" + "".join(ch.upper() if int(check[i], 16) >= 8 else ch for i, ch in enumerate(addr)))
PY
}

# Whether the new access key can sign with the key. Runs in a subshell with only
# that key in the environment, so nothing from your own credentials leaks in.
# export is a builtin, so the secret never shows up in a process listing.
# Prints AWS's error, if any.
new_key_signs() {
  (
    unset AWS_PROFILE AWS_DEFAULT_PROFILE AWS_SESSION_TOKEN AWS_SECURITY_TOKEN \
      AWS_CREDENTIAL_EXPIRATION AWS_ROLE_ARN AWS_WEB_IDENTITY_TOKEN_FILE
    export AWS_ACCESS_KEY_ID=$ACCESS_KEY_ID
    export AWS_SECRET_ACCESS_KEY=$SECRET
    aws kms sign --region "$REGION" --key-id "$KEY_ARN" \
      --message-type RAW --signing-algorithm ECDSA_SHA_256 \
      --message "fileb://$TMP" --query KeyId --output text >/dev/null
  ) 2>&1
}

copy() { printf '%s' "$1" | pbcopy; }

# The key policy. Only the bot's user may sign. The account (and so IAM) keeps
# every other action, for administration and recovery, but not kms:Sign, nor
# kms:CreateGrant, which could hand Sign to someone else. Without this, the
# default key policy lets any IAM policy in the account grant kms:Sign on the key.
key_policy() {
  local partition=${USER_ARN#arn:}
  partition=${partition%%:*}
  printf '{"Version":"2012-10-17","Statement":[%s,%s]}' \
    "{\"Sid\":\"AccountAdministersButCannotSign\",\"Effect\":\"Allow\",\"Principal\":{\"AWS\":\"arn:$partition:iam::$ACCOUNT:root\"},\"NotAction\":[\"kms:Sign\",\"kms:CreateGrant\"],\"Resource\":\"*\"}" \
    "{\"Sid\":\"OnlyTheBotSigns\",\"Effect\":\"Allow\",\"Principal\":{\"AWS\":\"$USER_ARN\"},\"Action\":[\"kms:Sign\",\"kms:GetPublicKey\"],\"Resource\":\"*\"}"
}

# Creates the key and prints its ARN. A brand-new IAM user can take a few
# seconds before KMS accepts it as a principal, and until then create-key
# answers "invalid principals" without creating anything, so that is retried.
# Any other refusal is final.
create_key() {
  local out
  for _ in $(seq 1 20); do
    if out=$(aws kms create-key --region "$REGION" \
      --key-spec ECC_SECG_P256K1 --key-usage SIGN_VERIFY \
      --description "Stitch operator key ($NAME)" \
      --tags "TagKey=$TAG_KEY,TagValue=$NAME" \
      --policy "$(key_policy)" \
      --query KeyMetadata.Arn --output text 2>&1); then
      printf '%s\n' "$out"
      return 0
    fi
    [[ $out == *"invalid principals"* ]] || break
    sleep 3
  done
  printf '%s\n' "$out" >&2
  return 1
}

# ---------------------------------------------------------------- preflight

step "Checking your AWS credentials"
IDENTITY=$(aws sts get-caller-identity --region "$REGION" --query '[Account,Arn]' --output text 2>&1) ||
  die "AWS CLI can't authenticate: $IDENTITY
Run aws configure, or aws sso login${AWS_PROFILE:+ --profile $AWS_PROFILE}, and try again."
ACCOUNT=${IDENTITY%%$'\t'*}
CALLER=${IDENTITY#*$'\t'}
say "Account $ACCOUNT, as $CALLER"

existing=$(aws kms list-aliases --region "$REGION" \
  --query "Aliases[?AliasName=='$ALIAS'].AliasName" --output text 2>&1) ||
  die "can't list KMS keys in $REGION: $existing"
[[ -z $existing || $existing == None ]] ||
  die "$ALIAS already exists in $REGION. Pick another bot name, or delete the old alias first"

if out=$(aws iam get-user --user-name "$USER_NAME" 2>&1); then
  die "IAM user $USER_NAME already exists. Pick another bot name, or delete the old user first"
elif [[ $out != *NoSuchEntity* ]]; then
  die "can't check for IAM user $USER_NAME: $out"
fi

cat <<EOF

About to create, in account $ACCOUNT:
  IAM user    $USER_NAME, allowed kms:Sign and kms:GetPublicKey on the key only
  KMS key     ECC_SECG_P256K1 in $REGION, alias $ALIAS, that only $USER_NAME can sign with
  Access key  for $USER_NAME
EOF
confirm "Go ahead?" || {
  say "Nothing created."
  exit 0
}

# ---------------------------------------------------------------- create

# The user comes first: the key policy names it.
step "Creating IAM user $USER_NAME"
USER_ARN=$(aws iam create-user --user-name "$USER_NAME" --tags "Key=$TAG_KEY,Value=$NAME" \
  --query User.Arn --output text)
USER_MADE=1
[[ $USER_ARN == arn:aws*:iam::$ACCOUNT:user/$USER_NAME ]] || die "unexpected user ARN from create-user: $USER_ARN"

step "Creating the KMS key"
KEY_ARN=$(create_key) || die "couldn't create the KMS key"
[[ $KEY_ARN == arn:aws*:kms:$REGION:$ACCOUNT:key/* ]] || die "unexpected key ARN from create-key: $KEY_ARN"
say "$KEY_ARN"
aws kms create-alias --region "$REGION" --alias-name "$ALIAS" --target-key-id "$KEY_ARN"
ALIAS_MADE=1

OPERATOR_ADDRESS=
if have_python; then
  public_key=$(aws kms get-public-key --region "$REGION" --key-id "$KEY_ARN" \
    --query PublicKey --output text)
  OPERATOR_ADDRESS=$(address_from_public_key "$public_key") ||
    die "couldn't read an Ethereum address from the key's public key"
  say "Operator address $OPERATOR_ADDRESS"
fi

step "Giving $USER_NAME sign-only access to it"
# Scoped to the key ARN, not the alias: IAM matches Resource against the key's
# own ARN, so an alias there would grant nothing.
POLICY=$(printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["kms:Sign","kms:GetPublicKey"],"Resource":"%s"}]}' "$KEY_ARN")
aws iam put-user-policy --user-name "$USER_NAME" \
  --policy-name "$POLICY_NAME" --policy-document "$POLICY"
POLICY_MADE=1
say "Inline policy $POLICY_NAME attached"

step "Creating the access key"
# Captured straight into variables: a here-string would put the secret in a
# temp file on bash 3.2.
out=$(aws iam create-access-key --user-name "$USER_NAME" \
  --query 'AccessKey.[AccessKeyId,SecretAccessKey]' --output text)
ACCESS_KEY_ID=${out%%$'\t'*}
SECRET=${out#*$'\t'}
out=
[[ $ACCESS_KEY_ID =~ ^[A-Z0-9]{16,128}$ && $SECRET =~ ^[A-Za-z0-9/+=]{16,128}$ ]] ||
  die "create-access-key returned something unexpected"
say "$ACCESS_KEY_ID"

step "Checking the new access key can sign"
# A new access key takes a few seconds, sometimes a minute, to work everywhere
# in AWS. The panel's Verify would fail until then, so wait here instead.
# The test message is plain text, hashed with SHA-256 by KMS: no Ethereum
# transaction or EIP-712 message hashes to it, so the signature authorises
# nothing.
TMP=$(mktemp)
printf 'Stitch KMS setup check. This signature authorises nothing.' >"$TMP"
signed=
err=
for _ in $(seq 1 30); do
  if err=$(new_key_signs); then
    signed=1
    break
  fi
  printf '.'
  sleep 3
done
[[ -n $signed ]] || die "the new access key still can't sign after 90 seconds. Last error from AWS:
$err"
say "Signed a test message with the new access key."

# ---------------------------------------------------------------- hand over

if [[ -n $SAVE ]]; then
  (
    umask 077
    set -o noclobber
    printf '# Stitch operator signer for bot %s, created %s\n# key_id           = %s\n# operator_address = %s\n# For a bot run by hand: AWS_KMS_CREDENTIALS_FILE=<this file>\n[default]\naws_access_key_id = %s\naws_secret_access_key = %s\n' \
      "$NAME" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$KEY_ARN" "${OPERATOR_ADDRESS:-run Verify in the panel}" \
      "$ACCESS_KEY_ID" "$SECRET" >"$SAVE"
  )
  # The secret is safe in the file, so from here a failure keeps what was made.
  FINISHED=1
  say "Access key saved to $SAVE (readable by you only)."
fi

if [[ -n $CLIPBOARD ]]; then
  step "Paste into Stitch Fleet: Add bot → Signer → HSM — AWS KMS"
  say "Each value goes onto the clipboard in turn. If you use a clipboard history app, it will remember the secret."
  read -r -p "Press Enter to copy the Key ARN... " _
  copy "$KEY_ARN"
  read -r -p "Copied. Paste it into Key ARN, then press Enter to copy the Access key ID... " _
  copy "$ACCESS_KEY_ID"
  read -r -p "Copied. Paste it into Access key ID, then press Enter to copy the Secret access key... " _
  SECRET_ON_CLIPBOARD=1
  copy "$SECRET"
  # Handed over: stopping now (even Ctrl-C) keeps the signer, and only clears
  # the clipboard.
  FINISHED=1
  read -r -p "Copied. Paste it into Secret access key, then press Enter to clear the clipboard... " _
  copy ""
  SECRET_ON_CLIPBOARD=
  say "Clipboard cleared."
fi
SECRET=

cat <<EOF

Done. Then press Verify in the panel: it signs a throwaway message and fills in
the operator address${OPERATOR_ADDRESS:+, which should be $OPERATOR_ADDRESS}.

  Region            $REGION
  Key ARN           $KEY_ARN
  Alias             $ALIAS
  IAM user          $USER_NAME
  Access key ID     $ACCESS_KEY_ID
EOF
[[ -n $OPERATOR_ADDRESS ]] && say "  Operator address  $OPERATOR_ADDRESS"
cat <<EOF

Whoever holds this access key can sign as the operator. To cut it off:
  aws iam update-access-key --user-name $USER_NAME --access-key-id $ACCESS_KEY_ID --status Inactive
EOF
