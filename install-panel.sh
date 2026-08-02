#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (c) 2026 Textile, Inc.
#
# One-command install for Stitch: a web UI for running a fleet of bots on one
# Docker host. Bot config, wallets, and live runs happen in the browser afterward.
#
# Recommended (pinned release + checksum):
#
#   TAG=vX.Y.Z   # from https://github.com/textile-protocol/textile-stitch/releases
#   curl -fsSL "https://raw.githubusercontent.com/textile-protocol/textile-stitch/${TAG}/install-panel.sh" -o install-panel.sh
#   curl -fsSL "https://github.com/textile-protocol/textile-stitch/releases/download/${TAG}/install-panel.sh.sha256" -o install-panel.sh.sha256
#   sha256sum -c install-panel.sh.sha256
#   STITCH_REF="$TAG" PANEL_IMAGE="ghcr.io/textile-protocol/textile-stitch-panel:sha-<commit>" \
#     STITCH_REQUIRE_PINNED=1 PANEL_MODE=server TS_AUTHKEY=… PANEL_USERS=… \
#     sh install-panel.sh
#
# Quick path:
#
#   curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh | sh
#
# Two modes:
#   - local computer → password login on http://127.0.0.1:8420 (loopback only)
#   - server         → Tailscale sidecar, no host port published
#
# Non-interactive: set PANEL_MODE=local|server plus the mode's credentials
# (local: PANEL_PASSWORD; server: TS_AUTHKEY and PANEL_USERS). Optionally set
# PANEL_BOTS_DIR, PANEL_DIR, PANEL_IMAGE, STITCH_REF, STITCH_COMPOSE_SHA256,
# STITCH_REQUIRE_PINNED.

set -eu

# Native Windows has no POSIX sh that can drive this installer reliably. Point
# operators at the PowerShell installer instead of failing halfway through.
case "$(uname -s 2>/dev/null || true)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    printf '%s\n' "error: install-panel.sh is for macOS/Linux. On Windows use PowerShell:" >&2
    printf '%s\n' "" >&2
    printf '%s\n' "  irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex" >&2
    printf '%s\n' "" >&2
    printf '%s\n' "Docs: https://github.com/textile-protocol/textile-stitch/blob/main/docs/install-panel.md" >&2
    exit 1
    ;;
esac

# Every secret this writes — the auth key, the password hash — lands in .env, so
# tighten the mask before creating anything rather than chmod-ing after.
umask 077

REPO_RAW="${STITCH_REPO_RAW:-https://raw.githubusercontent.com/textile-protocol/textile-stitch}"
GITHUB_API="${STITCH_GITHUB_API:-https://api.github.com/repos/textile-protocol/textile-stitch}"
DEFAULT_DIR="${HOME}/stitch-panel"
DEFAULT_BOTS_DIR_SERVER="/srv/stitch/bots"
DEFAULT_BOTS_DIR_LOCAL="${HOME}/stitch-bots"
DEFAULT_IMAGE_REPO="ghcr.io/textile-protocol/textile-stitch-panel"

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- prompting ---
# Reads come from /dev/tty, not stdin: the usual way to run this is
# `curl … | sh`, which makes stdin the script itself. Without this every prompt
# would silently read a line of shell source.
# Opening it, not just stat-ing it: /dev/tty exists as a device node even when the
# process has no controlling terminal, so `[ -r /dev/tty ]` says yes and the first
# prompt then dies with "No such device or address" instead of the message below.
have_tty() { ( : >/dev/tty ) 2>/dev/null; }

ask() { # prompt default -> echoes the answer
  _prompt="$1"; _default="${2:-}"
  if [ -n "$_default" ]; then
    printf '%s [%s]: ' "$_prompt" "$_default" >/dev/tty
  else
    printf '%s: ' "$_prompt" >/dev/tty
  fi
  IFS= read -r _answer </dev/tty || _answer=''
  [ -n "$_answer" ] || _answer="$_default"
  printf '%s' "$_answer"
}

ask_secret() { # prompt -> echoes the answer, never echoing keystrokes
  printf '%s: ' "$1" >/dev/tty
  # `stty -echo` can fail on an unusual terminal; a visible secret is worse than
  # no prompt, so bail rather than fall back to echoing it.
  stty -echo </dev/tty 2>/dev/null || die "can't turn off terminal echo; set $2 in the environment instead"
  IFS= read -r _secret </dev/tty || _secret=''
  stty echo </dev/tty 2>/dev/null || true
  printf '\n' >/dev/tty
  printf '%s' "$_secret"
}

# Unicode scalar count — matches stitch-panel's `chars().count()`, independent of
# locale. Under LC_ALL=C, `wc -m` counts UTF-8 bytes and would accept six emoji
# (24 bytes) that the panel then rejects as six characters.
password_chars() {
  # Only python3: unversioned `python` is still Python 2 on some hosts, where
  # len(sys.stdin.read()) counts UTF-8 bytes and the 12-char check lies.
  if command -v python3 >/dev/null 2>&1; then
    printf '%s' "$1" | PYTHONIOENCODING=utf-8 python3 -c 'import sys; print(len(sys.stdin.read()))'
    return
  fi
  # Probe before trusting wc -m: an unsupported LC_ALL often falls back to C,
  # where wc still succeeds but counts UTF-8 bytes (é → 2). A real UTF-8 locale
  # counts that single character as 1.
  for _loc in C.UTF-8 en_US.UTF-8 UTF-8; do
    _probe="$(printf 'é' | LC_ALL="$_loc" wc -m 2>/dev/null | tr -d ' \t')" || _probe=''
    [ "$_probe" = 1 ] || continue
    printf '%s' "$1" | LC_ALL="$_loc" wc -m | tr -d ' \t'
    return
  done
  # Last resort: byte count. Safe for ASCII (the usual password). Non-ASCII
  # without python3/UTF-8 locale is rare; a byte count would over-count and
  # incorrectly pass, so refuse rather than guess.
  if printf '%s' "$1" | LC_ALL=C grep -q '[^ -~]'; then
    die 'need python3 (or a UTF-8 locale) to count non-ASCII password characters'
  fi
  printf '%s' "$1" | wc -c | tr -d ' \t'
}

# A value from the environment, or a prompt, or a clear failure naming the
# variable to set. The third case is what an unattended run hits.
need() { # env-value prompt var-name secret?
  if [ -n "$1" ]; then printf '%s' "$1"; return; fi
  have_tty || die "$3 is not set and there is no terminal to ask on. Set it and re-run:
  $3=… sh install-panel.sh"
  if [ "${4:-}" = secret ]; then ask_secret "$2" "$3"; else ask "$2" ''; fi
}

# Password of at least 12 characters. Retries on a TTY when the typed value is
# too short or the confirmation does not match — the panel rejects short ones,
# and a silent re-prompt beats a dead install with "hashing the password failed".
need_password() { # env-value prompt var-name
  _existing="$1"
  _prompt="$2"
  _var="$3"
  if [ -n "$_existing" ]; then
    _len="$(password_chars "$_existing")"
    [ "$_len" -ge 12 ] || die "$_var must be at least 12 characters (got $_len)"
    printf '%s' "$_existing"
    return
  fi
  have_tty || die "$_var is not set and there is no terminal to ask on. Set it and re-run:
  $_var=… sh install-panel.sh"
  while :; do
    _pw="$(ask_secret "$_prompt" "$_var")"
    [ -n "$_pw" ] || die 'a panel password is required'
    _len="$(password_chars "$_pw")"
    if [ "$_len" -lt 12 ]; then
      warn "need at least 12 characters (got $_len). Try again."
      continue
    fi
    _again="$(ask_secret 'Again' "$_var")"
    if [ "$_pw" != "$_again" ]; then
      warn "those didn't match. Try again."
      continue
    fi
    printf '%s' "$_pw"
    return
  done
}

fetch() { # url dest
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2" || die "couldn't download $1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1" || die "couldn't download $1"
  else
    die 'need curl or wget to download the compose file'
  fi
}

fetch_stdout() { # url -> stdout
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$1"
  else
    die 'need curl or wget'
  fi
}

normalize_mode() { # raw -> local|server
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    local|laptop|desktop|computer) printf 'local' ;;
    server|tailscale|ts) printf 'server' ;;
    *) return 1 ;;
  esac
}

# Prefer an explicit STITCH_REF. Otherwise resolve the latest GitHub release tag
# so a fresh install is not permanently wired to mutable `main`. Falling back to
# main is still allowed (with a warning) when the API is unreachable.
resolve_ref() {
  if [ -n "${STITCH_REF:-}" ]; then
    printf '%s' "$STITCH_REF"
    return
  fi
  _tag=''
  if _json="$(fetch_stdout "$GITHUB_API/releases/latest" 2>/dev/null)"; then
    _tag="$(printf '%s' "$_json" | tr ',' '\n' | grep -m1 '"tag_name"' | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"//; s/".*//')"
  fi
  if [ -n "$_tag" ]; then
    printf '%s' "$_tag"
    return
  fi
  warn "couldn't resolve the latest release tag from GitHub; falling back to main."
  warn "Pin STITCH_REF=vX.Y.Z for a reproducible install."
  printf '%s' main
}

sha256_file() { # path -> hex digest
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die 'need sha256sum or shasum to verify downloaded files'
  fi
}

verify_sha256() { # path expected-hex-or-sha256sum-line
  _got="$(sha256_file "$1")"
  # Accept a raw 64-char hex digest or a sha256sum(1) line (`<hash>  file`).
  _want="$(printf '%s' "$2" | awk '{print $1}' | tr 'A-F' 'a-f')"
  printf '%s' "$_want" | grep -Eq '^[0-9a-f]{64}$' ||
    die "STITCH_COMPOSE_SHA256 is not a SHA-256 digest (got: $2)"
  [ "$_got" = "$_want" ] || die "checksum mismatch for $1 (got $_got, want $_want)"
}

# STITCH_REQUIRE_PINNED allowlists immutable forms — blacklisting `:latest` /
# `main` still lets `:stable` or other floating branches through.
is_pinned_image() { # image ref
  printf '%s' "$1" | grep -Eq '@sha256:[0-9a-fA-F]{64}$' && return 0
  printf '%s' "$1" | grep -Eq ':sha-[0-9a-fA-F]{7,64}$' && return 0
  return 1
}

is_pinned_ref() { # git ref
  # cargo-dist release tags: vX.Y.Z with optional pre-release / build metadata
  printf '%s' "$1" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([+.-][A-Za-z0-9.+-]+)?$' && return 0
  printf '%s' "$1" | grep -Eq '^[0-9a-fA-F]{40}$' && return 0
  return 1
}

require_pinned() {
  case "${STITCH_REQUIRE_PINNED:-}" in
    1|true|TRUE|yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

# Read KEY=value from an env file without sourcing it (secrets stay inert).
env_file_get() { # file key -> value (empty if missing)
  [ -f "$1" ] || return 0
  _line="$(grep -E "^$2=" "$1" | head -n1 || true)"
  [ -n "$_line" ] || return 0
  _val="${_line#*=}"
  case "$_val" in
    \'*\') _val="${_val#\'}"; _val="${_val%\'}" ;;
    \"*\") _val="${_val#\"}"; _val="${_val%\"}" ;;
  esac
  printf '%s' "$_val"
}

# Docker treats an image with no tag/digest as :latest.
is_floating_latest_image() {
  case "$1" in
    *@*) return 1 ;; # digest pin
  esac
  _name="${1##*/}"
  case "$_name" in
    *:latest) return 0 ;;
    *:*) return 1 ;;
    *) return 0 ;;
  esac
}

# ---------------------------------------------------------------- preflight ---
# Plain-language Docker help for operators who may never have used containers.
# Detect the OS so the install link and "how to start it" steps match the machine.
docker_os() {
  case "$(uname -s 2>/dev/null || true)" in
    Darwin) printf 'mac' ;;
    Linux) printf 'linux' ;;
    *) printf 'other' ;;
  esac
}

docker_install_url() {
  case "$(docker_os)" in
    mac) printf 'https://docs.docker.com/desktop/setup/install/mac-install/' ;;
    linux) printf 'https://docs.docker.com/engine/install/' ;;
    *) printf 'https://docs.docker.com/get-docker/' ;;
  esac
}

# reason: missing | compose | not_running
explain_docker() {
  _reason="$1"
  _os="$(docker_os)"
  say '' >&2
  case "$_reason" in
    missing)
      printf 'error: Docker is not installed on this computer.\n' >&2
      say '' >&2
      say 'Stitch runs inside Docker — a free app that hosts the web panel.' >&2
      say 'Install it once, then re-run the same install command.' >&2
      say '' >&2
      case "$_os" in
        mac)
          say 'What to do:' >&2
          say '  1. Download and install Docker Desktop for Mac:' >&2
          say "     $(docker_install_url)" >&2
          say '  2. Open Docker Desktop from Applications and wait until it says' >&2
          say '     Docker is running (the whale icon in the menu bar is steady).' >&2
          say '  3. Re-run this install command in Terminal.' >&2
          ;;
        linux)
          say 'What to do:' >&2
          say '  1. Install Docker Engine (or Docker Desktop) for your distro:' >&2
          say "     $(docker_install_url)" >&2
          say '  2. Start it, e.g. `sudo systemctl start docker` (and enable it if you want).' >&2
          say '  3. Make sure your user can run docker (`sudo usermod -aG docker $USER`,' >&2
          say '     then log out and back in), then re-run this install command.' >&2
          ;;
        *)
          say "Install Docker from $(docker_install_url), start it, then re-run this command." >&2
          ;;
      esac
      ;;
    compose)
      printf 'error: Docker Compose v2 is missing.\n' >&2
      say '' >&2
      say 'The `docker` command is present, but `docker compose` does not work.' >&2
      say 'Stitch needs Compose v2 (the plugin). The old `docker-compose` script is not enough.' >&2
      say '' >&2
      case "$_os" in
        mac)
          say 'Fix: update or reinstall Docker Desktop for Mac, open it, then re-run:' >&2
          say "  $(docker_install_url)" >&2
          ;;
        linux)
          say 'Fix: install the Compose plugin for your distro, then re-run:' >&2
          say '  https://docs.docker.com/compose/install/linux/' >&2
          ;;
        *)
          say 'Install or update Docker so `docker compose version` works, then re-run.' >&2
          ;;
      esac
      ;;
    not_running)
      printf 'error: Docker is installed but not running.\n' >&2
      say '' >&2
      say 'The panel cannot start until Docker is up.' >&2
      say '' >&2
      case "$_os" in
        mac)
          say 'What to do:' >&2
          say '  1. Open Docker Desktop from Applications (or Spotlight: Docker).' >&2
          say '  2. Wait until it finishes starting (menu bar whale icon steady /' >&2
          say '     "Docker Desktop is running").' >&2
          say '  3. Re-run this install command.' >&2
          ;;
        linux)
          say 'What to do:' >&2
          say '  1. Start Docker: `sudo systemctl start docker`' >&2
          say '  2. If you see permission denied, add yourself to the docker group:' >&2
          say '     `sudo usermod -aG docker $USER` — then log out and back in.' >&2
          say '  3. Re-run this install command.' >&2
          ;;
        *)
          say 'Start Docker, wait until it is ready, then re-run this command.' >&2
          ;;
      esac
      ;;
  esac
  say '' >&2
  exit 1
}

step 'Checking Docker'
command -v docker >/dev/null 2>&1 || explain_docker missing
docker compose version >/dev/null 2>&1 || explain_docker compose
docker info >/dev/null 2>&1 || explain_docker not_running
say 'Docker and Compose v2 are ready.'

# ------------------------------------------------------------------- inputs ---
step 'Where to install'
# PANEL_DIR is optional and defaults to $DEFAULT_DIR, so it must never be a `need`:
# `need` dies when a value is unset and there is no TTY, which would break an
# unattended install that set the required keys but left this one to default.
PANEL_DIR="${PANEL_DIR:-}"
if [ -z "$PANEL_DIR" ]; then
  if have_tty; then
    PANEL_DIR="$(ask 'Directory for the compose file and .env' "$DEFAULT_DIR")"
  else
    PANEL_DIR="$DEFAULT_DIR"
  fi
fi

env_file="$PANEL_DIR/.env"
compose_mode=''
if [ -f "$env_file" ]; then
  step 'Existing install found'
  say "$env_file already exists, so its settings are kept as they are."
  say 'Delete it first if you want to be asked again.'
  reuse_env=yes
  # Prefer PANEL_MODE from .env; fall back to inferring from TS_AUTHKEY.
  _saved_mode="$(env_file_get "$env_file" PANEL_MODE)"
  if [ -n "$_saved_mode" ]; then
    compose_mode="$(normalize_mode "$_saved_mode")" ||
      die "existing PANEL_MODE in $env_file is not local/server (got '$_saved_mode')"
  elif grep -q '^TS_AUTHKEY=' "$env_file" 2>/dev/null; then
    compose_mode=server
  else
    compose_mode=local
  fi
  # Reload pins from .env before resolving defaults — otherwise a rerun with
  # empty env defaults PANEL_IMAGE to :latest, rewrites compose to main, and
  # skews against the pinned image Compose still reads from .env.
  if [ -z "${PANEL_IMAGE:-}" ]; then
    _saved="$(env_file_get "$env_file" PANEL_IMAGE)"
    if [ -n "$_saved" ]; then
      PANEL_IMAGE="$_saved"
      say "Reusing PANEL_IMAGE from $env_file"
    fi
  fi
  if [ -z "${STITCH_REF:-}" ]; then
    _saved="$(env_file_get "$env_file" STITCH_REF)"
    if [ -n "$_saved" ]; then
      STITCH_REF="$_saved"
      say "Reusing STITCH_REF from $env_file"
    fi
  fi
else
  reuse_env=no
fi

if [ "$reuse_env" = no ]; then
  # PANEL_MODE=local|server, or ask. Local = password on loopback; server = Tailscale.
  PANEL_MODE_RAW="${PANEL_MODE:-}"
  if [ -z "$PANEL_MODE_RAW" ]; then
    if have_tty; then
      say 'Local computer: password login at http://127.0.0.1:8420 (this machine only).'
      say 'Server: Tailscale, so you can open Stitch from your other devices securely.'
      say ''
      PANEL_MODE_RAW="$(ask 'Install on a local computer or a server? (local/server)' 'local')"
    else
      die 'PANEL_MODE is not set and there is no terminal to ask on. Set PANEL_MODE=local or PANEL_MODE=server and re-run.'
    fi
  fi
  compose_mode="$(normalize_mode "$PANEL_MODE_RAW")" ||
    die "PANEL_MODE must be 'local' or 'server' (got '$PANEL_MODE_RAW')"
fi

# Server compose mounts /dev/net/tun and expects a Linux Docker host. macOS can
# still run local mode (password on loopback); Tailscale-on-Docker-Desktop is
# the awkward path, so nudge Mac operators toward local without blocking them.
if [ "$compose_mode" = server ]; then
  case "$(uname -s 2>/dev/null || true)" in
    Darwin)
      warn 'server mode is aimed at Linux hosts (Tailscale sidecar + /dev/net/tun).'
      warn 'On a Mac, prefer local mode: password login at http://127.0.0.1:8420.'
      ;;
  esac
fi

DEFAULT_BOTS_DIR="$DEFAULT_BOTS_DIR_SERVER"
if [ "$compose_mode" = local ]; then
  DEFAULT_BOTS_DIR="$DEFAULT_BOTS_DIR_LOCAL"
fi

PANEL_BOTS_DIR="${PANEL_BOTS_DIR:-}"
if [ -z "$PANEL_BOTS_DIR" ]; then
  if have_tty; then
    PANEL_BOTS_DIR="$(ask 'Directory on this host for bot configs' "$DEFAULT_BOTS_DIR")"
  else
    PANEL_BOTS_DIR="$DEFAULT_BOTS_DIR"
  fi
fi

if [ "$reuse_env" = no ] && [ "$compose_mode" = server ]; then
  step 'Tailscale'
  # Only explain what we are about to ask for. With both values already in the
  # environment this is an unattended run, and a wall of guidance is just noise.
  if [ -z "${TS_AUTHKEY:-}" ]; then
    say 'The panel is reachable only over your tailnet: a Tailscale sidecar joins the'
    say 'tailnet, terminates TLS, and proxies to the panel. Nothing is published on'
    say 'this host, because reaching the panel means reaching the Docker socket.'
    say ''
    say 'Mint a key at https://login.tailscale.com/admin/settings/keys —'
    say 'reusable, and NOT ephemeral, so the panel keeps its identity across restarts.'
  fi
  TS_AUTHKEY="$(need "${TS_AUTHKEY:-}" 'Tailscale auth key (not shown)' TS_AUTHKEY secret)"
  [ -n "$TS_AUTHKEY" ] || die 'a Tailscale auth key is required'
  case "$TS_AUTHKEY" in
    tskey-*) ;;
    *) warn "that does not look like a Tailscale auth key (they start with 'tskey-'). Continuing anyway." ;;
  esac

  if [ -z "${PANEL_USERS:-}" ]; then
    say ''
    say 'Who may drive the panel: the tailnet login(s) from the Users page of your'
    say 'Tailscale console. Comma-separate for a team. Use the login, not a nickname.'
  fi
  PANEL_USERS="$(need "${PANEL_USERS:-}" 'Tailnet login(s), comma-separated' PANEL_USERS)"
  [ -n "$PANEL_USERS" ] || die 'at least one tailnet login is required'
  case "$PANEL_USERS" in
    *@*) ;;
    *) warn "'$PANEL_USERS' has no @ in it, so it may be a nickname rather than a login. The panel will reject anything else." ;;
  esac
fi

# ------------------------------------------------------------------- ref ------
# Track whether the operator pinned each input before we apply defaults — the
# quick path defaults PANEL_IMAGE to :latest, which tracks main, not the latest
# cargo-dist release tag.
_stitch_ref_explicit=0
[ -n "${STITCH_REF:-}" ] && _stitch_ref_explicit=1

step 'Resolving install ref'
REF="$(resolve_ref)"
say "Using ref: $REF"
case "$REF" in
  main|master|dev|HEAD)
    warn "ref '$REF' is a moving branch. Prefer STITCH_REF=vX.Y.Z from a GitHub release."
    ;;
esac

# Container tags are sha-* (and latest on the default branch), not cargo-dist
# v* release tags — so the default image stays :latest unless the operator pins
# PANEL_IMAGE to a digest or sha-* tag. Strict installs set STITCH_REQUIRE_PINNED=1.
PANEL_IMAGE="${PANEL_IMAGE:-${DEFAULT_IMAGE_REPO}:latest}"

# :latest (including an untagged image ref, which Docker treats as latest) is
# published from main. Pairing it with a release-tag compose file skews when
# main moves first. If the operator did not set STITCH_REF, keep compose on
# main with the floating image.
if is_floating_latest_image "$PANEL_IMAGE"; then
  if [ "$_stitch_ref_explicit" = 0 ] && [ "$REF" != main ]; then
    warn "PANEL_IMAGE resolves to :latest; using STITCH_REF=main so compose tracks the image (resolved release was $REF)."
    warn "Pin both STITCH_REF=vX.Y.Z and PANEL_IMAGE=…:sha-… together for a release install."
    REF=main
  elif [ "$_stitch_ref_explicit" = 1 ]; then
    case "$REF" in
      main|master|dev|HEAD) ;;
      *)
        warn "STITCH_REF=$REF with floating PANEL_IMAGE=$PANEL_IMAGE can skew: a newer :latest may need compose keys this release does not ship."
        warn "Set PANEL_IMAGE to the sha-* tag from that release, or omit STITCH_REF to track main."
        ;;
    esac
  fi
fi

if require_pinned; then
  # Fail closed: do not treat a dynamically resolved /releases/latest tag as a pin.
  [ "$_stitch_ref_explicit" = 1 ] ||
    die "STITCH_REQUIRE_PINNED=1 requires STITCH_REF to be set explicitly (resolved $REF from GitHub)."
  is_pinned_image "$PANEL_IMAGE" || die "STITCH_REQUIRE_PINNED=1 requires PANEL_IMAGE to be a sha-* tag or @sha256:… digest (got $PANEL_IMAGE)."
  is_pinned_ref "$REF" || die "STITCH_REQUIRE_PINNED=1 requires STITCH_REF to be a release tag (vX.Y.Z) or 40-char commit SHA (got $REF)."
elif is_floating_latest_image "$PANEL_IMAGE"; then
  warn "PANEL_IMAGE=$PANEL_IMAGE uses a floating tag. Pin a sha-* tag or @sha256:… digest in production."
fi

if [ "$compose_mode" = local ]; then
  COMPOSE_FILE=docker-compose.panel.local.yml
else
  COMPOSE_FILE=docker-compose.panel.yml
fi

# ------------------------------------------------------------------- layout ---
step "Installing into $PANEL_DIR ($compose_mode)"
mkdir -p "$PANEL_DIR"

# Fetch compose to a temp path and verify before replacing any live file, so a
# bad download cannot poison $PANEL_DIR. STITCH_COMPOSE_SHA256 covers the server
# compose asset published with releases; local compose is not checksummed there.
_compose_tmp="$(mktemp)"
trap 'rm -f "$_compose_tmp"' EXIT
fetch "$REPO_RAW/$REF/$COMPOSE_FILE" "$_compose_tmp"
if [ "$COMPOSE_FILE" = 'docker-compose.panel.yml' ]; then
  if [ -n "${STITCH_COMPOSE_SHA256:-}" ]; then
    verify_sha256 "$_compose_tmp" "$STITCH_COMPOSE_SHA256"
    say 'Compose file checksum verified.'
  else
    warn "STITCH_COMPOSE_SHA256 is unset; compose file was not integrity-checked."
    warn "Set it from the release asset docker-compose.panel.yml.sha256 when pinning."
  fi
elif [ -n "${STITCH_COMPOSE_SHA256:-}" ]; then
  warn "STITCH_COMPOSE_SHA256 is set but this install uses $COMPOSE_FILE; checksum skipped."
fi
mv -f "$_compose_tmp" "$PANEL_DIR/$COMPOSE_FILE"
trap - EXIT

if [ "$compose_mode" = server ]; then
  mkdir -p "$PANEL_DIR/deploy"
  fetch "$REPO_RAW/$REF/deploy/panel-serve.json" "$PANEL_DIR/deploy/panel-serve.json"
  say 'Compose file and tailscale serve config in place.'
else
  say 'Local compose file in place.'
fi

step "Creating the bots root at $PANEL_BOTS_DIR"
if mkdir -p "$PANEL_BOTS_DIR" 2>/dev/null; then
  say 'Created.'
elif command -v sudo >/dev/null 2>&1; then
  say "Needs root. Running: sudo mkdir -p $PANEL_BOTS_DIR"
  sudo mkdir -p "$PANEL_BOTS_DIR" || die "couldn't create $PANEL_BOTS_DIR"
else
  die "couldn't create $PANEL_BOTS_DIR and sudo isn't available. Create it yourself and re-run."
fi

# ---------------------------------------------------------------------- env ---
if [ "$reuse_env" = no ]; then
  step 'Writing .env'
  # Written through a temp file in the same directory so a failure part-way leaves
  # no half-written file holding a real auth key or password hash.
  tmp_env="$PANEL_DIR/.env.tmp.$$"
  {
    printf '# Written by install-panel.sh. Keep it 0600.\n'
    printf 'PANEL_MODE=%s\n' "$compose_mode"
    printf 'PANEL_BOTS_DIR=%s\n' "$PANEL_BOTS_DIR"
    printf 'PANEL_IMAGE=%s\n' "$PANEL_IMAGE"
    # Persist the compose ref so a later rerun reloads the same pin pair.
    printf 'STITCH_REF=%s\n' "$REF"
    if [ "$compose_mode" = server ]; then
      printf 'TS_AUTHKEY=%s\n' "$TS_AUTHKEY"
      printf 'PANEL_USERS=%s\n' "$PANEL_USERS"
    fi
  } >"$tmp_env"
  mv "$tmp_env" "$env_file"
  chmod 600 "$env_file"
  say "Wrote $env_file (0600)."
fi

# --------------------------------------------------------------- password ----
# Hashed by the panel image itself, piped over stdin so the plaintext never
# reaches a command line where `ps` could read it.
add_password() {
  _pw="$1"
  _label="${2:-password}"
  _len="$(password_chars "$_pw")"
  [ "$_len" -ge 12 ] || die "password must be at least 12 characters (got $_len)"

  # Keep stderr: the binary's real reason (too short, docker failure, …) used to
  # disappear into /dev/null, leaving only "hashing the password failed".
  # mktemp: a predictable /tmp/…$$ path is symlink-clobberable on multi-user hosts.
  _err="$(mktemp "${TMPDIR:-/tmp}/stitch-panel-hash.XXXXXX")" ||
    die 'could not create a temp file for password hashing'
  if ! _out="$(printf '%s\n' "$_pw" | docker run --rm -i "$PANEL_IMAGE" hash-password 2>"$_err")"; then
    _msg="$(tr -d '\r' <"$_err" | sed '/^$/d; s/^Error: //' | tail -n 1)"
    rm -f "$_err"
    [ -n "$_msg" ] && die "hashing the password failed: $_msg"
    die 'hashing the password failed'
  fi
  rm -f "$_err"
  # Exact PHC line only — some Docker setups print warnings on stdout.
  _hash="$(printf '%s\n' "$_out" | grep -m1 '^\$argon2')" ||
    die 'the panel returned no argon2 password hash'

  # Single-quoted: an argon2 hash is full of $ that compose would otherwise read
  # as variable interpolation.
  # Drop any previous hash line so re-running with PANEL_PASSWORD replaces it.
  if grep -q '^PANEL_PASSWORD_HASH=' "$env_file" 2>/dev/null; then
    tmp_env="$PANEL_DIR/.env.tmp.$$"
    grep -v '^PANEL_PASSWORD_HASH=' "$env_file" >"$tmp_env"
    mv "$tmp_env" "$env_file"
    chmod 600 "$env_file"
  fi
  printf "PANEL_PASSWORD_HASH='%s'\n" "$_hash" >>"$env_file"
  say "Added $_label."
}

step "Pulling $PANEL_IMAGE"
if ! docker pull "$PANEL_IMAGE" >/dev/null; then
  _arch="$(uname -m 2>/dev/null || true)"
  case "$_arch" in
    arm64|aarch64)
      die "couldn't pull $PANEL_IMAGE — no linux/arm64 image in that tag's manifest. Apple Silicon / ARM hosts need a multi-arch panel image (amd64+arm64). Re-run after the published image includes arm64, or build from source: https://github.com/textile-protocol/textile-stitch/blob/main/docs/install-panel.md"
      ;;
  esac
  die "couldn't pull $PANEL_IMAGE"
fi
say 'Pulled.'

if [ "$compose_mode" = local ]; then
  # Password is required: it's the only way in.
  if ! grep -q '^PANEL_PASSWORD_HASH=' "$env_file" 2>/dev/null; then
    step 'Panel password'
    if [ -z "${PANEL_PASSWORD:-}" ]; then
      say 'This install is loopback-only. You log in with a password you choose now.'
    fi
    PANEL_PASSWORD="$(need_password "${PANEL_PASSWORD:-}" 'Panel password (12+ characters, not shown)' PANEL_PASSWORD)"
    add_password "$PANEL_PASSWORD" 'panel password'
  elif [ -n "${PANEL_PASSWORD:-}" ]; then
    step 'Updating the panel password'
    add_password "$PANEL_PASSWORD" 'panel password'
  fi
else
  # Optional fallback for tagged Tailscale nodes / Funnel edge cases.
  if [ -n "${PANEL_PASSWORD:-}" ]; then
    step 'Adding the password fallback'
    add_password "$PANEL_PASSWORD" 'password fallback'
  elif [ "$reuse_env" = no ] && have_tty; then
    # Retry on short / mismatched passwords. .env is already written by here, so
    # dying would leave reuse_env=yes on the next run and skip this prompt forever.
    step 'Password fallback (optional)'
    say 'Useful if you browse from a tagged Tailscale node, which gets no identity'
    say 'header. Press Enter to skip.'
    while :; do
      _pw="$(ask_secret 'Panel password (12+ characters, not shown)' PANEL_PASSWORD)"
      if [ -z "$_pw" ]; then
        say 'Skipped.'
        break
      fi
      _len="$(password_chars "$_pw")"
      if [ "$_len" -lt 12 ]; then
        warn "need at least 12 characters (got $_len). Try again."
        continue
      fi
      _again="$(ask_secret 'Again' PANEL_PASSWORD)"
      if [ "$_pw" != "$_again" ]; then
        warn "those didn't match. Try again."
        continue
      fi
      add_password "$_pw" 'password fallback'
      break
    done
  fi
fi

# ---------------------------------------------------------------------- up ----
step 'Starting the panel'
cd "$PANEL_DIR"
# --no-build because the image is already pulled: this deployment has no source
# tree, so a `build:` section in the compose file must not be reached.
docker compose -f "$COMPOSE_FILE" up -d --no-build

step 'Done'
if [ "$compose_mode" = local ]; then
  say 'Open http://127.0.0.1:8420 and log in with the password you set.'
else
  # Best-effort: ask the sidecar what name it ended up with, so the operator gets a
  # URL rather than a pattern to fill in. Never fatal — the panel is already up.
  host=''
  if host_json="$(docker compose -f "$COMPOSE_FILE" exec -T tailscale tailscale status --json 2>/dev/null)"; then
    host="$(printf '%s' "$host_json" | tr ',' '\n' | grep -m1 '"DNSName"' | sed 's/.*"DNSName":"//; s/\.".*//; s/\.$//')"
  fi
  if [ -n "$host" ]; then
    say "Open https://$host"
  else
    say 'Open https://stitch-panel.<your-tailnet>.ts.net'
    say '(run `tailscale status` on any tailnet device if you are not sure of the name)'
  fi
fi
say ''
say 'In the web UI: Add a bot, pick a corridor, paste your operator wallet key,'
say 'approve tokens, then start. The installer does not configure bots for you.'
say ''
say "Logs:    cd $PANEL_DIR && docker compose -f $COMPOSE_FILE logs -f panel"
say "Stop:    cd $PANEL_DIR && docker compose -f $COMPOSE_FILE down"
say 'Advanced setups (custom reverse proxy, building from source):'
say 'https://github.com/textile-protocol/textile-stitch/blob/main/docs/install-panel.md'
