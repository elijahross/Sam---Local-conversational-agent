#!/bin/zsh
# Code signing for Sam's builds (sourced by app/package.sh and app/Sam/build.sh).
#
# Identity: SIGN_ID if set, else a "Developer ID Application" certificate in the keychain
# (distribution: hardened runtime, secure timestamp, notarization), else the local
# "Sam Local Code Signing" identity from sidecar/computer/signing.sh (development: macOS keeps
# permission grants across rebuilds), else ad-hoc.
# Notarization uses a notarytool keychain profile (NOTARY_PROFILE, default "sam-notary"):
#   xcrun notarytool store-credentials sam-notary --apple-id <you@…> --team-id <TEAMID>
SIGN_ID=${SIGN_ID:-$(security find-identity -v -p codesigning 2>/dev/null | grep "Developer ID Application" | head -1 | sed -E 's/.*"(.*)"/\1/')}
if [ -n "$SIGN_ID" ]; then
  DIST=1
else
  DIST=0
  SIGN_ID=$(security find-identity -p codesigning 2>/dev/null | grep -E "(Sam|Rowan) Local Code Signing" | head -1 | awk '{print $2}')
  SIGN_ID=${SIGN_ID:--}
fi
NOTARY_PROFILE=${NOTARY_PROFILE:-sam-notary}
ENT="$(cd "$(dirname "${(%):-%x}")" && pwd)/entitlements"

# sign <path> [entitlements file] [identifier]
sign() {
  local args=(--force --sign "$SIGN_ID")
  if [ $DIST = 1 ]; then args+=(--options runtime --timestamp); else args+=(--timestamp=none); fi
  [ -n "$2" ] && args+=(--entitlements "$2")
  [ -n "$3" ] && args+=(--identifier "$3")
  codesign "${args[@]}" "$1"
}

# Sparkle.framework, inside out (Sparkle's documented order; the Downloader keeps its own
# entitlements: it is sandboxed).
sign_sparkle() {
  local F="$1/Contents/Frameworks/Sparkle.framework/Versions/B"
  [ -d "$F" ] || return 0
  local args=(--force --sign "$SIGN_ID")
  if [ $DIST = 1 ]; then args+=(--options runtime --timestamp); else args+=(--timestamp=none); fi
  codesign "${args[@]}" "$F/XPCServices/Installer.xpc"
  codesign "${args[@]}" --preserve-metadata=entitlements "$F/XPCServices/Downloader.xpc"
  codesign "${args[@]}" "$F/Autoupdate"
  codesign "${args[@]}" "$F/Updater.app"
  codesign "${args[@]}" "$1/Contents/Frameworks/Sparkle.framework"
}

# notarize <file.zip|file.dmg>: submit, wait, fail loudly with Apple's log on rejection.
notarize() {
  [ $DIST = 1 ] || { echo "(not notarized: no Developer ID certificate)"; return 0; }
  if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
    echo "!! notarytool profile \"$NOTARY_PROFILE\" not found. Store it once:"
    echo "   xcrun notarytool store-credentials $NOTARY_PROFILE --apple-id <your Apple ID> --team-id <TEAM ID>"
    return 1
  fi
  local out id
  # Offline / upload errors print no JSON: stop with notarytool's message instead of guessing.
  if ! out=$(xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json 2>&1); then
    echo "!! notarytool could not submit $1:"; echo "$out" | tail -3
    return 1
  fi
  id=$(echo "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id", ""))' 2>/dev/null)
  [ -n "$id" ] || { echo "!! notarytool gave no submission id:"; echo "$out" | tail -3; return 1; }
  if ! echo "$out" | grep -q '"status" *: *"Accepted"'; then
    echo "!! notarization of $1 failed:"; xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE"
    return 1
  fi
  echo "notarized $1 ($id)"
}
