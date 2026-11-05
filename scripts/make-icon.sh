#!/bin/bash
# Renders crates/mewrk/assets/icon.icns from the drawings in crates/ui/common/assets, with the renderer the window draws them with.
# 16 and 32 pixels come from the small drawing, whose shapes are heavier and which has no glass or shadows to turn to mud at that size.
# The canvas keeps macOS's margin around the 824-pixel plate: macOS draws anything else as a picture on a plate of its own.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q -p fluent --example render_svg
render=target/debug/examples/render_svg
assets=crates/ui/common/assets
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
iconset="$work/icon.iconset"
mkdir "$iconset"
"$render" "$assets/mewrk-icon-small.svg" 16 "$iconset/icon_16x16.png"
"$render" "$assets/mewrk-icon-small.svg" 32 "$iconset/icon_16x16@2x.png"
"$render" "$assets/mewrk-icon-small.svg" 32 "$iconset/icon_32x32.png"
"$render" "$assets/mewrk-icon.svg" 64 "$iconset/icon_32x32@2x.png"
for size in 128 256 512; do
    "$render" "$assets/mewrk-icon.svg" "$size" "$iconset/icon_${size}x${size}.png"
    "$render" "$assets/mewrk-icon.svg" "$((size * 2))" "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o crates/mewrk/assets/icon.icns
ls -l crates/mewrk/assets/icon.icns
