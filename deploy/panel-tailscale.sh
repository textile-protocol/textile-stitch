#!/bin/sh
set -eu

# Compose puts this shim first in PATH. Remove only that leading directory so
# the final exec reaches the image's real CLI rather than recursing into us.
case "$PATH" in
  "${0%/*}":*) PATH="${PATH#*:}" ;;
  *) echo 'panel tailscale shim must be first in PATH' >&2; exit 1 ;;
esac
export PATH

# containerboot invokes `tailscale --socket=... set` on every authenticated
# startup, before configuring Serve. TS_EXTRA_ARGS applies only to `up`, which
# auth-once deliberately skips for an enrolled node. `set` changes preferences
# without using an enrollment key; its failure must propagate to containerboot.
case "${1:-}" in
  --socket=*) socket="$1"; shift ;;
  *) socket='' ;;
esac

if [ "${1:-}" = set ]; then
  set -- "$@" --ssh=false --accept-routes=false --advertise-routes= \
    --advertise-exit-node=false --advertise-connector=false \
    --exit-node= --exit-node-allow-lan-access=false
fi

if [ -n "$socket" ]; then
  set -- "$socket" "$@"
fi
exec tailscale "$@"
