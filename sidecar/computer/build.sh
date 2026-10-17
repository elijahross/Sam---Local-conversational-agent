#!/bin/zsh
# Builds "Sam Computer Use.app" (menu-bar-less agent app). macOS ties Accessibility /
# Screen Recording grants to this app's signature: rebuilding with an ad-hoc signature means
# granting again, so only rebuild when the code changes.
set -e
cd "$(dirname $0)"
APP="build/Sam Computer Use.app"
mkdir -p "$APP/Contents/MacOS"
SDK=$(ls -d /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk 2>/dev/null | head -1)
SDK=${SDK:-$(xcrun --sdk macosx --show-sdk-path)}   # CI: Xcode's SDK
swiftc -O -sdk "$SDK" -target arm64-apple-macos14 main.swift -o "$APP/Contents/MacOS/sam-computer"
cat > "$APP/Contents/Info.plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>one.sam.computeruse</string>
<key>CFBundleName</key><string>Sam Computer Use</string>
<key>CFBundleExecutable</key><string>sam-computer</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1</string>
<key>LSUIElement</key><true/>
<key>NSAppleEventsUsageDescription</key><string>Sam controls apps you ask it to.</string>
</dict></plist>
PL
# Stable local identity (signing.sh) keeps macOS permission grants across rebuilds.
ID=$(security find-identity -p codesigning | grep -E "(Sam|Rowan) Local Code Signing" | head -1 | awk '{print $2}')
codesign --force --sign "${ID:--}" --identifier one.sam.computeruse "$APP"
codesign -d -r- "$APP" 2>&1 | grep designated
echo "built $APP"
