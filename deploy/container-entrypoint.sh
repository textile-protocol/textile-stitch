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

# file_secret VAR FILE_VAR PATH: if VAR is set, write it to PATH (0600 via the
# umask above), export FILE_VAR=PATH and unset VAR, so the secret is not in the
# exec'd environment that `docker inspect`, Docker's on-disk container config
# and /proc all expose. Unset or empty VAR means the operator mounted the file
# instead; leave everything alone.
file_secret() {
  eval "secret_value=\${$1:-}"
  if [ -n "${secret_value}" ]; then
    printf '%s\n' "${secret_value}" > "$3"
    export "$2=$3"
    unset "$1"
  fi
  unset secret_value
}

# Every secret the bot reads from the environment. Only one signer backend is
# used at a time (whichever the config selects), but all of them are filed so
# a stray one doesn't ride along. File names match what the panel provisions
# (src/panel/provision.rs). A new secret env var belongs in this list, and in
# tests/container_entrypoint.rs.
#
# STITCH_RFQ_API_KEY is the maker credential from `stitch connect`. Only the
# default `[rfq].api_key_env` name is handled; a custom one is the operator's
# to pass. The Turnkey API public key and FIREBLOCKS_API_KEY are identifiers,
# not secrets, so they stay plain env vars.
file_secret STITCH_PRIVATE_KEY STITCH_PRIVATE_KEY_FILE "${key_path}"
file_secret STITCH_RFQ_API_KEY STITCH_RFQ_API_KEY_FILE "${runtime_dir}/rfq-api.key"
file_secret TURNKEY_API_PRIVATE_KEY TURNKEY_API_PRIVATE_KEY_FILE "${runtime_dir}/turnkey-api.key"
file_secret MPCVAULT_API_TOKEN MPCVAULT_API_TOKEN_FILE "${runtime_dir}/mpcvault-api.token"
file_secret FIREBLOCKS_API_PRIVATE_KEY FIREBLOCKS_API_PRIVATE_KEY_FILE "${runtime_dir}/fireblocks-api.key"

exec "$@"
