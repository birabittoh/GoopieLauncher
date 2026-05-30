# Goopie Launcher

A lightweight, native desktop launcher for [Goopie](https://goopie.xyz) — a platform for recompiled Xbox 360 games.

This is a complete rewrite of the legacy [`SFML-CEF-Rexglue-Launcher`](../SFML-CEF-Rexglue-Launcher) in **Rust + [Tauri v2](https://v2.tauri.app)**. It uses the OS's native webview (WebKitGTK on Linux, WKWebView on macOS, WebView2 on Windows) instead of bundling Chromium, resulting in a dramatically smaller binary.

## Features

- **Lightweight** — no bundled browser, relies on the system webview
- **ISO extraction** — extract Xbox 360 game images using the pure-Rust `xdvdfs` crate
- **GitHub releases** — download and update recompiled game executables with SHA-256 verification
- **Save management** — backup, restore, rename and delete save slots
- **Vehicle browser** — parse and display Nuts & Bolts vehicle saves
- **Cross-platform** — Windows, Linux (macOS untested but should work)

## Architecture

The launcher loads the live UI from `https://goopie.xyz` and bridges native functionality via a **synchronous loopback HTTP server** on `127.0.0.1:<random port>`. At startup, a JS initialisation script is injected (before the page runs) that defines every `window.*` global as a synchronous `XMLHttpRequest` to the bridge server.

This approach keeps the website completely unchanged while providing the same synchronous call semantics the site expects.

### Security

The bridge token is randomly generated at launch and embedded in every request URL. Other local processes cannot make bridge calls without knowing the token.

> **Note:** If goopie.xyz ever adds a restrictive `connect-src` CSP header in the future, you would need to either ask for the loopback origin to be whitelisted or switch to bundling the website.

## Building

### Prerequisites

```bash
# Rust (stable)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Tauri CLI
cargo install tauri-cli --version "^2"

# Linux: system libraries
sudo apt-get install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf build-essential libssl-dev pkg-config
```

### Development

```bash
cargo tauri dev
# or to use the local Vite dev server:
cargo tauri dev -- -- --local
```

### Production build

```bash
cargo tauri build
```

Bundles are output to `src-tauri/target/release/bundle/`.

## URL selection

The launcher URL is resolved in this priority order (no auto-detection):

1. `--url <URL>` CLI argument
2. `--local` CLI flag → `http://localhost:5173/`
3. `GOOPIE_URL` environment variable
4. Default: `https://goopie.xyz`

Examples:

```bash
./goopie-launcher --local           # Use local Vite dev server
GOOPIE_URL=http://localhost:8080 ./goopie-launcher
./goopie-launcher --url https://staging.goopie.xyz
```

## Configuration

| Platform | Location |
|----------|----------|
| Windows  | Registry `HKCU\Software\GoopieLauncher` |
| Linux    | `~/.config/GoopieLauncher/config.ini` |

Games are stored under:
- Windows: `%LOCALAPPDATA%\Goopie\Games\`
- Linux: `~/.local/share/Goopie/Games/`

These paths match the legacy C++ launcher so existing installations are automatically found.

## CI / Release

GitHub Actions workflows mirror the naming convention of the legacy launcher:

| File | Trigger | Purpose |
|------|---------|---------|
| `ci.yml` | Push/PR (non-release) | Build both platforms |
| `_build.yml` | Called by ci/release | Reusable build job |
| `release.yml` | `v*` tag push | Build + create GitHub Release |
