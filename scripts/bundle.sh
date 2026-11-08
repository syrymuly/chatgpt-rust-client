#!/bin/sh
# Builds dist/Patina.app and dist/Patina-<version>.dmg.
# Usage: scripts/bundle.sh            (this Mac's architecture)
#        scripts/bundle.sh --universal (Apple silicon + Intel)
set -eu
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
EXE=chatgpt-rust-client

if [ "${1:-}" = "--universal" ]; then
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  BIN=target/$EXE-universal
  lipo -create -output "$BIN" \
    "target/aarch64-apple-darwin/release/$EXE" \
    "target/x86_64-apple-darwin/release/$EXE"
else
  cargo build --release
  BIN=target/release/$EXE
fi

APP=dist/Patina.app
rm -rf dist
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" dist/dmg
cp "$BIN" "$APP/Contents/MacOS/Patina"
cp assets/Patina.icns "$APP/Contents/Resources/Patina.icns"
sed "s/@VERSION@/$VERSION/g" assets/Info.plist > "$APP/Contents/Info.plist"

# Ad-hoc signature: required on Apple silicon. Not notarized.
codesign --force --sign - "$APP"

cp -R "$APP" dist/dmg/
ln -s /Applications dist/dmg/Applications
hdiutil create -quiet -volname Patina -srcfolder dist/dmg -ov -format UDZO "dist/Patina-$VERSION.dmg"
rm -rf dist/dmg
echo "dist/Patina-$VERSION.dmg"
