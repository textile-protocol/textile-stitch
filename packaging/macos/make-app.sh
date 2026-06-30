#!/usr/bin/env bash
# Assemble Stitch.app around a built stitch-setup binary. The bot binary
# (`stitch`) is bundled alongside it so the GUI can find and supervise it when
# the app is launched from Finder (where PATH is minimal). By default `stitch`
# is taken from the same directory as the stitch-setup binary (cargo builds both
# into the same target dir); override with a third argument.
# Usage: make-app.sh <path-to-stitch-setup-binary> <output-dir> [path-to-stitch-binary]
set -euo pipefail
BIN="$1"
OUT="$2"
STITCH_BIN="${3:-$(dirname "$BIN")/stitch}"
APP="$OUT/Stitch.app"
HERE="$(cd "$(dirname "$0")" && pwd)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$HERE/Info.plist" "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/stitch-setup"
chmod +x "$APP/Contents/MacOS/stitch-setup"
if [ -f "$STITCH_BIN" ]; then
  cp "$STITCH_BIN" "$APP/Contents/MacOS/stitch"
  chmod +x "$APP/Contents/MacOS/stitch"
else
  echo "warning: stitch bot binary not found at $STITCH_BIN; the app's Start/Approve/Update will be disabled until stitch is on PATH" >&2
fi

# Ad-hoc code-sign (sign identity "-") so Gatekeeper offers the normal
# right-click -> Open / "Open Anyway" path instead of rejecting a wholly unsigned
# download as "damaged". This is NOT a Developer ID signature: a downloaded copy
# still shows the unidentified-developer prompt on first launch. Sign the nested
# binaries before the bundle. Set STITCH_CODESIGN_ID to use a real identity.
SIGN_ID="${STITCH_CODESIGN_ID:--}"
if command -v codesign >/dev/null 2>&1; then
  [ -f "$APP/Contents/MacOS/stitch" ] && codesign --force --sign "$SIGN_ID" "$APP/Contents/MacOS/stitch"
  codesign --force --sign "$SIGN_ID" "$APP/Contents/MacOS/stitch-setup"
  codesign --force --sign "$SIGN_ID" "$APP"
fi
echo "Built $APP"
