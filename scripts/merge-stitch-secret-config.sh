#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (c) 2026 Textile, Inc.
#
# Merge a new stitch.toml into an AWS Secrets Manager secret that also holds
# STITCH_PRIVATE_KEY — without printing the secret to stdout/stderr.
#
# Why this exists: `aws secretsmanager put-secret-value` replaces the whole
# JSON blob. Reading SecretString into a shell variable (and then into an AI
# agent transcript) is how the hot wallet key leaks. This script keeps the
# blob in a 0600 temp file, merges with jq, puts it back, and never echoes it.
#
# Usage:
#   scripts/merge-stitch-secret-config.sh <secret-arn-or-name> <path-to-stitch.toml>
#
# Requires: aws CLI v2, jq. Credentials via the usual AWS env/profile.

set -euo pipefail

usage() {
  echo "usage: $0 <secret-arn-or-name> <path-to-stitch.toml>" >&2
  exit 2
}

[ "$#" -eq 2 ] || usage
SECRET_ID="$1"
TOML_PATH="$2"

[ -f "$TOML_PATH" ] || {
  echo "error: $TOML_PATH is not a file" >&2
  exit 1
}
command -v aws >/dev/null 2>&1 || {
  echo "error: aws CLI is required" >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "error: jq is required" >&2
  exit 1
}

tmpdir="$(mktemp -d)"
cleanup() { rm -rf "$tmpdir"; }
trap cleanup EXIT
chmod 700 "$tmpdir"

cur_file="$tmpdir/current.json"
next_file="$tmpdir/next.json"

# Pull the current secret to a mode-0600 file. Do not cat it.
aws secretsmanager get-secret-value \
  --secret-id "$SECRET_ID" \
  --query SecretString \
  --output text >"$cur_file"
chmod 600 "$cur_file"

jq -n --slurpfile cur "$cur_file" --rawfile cfg "$TOML_PATH" \
  '$cur[0] + {STITCH_CONFIG_TOML: $cfg}' >"$next_file"
chmod 600 "$next_file"

aws secretsmanager put-secret-value \
  --secret-id "$SECRET_ID" \
  --secret-string "file://$next_file" >/dev/null

# Confirmation only — never the secret body.
echo "updated STITCH_CONFIG_TOML on secret $SECRET_ID"
