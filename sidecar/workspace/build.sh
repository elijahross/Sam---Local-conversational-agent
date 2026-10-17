#!/bin/zsh
# Builds the Sam Workspace display helper (virtual display for Sam's own browser windows).
set -e
cd "$(dirname $0)"
mkdir -p build
clang -fobjc-arc -O2 workspace.m -o build/sam-workspace -framework Foundation -framework AppKit -framework CoreGraphics
echo "built build/sam-workspace"
