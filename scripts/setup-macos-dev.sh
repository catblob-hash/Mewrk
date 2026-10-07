#!/usr/bin/env bash
# Prepares a Mac for Mewrk development and local builds.
#
# Everything in the inner loop runs on a Mac — Biome, tsc, Vitest, the
# `node --test` check scripts, `cargo check` / `cargo test` across the Rust
# workspace — and so does `npm run tauri:build`, which bundles `Mewrk.app` and
# a `.dmg` from `src-tauri/tauri.macos.conf.json`. What does not run here is
# `npm run prob:fetch` and the formal verification it feeds: the ProB
# artifacts it pins are Windows builds. Run it from a fresh clone:
#
#   bash scripts/setup-macos-dev.sh
#
# It is idempotent, needs no root, and installs no toolchain behind your back:
# a missing prerequisite is reported with the command that installs it. The
# window and bundle icons need no step — `src-tauri/build.rs` renders them —
# and neither does Chrome for the browser-driven tests, which look in
# /Applications on their own. `--seed-agent-config` additionally writes the
# untracked agent-client files `npm test` asserts on; see
# scripts/seed-agent-config.sh for why that is opt-in.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
seed_agent_config=0
for argument in "$@"; do
  case "$argument" in
    --seed-agent-config) seed_agent_config=1 ;;
    *) echo "unknown argument: $argument" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "setup-macos-dev.sh targets macOS; on Linux run scripts/setup-linux-dev.sh." >&2
  exit 1
fi

step() { printf '\n[setup] %s\n' "$1"; }

# --- toolchain ---------------------------------------------------------------
step "toolchain"
missing=0
if ! xcode-select -p >/dev/null 2>&1; then
  echo "  Xcode Command Line Tools (clang, git, codesign) are missing: xcode-select --install" >&2
  missing=1
fi
if ! command -v node >/dev/null 2>&1; then
  echo "  Node.js >= 22.12 is missing: install it from https://nodejs.org" >&2
  missing=1
elif ! node -e 'const [a, b] = process.versions.node.split(".").map(Number); process.exit(a > 22 || (a === 22 && b >= 12) ? 0 : 1)'; then
  echo "  Node.js $(node --version) is older than the 22.12 package.json requires" >&2
  missing=1
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "  Rust is missing: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  missing=1
fi
if [ "$missing" -ne 0 ]; then
  exit 1
fi
echo "  $(clang --version | head -n 1)"
echo "  node $(node --version), $(cargo --version)"

# The AI SDK sidecar is a copy of this very `node` with the bundle injected
# (Node's single-executable applications). The nodejs.org build links only
# system libraries; Homebrew's links its own, so a sidecar built from it only
# runs on a Mac with the same Homebrew kegs installed.
node_binary=$(node -p process.execPath)
if otool -L "$node_binary" | grep -qE '/(opt/homebrew|usr/local/(opt|Cellar))/'; then
  echo "  warning: $node_binary links Homebrew libraries; a sidecar built from it" >&2
  echo "  runs on this Mac only. Use the nodejs.org build for anything you ship." >&2
fi

# `npm run tauri:build` carries only this Mac's own remote agent (its
# `beforeBuildCommand` runs `build:remote-agents -- --bundle`), and needs no
# other target. The remote agent Mewrk installs on SSH machines, WSL and other
# platforms is fetched from Mewrk's component channel, where a release puts it:
# `npm run build:remote-agents -- --release` cross-compiles it here for every
# platform (scripts/build-remote-agents.mjs) and `npm run publish:components`
# uploads it. The Linux builds link with the rust-lld rustup ships and need
# nothing but their targets; Windows ones need cargo-xwin. Without the targets a
# development build still builds the one agent a machine needs, when it can.
missing_targets=""
installed_targets=$(rustup target list --installed 2>/dev/null || true)
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
  printf '%s\n' "$installed_targets" | grep -qx "$target" || missing_targets="$missing_targets $target"
done
if [ -n "$missing_targets" ]; then
  echo "  remote agent targets missing (a development build builds the one a machine needs when it can):" >&2
  echo "    rustup target add$missing_targets" >&2
fi
if ! cargo xwin --version >/dev/null 2>&1; then
  echo "  cargo-xwin is missing, so no Windows agent builds here: cargo install cargo-xwin" >&2
fi

# --- javascript dependencies -------------------------------------------------
step "npm dependencies (root)"
(cd "$repo_root" && npm ci)
step "npm dependencies (aisdk-service)"
(cd "$repo_root/aisdk-service" && npm ci)

# --- build artifacts the Rust build and a development run need ---------------
step "frontend bundle (tauri.conf.json frontendDist)"
(cd "$repo_root" && npm run build >/dev/null)

# A development build runs this sidecar straight from aisdk-service/dist; an
# installed Mewrk fetches its own from the component channel instead, and a
# release publishes this one there (`npm run publish:components -- --aisdk`).
step "aisdk sidecar"
if [ ! -f "$repo_root/aisdk-service/dist/mewrk-aisdk" ]; then
  (cd "$repo_root" && npm run build:sidecar)
else
  echo "  present; npm run build:sidecar rebuilds it"
fi

# --- agent-client configuration ----------------------------------------------
step "agent-client configuration"
if [ "$seed_agent_config" -eq 1 ]; then
  bash "$repo_root/scripts/seed-agent-config.sh" --write
else
  bash "$repo_root/scripts/seed-agent-config.sh"
fi

step "verifying"
(cd "$repo_root/src-tauri" && cargo check --workspace --all-targets)

cat <<'EOF'

[setup] done.

  npm test                    # lint, check scripts, Vitest
  cd src-tauri && cargo test  # Rust workspace
  npm run tauri:dev           # the app, against the Vite dev server
  npm run tauri:build         # Mewrk.app and a .dmg under src-tauri/target/release/bundle/

Not available on macOS: `npm run prob:fetch` (the ProB toolchain pins Windows
artifacts), `npm run package:portable` and the NSIS installer.
EOF
