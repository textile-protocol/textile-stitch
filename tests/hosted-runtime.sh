#!/usr/bin/env bash
# Integration check for the hosted profile's real Docker/Unix-socket boundary.
# Only disposable test containers are enrolled; no production bots, keys,
# chains, Tailscale or AWS are touched. Needs root for nsenter and the volume
# mountpoint (CI runs it with sudo).
set -Eeuo pipefail
# Every check below is a silent test, so name the one that failed; without this
# a failure is just an exit code in CI.
trap 'echo "hosted-runtime.sh: check failed at line $LINENO: $BASH_COMMAND" >&2' ERR
: "${STITCH_TEST_IMAGE:?Build the panel image from this branch and set STITCH_TEST_IMAGE}"
name="stitch-hosted-$$"
bot="$name-bot"
other="$name-other"
repo="ghcr.io/textile-protocol/textile-stitch"
# The broker under test runs the production policy: bots only move onto bot
# images with a valid build attestation. The swap target is a real published
# bot digest whose bundle is checked in as a fixture (pulled anonymously from
# GHCR; the bundle itself is passed to the broker, which has no network).
fixtures="$(cd "$(dirname "$0")/.." && pwd)/src/panel/attestation/testdata"
attested="$repo@sha256:8fd05fd7d0e9e4ecc028a92c69526bb34d15c7ece1cf8bf13b34bc370e3d871d"
runtime_opts=()
runtime_limits=(--pids-limit 128 --memory 256m)
# Some CI sandboxes cannot create domain controllers. Validate resource-limit
# declarations with Compose there, and test the socket boundary without limits.
if [ "${STITCH_TEST_SKIP_LIMITS:-0}" = 1 ]; then runtime_limits=(); fi
if [ -n "${STITCH_TEST_CGROUP_PARENT:-}" ]; then runtime_opts+=(--cgroup-parent "$STITCH_TEST_CGROUP_PARENT"); fi
bots_dir="$(mktemp -d)"
# Swap request bodies live outside the directory the broker mounts.
scratch="$(mktemp -d)"
cleanup() {
  docker rm -f "$name-panel" "$name-broker" "$bot" "$other" >/dev/null 2>&1 || true
  docker volume rm "$name-socket" >/dev/null 2>&1 || true
  rm -rf "$bots_dir" "$scratch"
}
trap cleanup EXIT
# One bot directory with a config and the key files a host might leave beside
# it. Only the config may ever leave the broker.
mkdir -p "$bots_dir/bot"
printf 'marker = "hosted-runtime-config"\n' >"$bots_dir/bot/stitch.toml"
printf 'tx_live_hosted_runtime_secret' >"$bots_dir/bot/rfq-api.key"
printf 'hosted-runtime-key-secret' >"$bots_dir/bot/stitch.key"
chown -R 1000:1000 "$bots_dir"

# `sleep` as the entrypoint, so the clone on the bot image (which has one too)
# keeps running after the swap without a config or keys.
docker run "${runtime_opts[@]}" -d --name "$bot" --network none -e KEEP=env --label keep=label \
  -v "$bots_dir/bot:/home/stitch/run" --entrypoint sleep busybox:1.37 3600 >/dev/null
docker run "${runtime_opts[@]}" -d --name "$other" --network none busybox:1.37 sleep 3600 >/dev/null
docker volume create "$name-socket" >/dev/null
docker run "${runtime_opts[@]}" -d --name "$name-broker" --network none --read-only --cap-drop ALL \
  --cap-add CHOWN --cap-add DAC_OVERRIDE \
  --security-opt no-new-privileges "${runtime_limits[@]}" \
  -e "STITCH_BROKER_BOTS=$bot" -e "STITCH_BROKER_IMAGE_REPOSITORY=$repo" \
  -e "STITCH_BROKER_HOST_BOTS_DIR=$bots_dir" -e STITCH_BROKER_BOTS_DIR=/data/bots \
  -v /var/run/docker.sock:/var/run/docker.sock -v "$name-socket:/run/stitch-broker" \
  -v "$bots_dir:/data/bots" \
  "$STITCH_TEST_IMAGE" runtime-broker >/dev/null
socket="$(docker volume inspect --format '{{.Mountpoint}}' "$name-socket")/control.sock"
for _ in {1..50}; do
  if [ -S "$socket" ]; then break; fi
  sleep .2
done
[ -S "$socket" ] || { docker logs "$name-broker"; exit 1; }

broker() { curl -sS -o /dev/null -w '%{http_code}' --unix-socket "$socket" "$@"; }
# Never `producer | grep -q` under pipefail: grep quits on its first match and
# a producer still streaming (docker logs writes one frame per log line) is
# killed by SIGPIPE, so the pipeline fails with 141 on a check that passed.
# Capture first, then grep the capture.
broker_logs() { docker logs "$name-broker" 2>&1; }

# Inventory: the enrolled name only, and no env.
body="$(curl -fsS --unix-socket "$socket" http://broker/v1/bots)"
[[ "$body" == *"\"$bot\""* && "$body" != *"$other"* && "$body" != *'"env"'* ]]

# Configs: the enrolled bot's stitch.toml, and never a key file beside it.
body="$(curl -fsS --unix-socket "$socket" http://broker/v1/configs)"
[[ "$body" == *hosted-runtime-config* && "$body" == *'"dir":"bot"'* ]]
[[ "$body" != *hosted_runtime_secret* && "$body" != *hosted-runtime-key-secret* && "$body" != *rfq-api.key* ]]
# The maker key beside it is reported as present, and only as that.
[[ "$body" == *'"rfq_api_key":true'* ]]
# A quoting save with a field outside the set doesn't parse.
code="$(broker -X POST -H 'content-type: application/json' \
  --data '{"edit":{"pool":0,"rpc_url":"https://attacker"},"actor":"a","bot":"b"}' "http://broker/v1/bots/$bot/settings")"
[[ "$code" = 4* ]]

# Lifecycle, with Restart refusing to start a stopped bot.
for action in stop start restart; do
  [ "$(broker -X POST "http://broker/v1/bots/$bot/$action")" = 204 ]
  if [ "$action" = stop ]; then
    [ "$(broker -X POST "http://broker/v1/bots/$bot/restart")" = 409 ]
    [ "$(docker inspect --format '{{.State.Running}}' "$bot")" = false ]
  fi
done
[ "$(docker inspect --format '{{.State.Running}}' "$bot")" = true ]

# Logs are served for the enrolled bot only.
[ "$(broker "http://broker/v1/bots/$bot/logs?tail=5")" = 200 ]
[ "$(broker "http://broker/v1/bots/$other/logs?tail=5")" = 403 ]

# Image swaps: tags and other repositories are refused before Docker is asked.
for image in "$repo:latest" "$repo:sha-abc" "busybox:1.37" "ghcr.io/attacker/x@sha256:$(printf 'a%.0s' {1..64})"; do
  [ "$(broker -X POST -H 'content-type: application/json' --data "{\"image\":\"$image\"}" "http://broker/v1/bots/$bot/image")" = 400 ]
done
[ "$(docker inspect --format '{{.Config.Image}}' "$bot")" = busybox:1.37 ]

swap() { # image, then zero or more bundle files
  local image="$1"; shift
  jq -n --arg image "$image" '{image: $image, attestations: [inputs | tojson]}' "$@" </dev/null > "$scratch/swap.json"
  broker -X POST -H 'content-type: application/json' --data-binary "@$scratch/swap.json" "http://broker/v1/bots/$bot/image"
}

# A digest without a valid bot attestation is refused before Docker is asked:
# none at all, the panel image's bundle, and a valid bundle for another digest.
[ "$(swap "$attested")" = 403 ]
[ "$(swap "$attested" "$fixtures/panel-d5fccbe8.sigstore.json")" = 403 ]
[ "$(swap "$repo@sha256:$(printf 'b%.0s' {1..64})" "$fixtures/bot-8fd05fd7.sigstore.json")" = 403 ]
[ "$(docker inspect --format '{{.Config.Image}}' "$bot")" = busybox:1.37 ]
grep -q 'refused an unattested image' <<<"$(broker_logs)"

# A real swap onto the attested digest keeps the name, env, labels and mounts,
# and leaves no -next/-previous container behind.
[ "$(swap "$attested" "$fixtures/bot-8fd05fd7.sigstore.json")" = 204 ]
[ "$(docker inspect --format '{{.Config.Image}}' "$bot")" = "$attested" ]
[[ "$(grep 'verified the image' <<<"$(broker_logs)")" == *833f1809b9e77c6784e1625f38ead70fea5d24ea* ]]
[ "$(docker inspect --format '{{.State.Running}}' "$bot")" = true ]
grep -qx KEEP=env <<<"$(docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$bot")"
[ "$(docker inspect --format '{{index .Config.Labels "keep"}}' "$bot")" = label ]
[ "$(docker inspect --format '{{range .Mounts}}{{.Destination}}{{end}}' "$bot")" = /home/stitch/run ]
[ -z "$(docker ps -aq --filter "name=^$bot-(next|previous)\$")" ]

# No passthrough, no other containers, no policy endpoint.
for path in /containers/create "/v1/bots/$bot/exec" "/v1/bots/$bot/remove" "/v1/bots/$other/start" /v1/policy; do
  code="$(broker -X POST -H 'content-type: application/json' --data '{"Privileged":true,"Binds":["/:/host"]}' "http://broker$path")"
  [[ "$code" = 4* ]]
done

# Same uid/groups/capabilities/mounts as a compromised panel process: no daemon
# socket, no key directory, and the socket directory isn't writable.
docker run "${runtime_opts[@]}" --rm --user 1000:65532 --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges -v "$name-socket:/run/stitch-broker:ro" \
  busybox:1.37 sh -ec 'test ! -e /var/run/docker.sock; test ! -e /run/stitch; test ! -e /home/stitch/secrets; test ! -w /run/stitch-broker; test -S /run/stitch-broker/control.sock'

# The hosted panel itself, wired like docker-compose.panel.hosted.yml.
password='hosted-runtime-test-password'
hash="$(printf '%s\n' "$password" | docker run --rm -i "$STITCH_TEST_IMAGE" hash-password)"
docker run "${runtime_opts[@]}" -d --name "$name-panel" --network none --user 1000:65532 --read-only --cap-drop ALL \
  --security-opt no-new-privileges "${runtime_limits[@]}" \
  -e STITCH_PANEL_PROFILE=hosted -e STITCH_PANEL_PUBLIC_ORIGIN=https://stitch-panel.example.ts.net \
  -e "STITCH_PANEL_PASSWORD_HASH=$hash" -e "STITCH_PANEL_HOST_BOTS_DIR=$bots_dir" \
  -e STITCH_PANEL_CORRIDOR_API=off -e STITCH_PANEL_COINGECKO_API=off \
  --tmpfs /data/bots:uid=1000,gid=1000,mode=0700 -v "$name-socket:/run/stitch-broker:ro" \
  "$STITCH_TEST_IMAGE" >/dev/null
panel_pid="$(docker inspect --format '{{.State.Pid}}' "$name-panel")"
# The panel has no bind mount of any bot directory.
if grep -qF "$bots_dir" <<<"$(docker inspect --format '{{range .Mounts}}{{.Source}}{{println}}{{end}}' "$name-panel")"; then
  echo "the hosted panel has a bind mount of the bots directory" >&2; exit 1
fi
panel() { nsenter -t "$panel_pid" -n curl -sS "$@"; }
for _ in {1..50}; do
  if panel -o /dev/null -H 'Host: localhost:8420' http://127.0.0.1:8420/api/health 2>/dev/null; then break; fi
  sleep .2
done
code() { panel -o /dev/null -w '%{http_code}' "$@"; }
local_host=(-H 'Host: localhost:8420')
[ "$(code "${local_host[@]}" http://127.0.0.1:8420/api/session)" = 200 ]
grep -q '"profile":"hosted"' <<<"$(panel "${local_host[@]}" http://127.0.0.1:8420/api/session)"
# DNS rebinding / wrong host.
[ "$(code -H 'Host: attacker.example' http://127.0.0.1:8420/api/session)" = 403 ]
# No identity-header shortcut.
[ "$(code "${local_host[@]}" -H 'Tailscale-User-Login: owner@example.com' http://127.0.0.1:8420/api/bots)" = 401 ]
# Password login through the break-glass origin.
cookie="$(panel -D - -o /dev/null "${local_host[@]}" -H 'Origin: http://localhost:8420' -H 'content-type: application/json' \
  --data "{\"password\":\"$password\"}" http://127.0.0.1:8420/api/login | tr -d '\r' | sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\).*/\1/p')"
[ -n "$cookie" ]
authed=("${local_host[@]}" -H "Cookie: $cookie" -H 'Origin: http://localhost:8420')
[ "$(code "${authed[@]}" http://127.0.0.1:8420/api/bots)" = 200 ]
# Its fleet comes from the broker's copy of the config, with no key in it.
fleet="$(panel "${authed[@]}" http://127.0.0.1:8420/api/bots)"
[[ "$fleet" != *hosted_runtime_secret* && "$fleet" != *hosted-runtime-key-secret* ]]
# Refused by the hosted policy.
for route in /api/bots/x/withdraw /api/wallets/generate /api/panel/update /api/bots /api/bots/x/approve; do
  [ "$(code "${authed[@]}" -X POST -H 'content-type: application/json' --data '{}' "http://127.0.0.1:8420$route")" = 403 ]
done
[ "$(code "${authed[@]}" "http://127.0.0.1:8420/api/bots/x/config")" = 403 ]
# Wrong origin on a change.
[ "$(code "${local_host[@]}" -H "Cookie: $cookie" -H 'Origin: https://attacker.example' -X POST http://127.0.0.1:8420/api/logout)" = 403 ]
# Broker gone: fail closed, no Docker fallback.
docker stop "$name-broker" >/dev/null
[ "$(code "${authed[@]}" http://127.0.0.1:8420/api/bots)" = 502 ]
printf 'Hosted runtime boundary checks passed.\n'
