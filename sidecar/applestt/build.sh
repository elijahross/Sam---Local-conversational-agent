#!/bin/zsh
# Builds applestt (Apple on-device speech recognition CLI, macOS 26+).
set -e
cd "$(dirname $0)"
mkdir -p build
SDK=$(ls -d /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk 2>/dev/null | head -1)
SDK=${SDK:-$(xcrun --sdk macosx --show-sdk-path)}   # CI: Xcode's SDK
swiftc -O -sdk "$SDK" -target arm64-apple-macos26 main.swift -o build/applestt
echo "built build/applestt"
