#!/bin/zsh
# Prepares a release in dist/release/ (see release/README.md):
#   app/release.sh             full release: Sam.app (zip for Sparkle + dmg), appcast.xml, signed manifest
#   app/release.sh --manifest  model manifest only: signed manifest.json for the latest release
# Add --publish to upload with the GitHub CLI; without it the gh command is printed instead.
# Signing uses the Sparkle EdDSA key in the login Keychain (app/vendor/Sparkle/bin/generate_keys).
set -e
cd "$(dirname $0)/.."
source release/version.env
REPO=elijahross/TTS
OUT=dist/release
BIN=app/vendor/Sparkle/bin
MODE=full; PUBLISH=0
for a in "$@"; do case $a in --manifest) MODE=manifest;; --publish) PUBLISH=1;; *) echo "unknown option $a"; exit 2;; esac; done

[ -x $BIN/sign_update ] || ./app/Sam/build.sh >/dev/null   # fetches Sparkle's tools
rm -rf $OUT && mkdir -p $OUT

# The manifest must move forward: installed apps ignore a version they already have.
NEW=$(python3 -c 'import json; print(json.load(open("release/manifest.json"))["version"])')
OLD=$(curl -sfL "https://github.com/$REPO/releases/latest/download/manifest.json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])' 2>/dev/null || echo 0)
if [ "$NEW" -le "$OLD" ] && [ $MODE = manifest ]; then echo "release/manifest.json version $NEW is not above the published $OLD: bump it"; exit 1; fi
cp release/manifest.json $OUT/manifest.json
$BIN/sign_update -p $OUT/manifest.json > $OUT/manifest.json.sig
echo "manifest v$NEW signed"

if [ $MODE = full ]; then
  ./app/package.sh
  ditto -c -k --sequesterRsrc --keepParent dist/Sam.app $OUT/Sam-$VERSION.zip
  cp dist/Sam-$VERSION.dmg $OUT/
  # appcast.xml: one entry for this version, EdDSA-signed, downloaded from the versioned release.
  T=$(mktemp -d); cp $OUT/Sam-$VERSION.zip $T/
  $BIN/generate_appcast --download-url-prefix "https://github.com/$REPO/releases/download/v$VERSION/" -o $OUT/appcast.xml $T >/dev/null
  rm -rf $T
  grep -q "sparkle:edSignature" $OUT/appcast.xml && echo "appcast signed (version $VERSION, build $BUILD)"
  CMD=(gh release create "v$VERSION" $OUT/Sam-$VERSION.zip $OUT/Sam-$VERSION.dmg $OUT/appcast.xml $OUT/manifest.json $OUT/manifest.json.sig
       --repo $REPO --title "Sam $VERSION" --generate-notes)
else
  TAG=$(gh release view --repo $REPO --json tagName -q .tagName)
  CMD=(gh release upload "$TAG" $OUT/manifest.json $OUT/manifest.json.sig --repo $REPO --clobber)
fi

ls -la $OUT
if [ $PUBLISH = 1 ]; then "${CMD[@]}"; else echo "\nTo publish:\n  ${(j: :)${(q)CMD[@]}}"; fi
