#!/usr/bin/env bash
# Build a distributable Stitch.dmg: a drag-to-Applications disk image wrapped
# around a built Stitch.app. This is the native macOS install experience — the
# user opens the image and drags Stitch onto the Applications alias, so the app
# runs from a stable path under /Applications instead of from ~/Downloads.
#
# That move matters for Stitch specifically. A freshly-downloaded, quarantined
# app launched in place is subject to Gatekeeper App Translocation: macOS runs it
# from a randomized read-only mount, which breaks stitch-desktop's lookup of the
# sibling `stitch` binary (see make-app.sh) and the in-app Update button. Moving
# the app in Finder — which is exactly what dragging it to Applications is —
# disables translocation and gives it a writable, predictable home.
#
# Layout (background + icon positions) is applied with dmgbuild, which writes
# .DS_Store directly. Do not go back to Finder AppleScript — it fails on current
# GitHub macos runners (-10010 / missing window) and silently shipped plain DMGs.
#
# Signs the finished image with $STITCH_CODESIGN_ID when set (Developer ID),
# matching make-app.sh. Notarizing/stapling the .dmg happens in CI; a single
# notarization of the image also covers the app inside it.
#
# Usage: make-dmg.sh <path-to-Stitch.app> <output-dir>
set -euo pipefail

APP_IN="$1"
OUT_IN="$2"
HERE="$(cd "$(dirname "$0")" && pwd)"
VOL="Stitch"
# Custom window background (with the usual "drag here" arrow). Finder uses this
# image at native pixel size rather than scaling it, so it must match the
# 600x400 window in dmgbuild_settings.py exactly.
BG="$HERE/dmg-background.png"
SETTINGS="$HERE/dmgbuild_settings.py"

[ -d "$APP_IN" ] || { echo "error: app bundle not found at $APP_IN" >&2; exit 1; }
[ -f "$BG" ] || { echo "error: missing DMG background at $BG" >&2; exit 1; }
[ -f "$SETTINGS" ] || { echo "error: missing dmgbuild settings at $SETTINGS" >&2; exit 1; }

# Absolute paths — dmgbuild resolves files from the process cwd.
APP="$(cd "$(dirname "$APP_IN")" && pwd)/$(basename "$APP_IN")"
OUT="$(mkdir -p "$OUT_IN" && cd "$OUT_IN" && pwd)"
DMG="$OUT/Stitch.dmg"
rm -f "$DMG"

# dmgbuild shells out to hdiutil; macOS only.
if ! command -v hdiutil >/dev/null 2>&1; then
  echo "error: hdiutil not found (make-dmg.sh must run on macOS)" >&2
  exit 1
fi

ensure_dmgbuild() {
  if command -v dmgbuild >/dev/null 2>&1; then
    return 0
  fi
  python3 -m pip install --user --quiet 'dmgbuild>=1.6.7,<2'
  local user_base
  user_base="$(python3 -m site --user-base)"
  export PATH="${user_base}/bin:${PATH}"
  command -v dmgbuild >/dev/null 2>&1 || {
    echo "error: dmgbuild not on PATH after pip install (looked in ${user_base}/bin)" >&2
    exit 1
  }
}

ensure_dmgbuild

# Creates a compressed UDZO image with background + icon layout baked in.
# Fail hard on any error — never ship a plain white drag-to-Applications DMG.
dmgbuild -s "$SETTINGS" \
  -D "app=$APP" \
  -D "background=$BG" \
  "$VOL" "$DMG"

# Ad-hoc ("-") DMGs aren't worth signing (nothing verifies them); sign only with
# a real Developer ID, so the image can be notarized and stapled in CI.
SIGN_ID="${STITCH_CODESIGN_ID:-}"
if [ -n "$SIGN_ID" ] && [ "$SIGN_ID" != "-" ] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign "$SIGN_ID" "$DMG"
fi
echo "Built $DMG"
