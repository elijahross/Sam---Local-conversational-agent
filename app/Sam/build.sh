#!/bin/zsh
# Builds "Sam.app" (agent app, no Dock icon). Signed with the local identity from
# sidecar/computer/signing.sh so macOS keeps Microphone / Accessibility / Input Monitoring
# grants across rebuilds.
set -e
cd "$(dirname $0)"
source ../../release/version.env   # VERSION, BUILD (Sparkle compares BUILD)
APP="build/Sam.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks"
SDK=$(ls -d /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk 2>/dev/null | head -1)
SDK=${SDK:-$(xcrun --sdk macosx --show-sdk-path)}   # CI: Xcode's SDK

# Sparkle (app updates), pinned by version and checksum; fetched once into app/vendor/.
SPARKLE_VER=2.6.4
SPARKLE_SHA=50612a06038abc931f16011d7903b8326a362c1074dabccb718404ce8e585f0b
VENDOR=../vendor/Sparkle
if [ ! -d $VENDOR/Sparkle.framework ]; then
  T=$(mktemp -d)
  curl -sfL "https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_VER/Sparkle-$SPARKLE_VER.tar.xz" -o $T/s.tar.xz
  echo "$SPARKLE_SHA  $T/s.tar.xz" | shasum -a 256 -c - >/dev/null || { echo "Sparkle checksum mismatch"; exit 1; }
  mkdir -p $T/x $VENDOR && tar -xf $T/s.tar.xz -C $T/x && cp -R $T/x/Sparkle.framework $T/x/bin $VENDOR/ && rm -rf $T
fi

swiftc -O -swift-version 5 -sdk "$SDK" -target arm64-apple-macos26 Sources/*.swift -o "$APP/Contents/MacOS/Sam" \
  -F $VENDOR -framework Sparkle -Xlinker -rpath -Xlinker @executable_path/../Frameworks \
  -framework AppKit -framework SwiftUI -framework AVFoundation -framework Carbon -framework Network -framework Accelerate -framework ServiceManagement
rm -rf "$APP/Contents/Frameworks/Sparkle.framework"
ditto $VENDOR/Sparkle.framework "$APP/Contents/Frameworks/Sparkle.framework"
# The logo (media/ SVGs) for the menu bar and the app, and the app icon rendered from it.
RES="$APP/Contents/Resources"
mkdir -p "$RES"
cp ../../media/logo-black.svg ../../media/logo-white.svg "$RES/"
cp ../../release/manifest.json "$RES/"   # the model manifest this version ships with
swiftc -O -sdk "$SDK" -target arm64-apple-macos14 tools/make_icon/main.swift Sources/Logo.swift -o build/make_icon
rm -rf build/AppIcon.iconset
build/make_icon build/AppIcon.iconset ../../media/logo-black.svg >/dev/null
iconutil -c icns build/AppIcon.iconset -o "$RES/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>one.sam.app</string>
<key>CFBundleName</key><string>Sam</string>
<key>CFBundleDisplayName</key><string>Sam</string>
<key>CFBundleExecutable</key><string>Sam</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
<key>CFBundleShortVersionString</key><string>$VERSION</string>
<key>CFBundleVersion</key><string>$BUILD</string>
<key>SUFeedURL</key><string>https://github.com/elijahross/Sam---Local-conversational-agent/releases/latest/download/appcast.xml</string>
<key>SamManifestURL</key><string>https://github.com/elijahross/Sam---Local-conversational-agent/releases/latest/download/manifest.json</string>
<key>SUPublicEDKey</key><string>$(cat ../../release/update-public-key.txt)</string>
<key>SUEnableAutomaticChecks</key><true/>
<key>SUScheduledCheckInterval</key><integer>86400</integer>
<key>LSMinimumSystemVersion</key><string>26.0</string>
<key>LSUIElement</key><true/>
<key>NSMicrophoneUsageDescription</key><string>Sam listens only while the island is open, so you can talk to it.</string>
<key>NSAppleEventsUsageDescription</key><string>Sam works with apps you ask it to.</string>
</dict></plist>
PL
# The same identity as the shipped app (app/sign.sh), so updates keep macOS grants.
source ../sign.sh
sign_sparkle "$APP"
sign "$APP" "$ENT/Sam.entitlements" one.sam.app
echo "built $APP"
