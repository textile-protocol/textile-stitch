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

# Capture device + mount point from this attach. If another volume already uses
# the volname, macOS mounts us at "/Volumes/Stitch 1" (etc.) while the HFS
# volume name — and Finder's `disk` name — stay "Stitch". Never key Finder off
# basename(mount) ("Stitch 1") or bare `disk "Stitch"` (ambiguous); always use
# the POSIX mount path from this attach.
ATTACH_OUT="$(hdiutil attach -readwrite -noverify -noautoopen "$TMP_DMG")"
DEV="$(printf '%s\n' "$ATTACH_OUT" | awk '/Apple_HFS/ {print $1; exit}')"
# Mount path is everything from /Volumes/… (may contain spaces).
MNT="$(printf '%s\n' "$ATTACH_OUT" | awk '/Apple_HFS/ {
  match($0, /\/Volumes\/.*/); if (RSTART) print substr($0, RSTART, RLENGTH); exit
}')"
[ -n "$DEV" ] && [ -n "$MNT" ] && [ -d "$MNT" ] || {
  echo "error: failed to attach $TMP_DMG (dev='$DEV' mnt='$MNT')" >&2
  printf '%s\n' "$ATTACH_OUT" >&2
  exit 1
}
# Escape for AppleScript double-quoted strings.
as_quote() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}
MNT_AS="$(as_quote "$MNT")"
# Let the volume settle before scripting Finder.
sleep 2

# Best-effort window layout: size, icon positions, and background. Finder
# automation is available on the macOS CI runners; if it ever isn't, the image
# still works — it just lacks the custom positions/arrow.
osascript <<OSA || echo "warning: Finder layout failed; shipping a plain drag-to-Applications image" >&2
set mntAlias to POSIX file "$MNT_AS" as alias
tell application "Finder"
  open mntAlias
  set win to container window of mntAlias
  set current view of win to icon view
  set toolbar visible of win to false
  set statusbar visible of win to false
  set the bounds of win to {200, 120, 800, 520}
  set theViewOptions to the icon view options of win
  set arrangement of theViewOptions to not arranged
  set icon size of theViewOptions to 120
  try
    set background picture of theViewOptions to file ".background:background.png" of mntAlias
  end try
  set position of item "Stitch.app" of win to {150, 205}
  set position of item "Applications" of win to {455, 205}
  update without registering applications
  delay 1
  close win
end tell
OSA

sync

# Finder often keeps the volume busy for a few seconds after layout (Spotlight /
# .DS_Store / the container window). A single detach or even -force can fail with
# "Resource busy" (hdiutil exit 16) on CI runners. Close/eject *this* mount via
# its POSIX path, then retry detach with backoff before converting.
is_detached() {
  local target="$1"
  local mount="$2"
  # Mount point gone and device no longer listed → already ejected (e.g. via Finder).
  if [ ! -d "$mount" ] && ! hdiutil info 2>/dev/null | grep -Fq "$target"; then
    return 0
  fi
  return 1
}

detach_dmg() {
  local target="$1"
  local mount="$2"
  local mount_as="$3"
  local attempt
  # Best-effort: drop Finder's hold on this mount before hdiutil fights it.
  osascript <<OSA >/dev/null 2>&1 || true
set mntAlias to POSIX file "$mount_as" as alias
tell application "Finder"
  try
    close (every window whose target is mntAlias)
  end try
  try
    eject mntAlias
  end try
end tell
OSA
  if is_detached "$target" "$mount"; then
    return 0
  fi
  for attempt in 1 2 3 4 5 6 7 8; do
    if hdiutil detach "$target" >/dev/null 2>&1; then
      return 0
    fi
    if hdiutil detach "$target" -force >/dev/null 2>&1; then
      return 0
    fi
    # Device node vs path can disagree after a partial eject — only this mount.
    if [ -d "$mount" ] && hdiutil detach "$mount" -force >/dev/null 2>&1; then
      return 0
    fi
    if is_detached "$target" "$mount"; then
      return 0
    fi
    sleep "$attempt"
  done
  echo "error: could not detach $target ($mount) after retries" >&2
  hdiutil info >&2 || true
  return 1
}

detach_dmg "$DEV" "$MNT" "$MNT_AS"
hdiutil convert "$TMP_DMG" -format UDZO -imagekey zlib-level=9 -o "$DMG" >/dev/null

# Ad-hoc ("-") DMGs aren't worth signing (nothing verifies them); sign only with
# a real Developer ID, so the image can be notarized and stapled in CI.
SIGN_ID="${STITCH_CODESIGN_ID:-}"
if [ -n "$SIGN_ID" ] && [ "$SIGN_ID" != "-" ] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign "$SIGN_ID" "$DMG"
fi
echo "Built $DMG"
