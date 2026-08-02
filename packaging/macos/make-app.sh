#!/usr/bin/env bash
# Assemble Stitch.app around stitch-desktop (menu bar) + stitch-panel + stitch.
# The tray app starts the local panel (process runtime, no Docker) and opens the
# browser. Usage:
#   make-app.sh <path-to-stitch-desktop> <output-dir> [path-to-stitch] [path-to-stitch-panel]
set -euo pipefail
BIN="$1"
OUT="$2"
STITCH_BIN="${3:-$(dirname "$BIN")/stitch}"
PANEL_BIN="${4:-$(dirname "$BIN")/stitch-panel}"
APP="$OUT/Stitch.app"
HERE="$(cd "$(dirname "$0")" && pwd)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$HERE/Info.plist" "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/stitch-desktop"
chmod +x "$APP/Contents/MacOS/stitch-desktop"

# Both helpers are required. A missing `stitch` makes stitch-desktop exit
# immediately on launch (Finder shows nothing — no Dock, no dialog).
copy_required() {
  local src="$1"
  local name="$2"
  if [ ! -f "$src" ]; then
    echo "error: required binary '$name' not found at '$src'" >&2
    echo "error: Stitch.app cannot launch without stitch + stitch-panel next to stitch-desktop" >&2
    exit 1
  fi
  cp "$src" "$APP/Contents/MacOS/$name"
  chmod +x "$APP/Contents/MacOS/$name"
}
copy_required "$STITCH_BIN" "stitch"
copy_required "$PANEL_BIN" "stitch-panel"

# App icon (referenced by CFBundleIconFile in Info.plist).
if [ -f "$HERE/Stitch.icns" ]; then
  mkdir -p "$APP/Contents/Resources"
  cp "$HERE/Stitch.icns" "$APP/Contents/Resources/Stitch.icns"
fi

# Ad-hoc code-sign (sign identity "-") so Gatekeeper offers the normal
# right-click -> Open / "Open Anyway" path instead of rejecting a wholly unsigned
# download as "damaged". This is NOT a Developer ID signature: a downloaded copy
# still shows the unidentified-developer prompt on first launch. Sign the nested
# binaries before the bundle. Set STITCH_CODESIGN_ID to use a real identity.
SIGN_ID="${STITCH_CODESIGN_ID:--}"
RUNTIME_OPT=""
if [ "$SIGN_ID" != "-" ]; then
  RUNTIME_OPT="--options runtime"
fi
if command -v codesign >/dev/null 2>&1; then
  # shellcheck disable=SC2086
  codesign --force $RUNTIME_OPT --sign "$SIGN_ID" "$APP/Contents/MacOS/stitch"
  # shellcheck disable=SC2086
  codesign --force $RUNTIME_OPT --sign "$SIGN_ID" "$APP/Contents/MacOS/stitch-panel"
  # shellcheck disable=SC2086
  codesign --force $RUNTIME_OPT --sign "$SIGN_ID" "$APP/Contents/MacOS/stitch-desktop"
  # shellcheck disable=SC2086
  codesign --force $RUNTIME_OPT --sign "$SIGN_ID" "$APP"
fi
echo "Built $APP"
ls -la "$APP/Contents/MacOS"
