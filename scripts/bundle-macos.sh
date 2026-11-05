#!/bin/bash
# Builds target/release/Mewrk.app, with Chromium in it for the preview pane. It signs with the identity in $1 or else
# $APPLE_SIGNING_IDENTITY, with the hardened runtime and the entitlements each of Chromium's processes needs. With neither, the
# signature is ad hoc: enough to run here and to keep the Keychain from asking again after every launch of the same build.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p mewrk -p mewrk-cef-helper
app=target/release/Mewrk.app
identity="${1:-${APPLE_SIGNING_IDENTITY:--}}"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp target/release/mewrk "$app/Contents/MacOS/mewrk"
cp crates/mewrk/assets/Info.plist "$app/Contents/Info.plist"
cp crates/mewrk/assets/icon.icns "$app/Contents/Resources/icon.icns"
# The privacy prompts in both languages, since every program Mewrk starts asks as Mewrk.
cp -R crates/mewrk/assets/InfoPlist/. "$app/Contents/Resources/"

# The CEF build the crate compiled against: cef-dll-sys `154.0.0+154.0.23` is CEF 154.0.23.
cef_version="$(grep -A1 '^name = "cef-dll-sys"$' Cargo.lock | sed -n 's/^version = "[^+]*+\(.*\)"$/\1/p')"
distribution=".cef/$cef_version/cef_macos_aarch64"
framework="Chromium Embedded Framework.framework"
# ditto keeps the framework's symlinks and extended attributes exactly as distributed.
ditto "$distribution/$framework" "$app/Contents/Frameworks/$framework"
cp "$distribution/CREDITS.html" "$app/Contents/Resources/CHROMIUM-CREDITS.html"

sign() {
    if [ "$identity" = "-" ]; then
        codesign --force --sign - "$1"
    elif [ -n "${2:-}" ]; then
        codesign --force --timestamp --options runtime --entitlements "$2" --sign "$identity" "$1"
    else
        codesign --force --timestamp --options runtime --sign "$identity" "$1"
    fi
}

for library in "$app/Contents/Frameworks/$framework/Libraries/"*.dylib; do
    sign "$library"
done
sign "$app/Contents/Frameworks/$framework"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' crates/mewrk/assets/Info.plist)"
entitlements_dir="$(mktemp -d)"
trap 'rm -rf "$entitlements_dir"' EXIT

# Chromium picks a helper by suffix, and each process type runs under its own entitlements.
helper() {
    local suffix="$1" id="$2"
    shift 2
    local name="Mewrk Helper$suffix"
    local bundle="$app/Contents/Frameworks/$name.app"
    mkdir -p "$bundle/Contents/MacOS"
    cp target/release/mewrk-cef-helper "$bundle/Contents/MacOS/$name"
    chmod 755 "$bundle/Contents/MacOS/$name"
    cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleExecutable</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$identifier.$id</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSEnvironment</key>
  <dict><key>MallocNanoZone</key><string>0</string></dict>
  <key>LSFileQuarantineEnabled</key><true/>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><string>1</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
    local entitlements="$entitlements_dir/$id.plist"
    {
        echo '<?xml version="1.0" encoding="UTF-8"?>'
        echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
        echo '<plist version="1.0"><dict>'
        for key in "$@"; do
            echo "  <key>$key</key><true/>"
        done
        echo '</dict></plist>'
    } > "$entitlements"
    sign "$bundle" "$entitlements"
}

helper "" helper
helper " (GPU)" helper.gpu com.apple.security.cs.allow-jit
helper " (Renderer)" helper.renderer com.apple.security.cs.allow-jit
helper " (Plugin)" helper.plugin com.apple.security.cs.allow-unsigned-executable-memory com.apple.security.cs.disable-library-validation
helper " (Alerts)" helper.alerts

sign "$app" crates/mewrk/assets/Entitlements.plist
du -sh "$app"
echo "$app"
