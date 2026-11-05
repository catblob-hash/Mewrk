#!/bin/bash
# Builds the DMG for other people's Macs: target/dist/Mewrk-<version>.dmg, with SHA256SUMS beside it. The app inside is notarized and stapled first
# so that it opens offline, and then so is the DMG around it. Needs $APPLE_SIGNING_IDENTITY, a Developer ID, and `notarize` with its Keychain profile.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/bin:$PATH"
identity="${APPLE_SIGNING_IDENTITY:?a release has to be signed: set APPLE_SIGNING_IDENTITY to a Developer ID}"
plist=crates/mewrk/assets/Info.plist
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist")" != "$version" ]; then
    echo "release-macos: $plist doesn't say $version, which Cargo.toml does" >&2
    exit 1
fi

# Replaces the target-cpu=native of .cargo/config.toml, which would crash on any chip older than this Mac's. The prefixes keep this Mac's paths out of
# panic messages.
export RUSTFLAGS="-C target-cpu=apple-m1 --remap-path-prefix=$HOME/.cargo/registry/src=/cargo --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$PWD=/mewrk"
export MACOSX_DEPLOYMENT_TARGET="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$plist")"

scripts/third-party-licenses.py
scripts/bundle-macos.sh "$identity"
notarize target/release/Mewrk.app

out=target/dist
stage="$out/stage"
name="Mewrk-$version.dmg"
rm -rf "$out"
mkdir -p "$stage"
ditto target/release/Mewrk.app "$stage/Mewrk.app"
ln -s /Applications "$stage/Applications"
hdiutil create -volname Mewrk -format ULMO -srcfolder "$stage" "$out/$name"
rm -rf "$stage"
codesign --force --timestamp --sign "$identity" "$out/$name"
notarize "$out/$name"
(cd "$out" && shasum -a 256 "$name" > SHA256SUMS)
du -sh "$out/$name"
echo "$out/$name"
