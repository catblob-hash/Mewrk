# Mewrk

> **Mewrk — Mew. Work.**

**English** · [简体中文](README.zh-CN.md)

Mewrk is a desktop coding agent for macOS and Windows. Like other coding agents, it reads and edits your project, runs commands, searches the web, spawns subagents and checks its own work in a browser. The difference is that everything the model sees and does stays in your hands: you can edit its context, choose the tools for each conversation, and rewrite every prompt Mewrk adds.

![The Mewrk window, with six panes open on one task](.github/assets/demo.webp)

## Get started

1. Download Mewrk from the [website](https://mewrk.dev/) or the [latest release](https://github.com/catblob-hash/Mewrk/releases/latest): macOS 13 or later on Apple silicon, or Windows 10 or 11 on x64.
2. If you use Claude Code, install the Claude Agent components under **Settings → Providers → Model providers → Claude Agent** (one click; Mewrk fetches them from npm) and Mewrk picks up its login. Otherwise open **Settings → Providers → Model providers** to sign in with ChatGPT, or add a provider with your own API key.
3. Add a folder with **New project** in the sidebar, and start a task.

The [documentation](https://mewrk.dev/en/index.html) explains every setting.

## Build from source

You need Node.js 22.12 or later and stable Rust, plus:

- **macOS 13 or later:** the Xcode Command Line Tools. Run `bash scripts/setup-macos-dev.sh` once; it checks the rest and prepares the checkout.
- **Windows:** Visual Studio Build Tools (MSVC) and [MSYS2](https://www.msys2.org) with the mingw64 `gcc`, `make` and `perl`.

```bash
npm install
npm --prefix aisdk-service install
npm run build && npm run build:sidecar
npm run tauri:dev      # run Mewrk from source
npm run tauri:build    # the installer or .dmg, in src-tauri/target/release/bundle/
```

On Linux you can work on the code and run the checks (`bash scripts/setup-linux-dev.sh` sets that up), but not build the app.

## Contribute

- Report bugs and suggest features in [Issues](https://github.com/catblob-hash/Mewrk/issues).
- Keep a pull request to one change, and open an issue first if it is a large one. Run `npm test` and `npm run test:rust` before you push.

## License

[GPL-3.0-or-later](LICENSE). Third-party components and their licenses are listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
