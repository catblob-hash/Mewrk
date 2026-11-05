#!/bin/bash
# Builds target/dist/Mewrk-<version>-x64-setup.exe for Windows, with SHA256SUMS beside it. Run it in Git Bash on an x64 Windows host with the
# MSVC toolchain. The installer is built by scripts/windows/installer.nsi with makensis, from $MAKENSIS (NSIS 3 and its default location
# below). Nothing here is signed: Authenticode needs a certificate this script does not have.
set -euo pipefail
cd "$(dirname "$0")/.."

triple="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$triple" != "x86_64-pc-windows-msvc" ]; then
    echo "package-windows: this builds for x86_64-pc-windows-msvc, and rustc runs on $triple" >&2
    exit 1
fi
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
python="${PYTHON:-python}"
makensis="${MAKENSIS:-$(command -v makensis || echo "/c/Program Files (x86)/NSIS/makensis.exe")}"
if [ ! -x "$makensis" ]; then
    echo "package-windows: no makensis at $makensis (set MAKENSIS to the NSIS 3 compiler)" >&2
    exit 1
fi
for file in crates/mewrk/assets/icon.ico scripts/windows/installer.nsi LICENSE THIRD-PARTY-NOTICES.md; do
    [ -f "$file" ] || { echo "package-windows: $file is missing" >&2; exit 1; }
done

# Replaces the target-cpu of .cargo/config.toml, which may name this machine's chip. x86-64-v2 runs on every x64 PC since about 2009.
# +crt-static links the C runtime into the binaries, so the install needs no Visual C++ redistributable. The C code that cc builds for the
# dependencies follows the same flag. If a dependency ever refuses the static runtime, drop that flag: the installer still works without it.
#
# The prefixes keep this machine's paths out of panic messages and debug info. rustc matches a prefix as the text cargo gave it, which is
# sometimes forward slashes and sometimes backslashes on Windows, so each prefix is mapped in both spellings.
flags="-C target-cpu=x86-64-v2 -C target-feature=+crt-static"
add_remap() {
    local forward="$1" backward="${1//\//\\}"
    flags+=" --remap-path-prefix=$forward=$2 --remap-path-prefix=$backward=$2"
}
add_remap "$(cygpath -m "${CARGO_HOME:-$HOME/.cargo}")/registry/src" /cargo
add_remap "$(cygpath -m "${RUSTUP_HOME:-$HOME/.rustup}")" /rustup
add_remap "$(cygpath -m "$PWD")" /mewrk
export RUSTFLAGS="$flags"

# The license list is generated from Cargo.lock (see scripts/third-party-licenses.py), as the macOS release does.
"$python" scripts/third-party-licenses.py

cargo build --locked --release -p mewrk
# By itself: built together, the agent's features would reach the host, which links only its client half (see bundle-macos.sh).
cargo build --locked --release -p mewrk-remote-agent --bin mewrk-remote

out=target/dist
stage=target/dist/stage
name="Mewrk-$version-x64-setup.exe"
rm -rf target/dist
mkdir -p "$stage/remote-agents/$triple"
cp target/release/mewrk.exe "$stage/mewrk.exe"
cp target/release/mewrk-remote.exe "$stage/remote-agents/$triple/mewrk-remote.exe"
cp LICENSE THIRD-PARTY-NOTICES.md THIRD-PARTY-LICENSES.md "$stage/"

# Git Bash would rewrite the /D options and the paths, so it is turned off for makensis alone. The script takes Windows paths.
MSYS_NO_PATHCONV=1 "$makensis" /V2 \
    "/DVERSION=$version" \
    "/DSTAGE=$(cygpath -wa "$stage")" \
    "/DOUTFILE=$(cygpath -wa "$out/$name")" \
    "$(cygpath -wa scripts/windows/installer.nsi)"
[ -f "$out/$name" ] || { echo "package-windows: makensis wrote no $out/$name" >&2; exit 1; }

rm -rf target/dist/stage
(cd "$out" && sha256sum --text "$name" > SHA256SUMS)
du -h "$out/$name"
echo "$out/$name"
