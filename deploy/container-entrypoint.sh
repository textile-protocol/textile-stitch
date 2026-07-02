#!/bin/sh
set -eu

# Tighten the file mode mask before creating the runtime dir or writing any
# secret, so the key/config land as 0600 and the dir as 0700 from the start.
umask 077

runtime_dir="${STITCH_RUNTIME_DIR:-/home/stitch/run}"
config_path="${STITCH_CONFIG_FILE:-${runtime_dir}/stitch.toml}"
key_path="${STITCH_PRIVATE_KEY_FILE:-${runtime_dir}/stitch.key}"

mkdir -p "${runtime_dir}"
# Fail loudly rather than write a key into a dir we cannot lock down.
chmod 700 "${runtime_dir}"

if [ -n "${STITCH_CONFIG_TOML:-}" ]; then
  printf '%s\n' "${STITCH_CONFIG_TOML}" > "${config_path}"
  unset STITCH_CONFIG_TOML
fi

if [ -n "${STITCH_PRIVATE_KEY:-}" ]; then
  printf '%s\n' "${STITCH_PRIVATE_KEY}" > "${key_path}"
  export STITCH_PRIVATE_KEY_FILE="${key_path}"
  unset STITCH_PRIVATE_KEY
fi

# MPC-wallet credentials (only one signer is used at a time; whichever the
# config selects). Each secret env var, if present, is written to a 0600 file and
# the matching *_FILE var is exported, same as the local key above. The Turnkey
# public key is not secret, so it stays a plain env var.
if [ -n "${TURNKEY_API_PRIVATE_KEY:-}" ]; then
  turnkey_key_path="${runtime_dir}/turnkey-api.key"
  printf '%s\n' "${TURNKEY_API_PRIVATE_KEY}" > "${turnkey_key_path}"
  export TURNKEY_API_PRIVATE_KEY_FILE="${turnkey_key_path}"
  unset TURNKEY_API_PRIVATE_KEY
fi

if [ -n "${MPCVAULT_API_TOKEN:-}" ]; then
  mpcvault_token_path="${runtime_dir}/mpcvault-api.token"
  printf '%s\n' "${MPCVAULT_API_TOKEN}" > "${mpcvault_token_path}"
  export MPCVAULT_API_TOKEN_FILE="${mpcvault_token_path}"
  unset MPCVAULT_API_TOKEN
fi

exec "$@"
