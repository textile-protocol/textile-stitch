#!/usr/bin/env bash
# Build a distributable Stitch.dmg: a drag-to-Applications disk image wrapped
# around a built Stitch.app. This is the native macOS install experience — the
# user opens the image and drags Stitch onto the Applications alias, so the app
# runs from a stable path under /Applications instead of from ~/Downloads.
#
# That move matters for Stitch specifically. A freshly-downloaded, quarantined
# app launched in place is subject to Gatekeeper App Translocation: macOS runs it
# from a randomized read-only mount, which breaks stitch-setup's lookup of the
# sibling `stitch` binary (see make-app.sh) and the in-app Update button. Moving
# the app in Finder — which is exactly what dragging it to Applications is —
# disables translocation and gives it a writable, predictable home.
#
# Signs the finished image with $STITCH_CODESIGN_ID when set (Developer ID),
# matching make-app.sh. Notarizing/stapling the .dmg happens in CI; a single
# notarization of the image also covers the app inside it.
#
# Usage: make-dmg.sh <path-to-Stitch.app> <output-dir>
set -euo pipefail
APP="$1"
OUT="$2"
HERE="$(cd "$(dirname "$0")" && pwd)"
VOL="Stitch"
DMG="$OUT/Stitch.dmg"
# Optional custom window background (with the usual "drag here" arrow). Finder
# uses this image at native pixel size rather than scaling it, so it must match
# the 600x400 window below exactly.
BG="$HERE/dmg-background.png"

[ -d "$APP" ] || { echo "error: app bundle not found at $APP" >&2; exit 1; }
mkdir -p "$OUT"
rm -f "$DMG"

STAGE="$(mktemp -d)"
TMP_DMG="$(mktemp -u).dmg"
trap 'rm -rf "$STAGE" "$TMP_DMG" 2>/dev/null || true' EXIT

cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
if [ -f "$BG" ]; then
  mkdir -p "$STAGE/.background"
  cp "$BG" "$STAGE/.background/background.png"
fi

# Writable image we can lay out in Finder, then convert to compressed read-only.
# Size to the staged contents plus 50 MB of slack so Finder has room to write
# .DS_Store / the background during layout (a just-fits image can fill up).
SIZE_MB=$(( $(du -sm "$STAGE" | cut -f1) + 50 ))
hdiutil create -srcfolder "$STAGE" -volname "$VOL" -fs HFS+ \
  -format UDRW -size "${SIZE_MB}m" -ov "$TMP_DMG" >/dev/null

DEV="$(hdiutil attach -readwrite -noverify -noautoopen "$TMP_DMG" \
  | awk '/Apple_HFS/ {print $1; exit}')"
# Let the volume settle before scripting Finder.
sleep 2

# Best-effort window layout: size, icon positions, and background. Finder
# automation is available on the macOS CI runners; if it ever isn't, the image
# still works — it just lacks the custom positions/arrow.
osascript <<OSA || echo "warning: Finder layout failed; shipping a plain drag-to-Applications image" >&2
tell application "Finder"
  tell disk "$VOL"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 800, 520}
    set theViewOptions to the icon view options of container window
    set arrangement of theViewOptions to not arranged
    set icon size of theViewOptions to 120
    try
      set background picture of theViewOptions to file ".background:background.png"
    end try
    set position of item "Stitch.app" of container window to {150, 205}
    set position of item "Applications" of container window to {455, 205}
    update without registering applications
    delay 1
    close
  end tell
end tell
OSA

sync
hdiutil detach "$DEV" >/dev/null 2>&1 || hdiutil detach "$DEV" -force >/dev/null
hdiutil convert "$TMP_DMG" -format UDZO -imagekey zlib-level=9 -o "$DMG" >/dev/null

# Ad-hoc ("-") DMGs aren't worth signing (nothing verifies them); sign only with
# a real Developer ID, so the image can be notarized and stapled in CI.
SIGN_ID="${STITCH_CODESIGN_ID:-}"
if [ -n "$SIGN_ID" ] && [ "$SIGN_ID" != "-" ] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign "$SIGN_ID" "$DMG"
fi
echo "Built $DMG"
