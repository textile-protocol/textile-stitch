#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (c) 2026 Textile, Inc.
#
# One-command install for Stitch: a web UI for running a fleet of bots on one
# Docker host. Bot config, wallets, and live runs happen in the browser afterward.
#
#   curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh | sh
#
# Two modes:
#   - local computer → password login on http://127.0.0.1:8420 (loopback only)
#   - server         → Tailscale sidecar, no host port published
#
# Non-interactive: set PANEL_MODE=local|server plus the mode's credentials
# (local: PANEL_PASSWORD; server: TS_AUTHKEY and PANEL_USERS). Optionally set
# PANEL_BOTS_DIR, PANEL_DIR, PANEL_IMAGE.

set -eu

# Every secret this writes — the auth key, the password hash — lands in .env, so
# tighten the mask before creating anything rather than chmod-ing after.
umask 077

REPO_RAW="${STITCH_REPO_RAW:-https://raw.githubusercontent.com/textile-protocol/textile-stitch}"
REF="${STITCH_REF:-main}"
DEFAULT_IMAGE="ghcr.io/textile-protocol/textile-stitch-panel:latest"
DEFAULT_DIR="${HOME}/stitch-panel"
DEFAULT_BOTS_DIR_SERVER="/srv/stitch/bots"
DEFAULT_BOTS_DIR_LOCAL="${HOME}/stitch-bots"

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

# A value from the environment, or a prompt, or a clear failure naming the
# variable to set. The third case is what an unattended run hits.
need() { # env-value prompt var-name secret?
  if [ -n "$1" ]; then printf '%s' "$1"; return; fi
  have_tty || die "$3 is not set and there is no terminal to ask on. Set it and re-run:
  $3=… sh install-panel.sh"
  if [ "${4:-}" = secret ]; then ask_secret "$2" "$3"; else ask "$2" ''; fi
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

normalize_mode() { # raw -> local|server
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    local|laptop|desktop|computer) printf 'local' ;;
    server|tailscale|ts) printf 'server' ;;
    *) return 1 ;;
  esac
}

# ---------------------------------------------------------------- preflight ---
step 'Checking Docker'
command -v docker >/dev/null 2>&1 || die 'Docker is not installed. See https://docs.docker.com/get-docker/'
docker compose version >/dev/null 2>&1 ||
  die 'Docker Compose v2 is missing. `docker compose version` has to work (the old `docker-compose` script is not enough).'
docker info >/dev/null 2>&1 ||
  die 'the Docker daemon is not reachable. Start Docker (or add yourself to the docker group) and re-run.'
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
  # Infer mode from what the previous install wrote so we pull the matching compose.
  if grep -q '^TS_AUTHKEY=' "$env_file" 2>/dev/null; then
    compose_mode=server
  else
    compose_mode=local
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

PANEL_IMAGE="${PANEL_IMAGE:-$DEFAULT_IMAGE}"

if [ "$compose_mode" = local ]; then
  COMPOSE_FILE=docker-compose.panel.local.yml
else
  COMPOSE_FILE=docker-compose.panel.yml
fi

# ------------------------------------------------------------------- layout ---
step "Installing into $PANEL_DIR ($compose_mode)"
mkdir -p "$PANEL_DIR"

# The compose file is downloaded rather than written here on purpose: it is the
# same file the repo ships, so this installer can never drift from the deployment
# it is supposed to be installing.
fetch "$REPO_RAW/$REF/$COMPOSE_FILE" "$PANEL_DIR/$COMPOSE_FILE"
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
  _hash="$(printf '%s\n' "$_pw" | docker run --rm -i "$PANEL_IMAGE" hash-password 2>/dev/null)" ||
    die 'hashing the password failed'
  [ -n "$_hash" ] || die 'the panel returned an empty password hash'
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
docker pull "$PANEL_IMAGE" >/dev/null || die "couldn't pull $PANEL_IMAGE"
say 'Pulled.'

if [ "$compose_mode" = local ]; then
  # Password is required: it's the only way in.
  if ! grep -q '^PANEL_PASSWORD_HASH=' "$env_file" 2>/dev/null; then
    step 'Panel password'
    if [ -z "${PANEL_PASSWORD:-}" ]; then
      say 'This install is loopback-only. You log in with a password you choose now.'
    fi
    PANEL_PASSWORD="$(need "${PANEL_PASSWORD:-}" 'Panel password (12+ characters, not shown)' PANEL_PASSWORD secret)"
    [ -n "$PANEL_PASSWORD" ] || die 'a panel password is required for a local install'
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
    step 'Password fallback (optional)'
    say 'Useful if you browse from a tagged Tailscale node, which gets no identity'
    say 'header. Press Enter to skip.'
    _pw="$(ask_secret 'Panel password (12+ characters, not shown)' PANEL_PASSWORD)"
    if [ -n "$_pw" ]; then add_password "$_pw" 'password fallback'; else say 'Skipped.'; fi
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
