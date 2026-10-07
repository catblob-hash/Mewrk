#!/bin/sh
# Cargo runner for macOS development runs, installed by scripts/macos-dev-signing.mjs only
# when MEWRK_DEV_SIGNING_IDENTITY names a code-signing identity in the login keychain.
#
# The linker gives every build a fresh ad-hoc signature, and macOS ties Keychain access
# ("Always Allow" on "Mewrk Safe Storage") and privacy grants (Desktop, Documents, Local
# Network, ...) to that signature, so each rebuild is asked about again. Re-signing the
# application binaries with one stable identity and one identifier before they start keeps
# those answers across rebuilds. Every other binary Cargo runs (tests, build helpers) is
# passed through untouched.
binary=$1
shift
case "$(basename "$binary")" in
  mewrk | mewrk-browser-dev)
    if [ -n "$MEWRK_DEV_SIGNING_IDENTITY" ] \
      && ! codesign --force --sign "$MEWRK_DEV_SIGNING_IDENTITY" --identifier com.mewrk.app "$binary"; then
      echo "[dev-signing] 无法用「$MEWRK_DEV_SIGNING_IDENTITY」签名 $binary，继续使用临时签名启动" >&2
    fi
    # The application hands its environment to every shell and tool it runs; a `cargo run`
    # in another project must not come back through this runner.
    unset CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER CARGO_TARGET_X86_64_APPLE_DARWIN_RUNNER \
      MEWRK_DEV_SIGNING_IDENTITY
    ;;
esac
exec "$binary" "$@"
