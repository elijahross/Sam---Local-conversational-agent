#!/bin/zsh
# Builds the standalone Sam.app (dist/Sam.app) and a DMG to install it: the app, the agent
# server, Apple speech recognition, the workspace display, the computer-use helper, Sam's
# voices, uv and the pinned Python package list. The Python runtime and the
# voice, memory, judgment and turn-taking models are installed by the app itself on first run (onboarding › Setup).
set -e
cd "$(dirname $0)/.."
source release/version.env
VER=$VERSION
echo "== building parts"
# The shipped server carries no build machine paths (panic locations, crate sources): remapped,
# in its own target dir so development builds keep their cache.
(cd server && CARGO_TARGET_DIR=target/dist RUSTFLAGS="--remap-path-prefix=$HOME=~ --remap-path-prefix=$PWD=." cargo build --release --bin voice-agent 2>&1 | tail -1)
./sidecar/applestt/build.sh >/dev/null
./sidecar/workspace/build.sh >/dev/null
./sidecar/computer/build.sh >/dev/null
./app/Sam/build.sh >/dev/null

# The app's Python runtime has no numba/llvmlite (setup removes them, ~260 MB): the speech
# sidecar's voice check must run without them (2026-10-16: librosa.pyin crashed it in every app).
uv run --no-project --quiet --python 3.12 --with numpy python - <<'PYCHK'
import sys, numpy as np, wave
sys.modules["numba"] = None; sys.modules["llvmlite"] = None
src = open("sidecar/speech_server.py").read()
ns = {"np": np}; exec(src[src.index("def _f0_st"):src.index("def _drift")], ns)
w = wave.open("voices/sam-female/ref.wav"); a = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float32) / 32768
if w.getframerate() != 24000: a = np.interp(np.arange(0, len(a), w.getframerate() / 24000), np.arange(len(a)), a).astype(np.float32)
f = ns["_f0_st"](a); assert f is not None and 5 < f < 14, f
print(f"voice check runs without numba (ref pitch {f:.1f} st)")
PYCHK

echo "== assembling dist/Sam.app"
rm -rf dist/Sam.app dist/dmg
mkdir -p dist
cp -R app/Sam/build/Sam.app dist/Sam.app
R=dist/Sam.app/Contents/Resources
mkdir -p $R/bin $R/server $R/sidecar/applestt/build $R/voices $R/runtime dist/Sam.app/Contents/Library/Helpers
cp server/target/dist/release/voice-agent $R/bin/sam-server
cp sidecar/workspace/build/sam-workspace $R/bin/sam-workspace
cp "$(command -v uv)" $R/bin/uv
cp -R server/tools server/static server/mcp.json server/persona_sam.md $R/server/
cp sidecar/speech_server.py sidecar/mind_server.py sidecar/model_manager.py $R/sidecar/
cp sidecar/applestt/build/applestt $R/sidecar/applestt/build/
for v in sam-male sam-female; do mkdir -p $R/voices/$v; cp -R voices/$v/{ref.wav,ref.txt,voice.json,fillers.json,fillers} $R/voices/$v/; done
cp app/runtime/requirements.lock $R/runtime/
cp -R "sidecar/computer/build/Sam Computer Use.app" "dist/Sam.app/Contents/Library/Helpers/"
# The helper shows up in Privacy & Security (Accessibility, Screen Recording): Sam's icon there too.
HELPER="dist/Sam.app/Contents/Library/Helpers/Sam Computer Use.app"
mkdir -p "$HELPER/Contents/Resources"
cp $R/AppIcon.icns "$HELPER/Contents/Resources/"
/usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" "$HELPER/Contents/Info.plist" 2>/dev/null || /usr/libexec/PlistBuddy -c "Set :CFBundleIconFile AppIcon" "$HELPER/Contents/Info.plist"

echo "== signing"
source app/sign.sh
echo "identity: $SIGN_ID$([ $DIST = 1 ] && echo ' (distribution: hardened runtime + notarization)')"
# Inside out: loose executables, the helper app, Sparkle, then the app itself.
sign $R/bin/sam-server "$ENT/helper.entitlements"
for f in $R/bin/sam-workspace $R/bin/uv $R/sidecar/applestt/build/applestt; do sign "$f"; done
sign "dist/Sam.app/Contents/Library/Helpers/Sam Computer Use.app/Contents/MacOS/sam-computer" "$ENT/helper.entitlements"
sign "dist/Sam.app/Contents/Library/Helpers/Sam Computer Use.app" "$ENT/helper.entitlements" one.sam.computeruse
sign_sparkle dist/Sam.app
sign dist/Sam.app "$ENT/Sam.entitlements" one.sam.app
codesign --verify --deep --strict dist/Sam.app && echo "signature ok"
if [ $DIST = 1 ]; then
  # Notarize the app and staple its ticket (also inside the update zip and the dmg).
  ditto -c -k --keepParent dist/Sam.app dist/Sam-notarize.zip
  notarize dist/Sam-notarize.zip || { rm -f dist/Sam-notarize.zip; echo "!! stopping: the app is signed but not notarized"; exit 1; }
  rm -f dist/Sam-notarize.zip
  xcrun stapler staple dist/Sam.app
  spctl --assess --type execute -vv dist/Sam.app
fi

echo "== dmg"
mkdir -p dist/dmg
cp -R dist/Sam.app dist/dmg/
ln -s /Applications dist/dmg/Applications
# The mounted volume shows Sam's icon (.VolumeIcon.icns + the custom-icon flag on the volume).
cp $R/AppIcon.icns dist/dmg/.VolumeIcon.icns
rm -f dist/Sam-$VER.dmg dist/Sam-rw.dmg
hdiutil create -volname "Sam" -srcfolder dist/dmg -ov -format UDRW dist/Sam-rw.dmg >/dev/null
MNT=$(mktemp -d /tmp/sam-dmg.XXXXXX)
hdiutil attach dist/Sam-rw.dmg -nobrowse -noautoopen -mountpoint "$MNT" >/dev/null
SetFile -a C "$MNT"
SetFile -a V "$MNT/.VolumeIcon.icns"
hdiutil detach "$MNT" >/dev/null
rmdir "$MNT" 2>/dev/null || true
hdiutil convert dist/Sam-rw.dmg -format UDZO -o dist/Sam-$VER.dmg >/dev/null
rm -f dist/Sam-rw.dmg
rm -rf dist/dmg
if [ $DIST = 1 ]; then
  # The dmg itself: signed, notarized, stapled (Gatekeeper checks it when it is opened).
  codesign --force --sign "$SIGN_ID" --timestamp dist/Sam-$VER.dmg
  notarize dist/Sam-$VER.dmg || { echo "!! stopping: the dmg is signed but not notarized"; exit 1; }
  xcrun stapler staple dist/Sam-$VER.dmg
  spctl --assess --type open --context context:primary-signature -vv dist/Sam-$VER.dmg
fi
# The .dmg file's own icon in Finder (kept locally; downloads show the generic disk image).
# Not on a notarized dmg: setting the icon adds a resource fork after signing.
[ $DIST = 1 ] || app/Sam/build/make_icon --set-icon $R/AppIcon.icns dist/Sam-$VER.dmg || true
du -sh dist/Sam.app dist/Sam-$VER.dmg
