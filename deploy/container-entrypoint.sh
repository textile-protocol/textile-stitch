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

exec "$@"
