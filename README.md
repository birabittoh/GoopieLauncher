# Goopie Launcher

A lightweight, native desktop launcher for [Goopie](https://goopie.xyz) — a platform for recompiled Xbox 360 games.

This is a complete rewrite of the legacy [`SFML-CEF-Rexglue-Launcher`](https://github.com/SolarCookies/SFML-CEF-Rexglue-Launcher) in **Rust + [Tauri v2](https://v2.tauri.app)**. It uses the OS's native webview (WebKitGTK on Linux, WKWebView on macOS, WebView2 on Windows) instead of bundling Chromium, resulting in a dramatically smaller binary.

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

> [!NOTE]
> `build.devUrl` in `tauri.conf.json` points at `https://goopie.xyz` (so plain
> `cargo tauri dev` loads the live site). Without internet access, the Tauri
> CLI itself panics trying to resolve that URL's DNS to "wait for the dev
> server" — unrelated to this launcher's own offline mode, since that crash
> happens before the app is even built/launched. Work around it with:
> ```bash
> cargo tauri dev --no-dev-server-wait
> ```

### Production build

```bash
cargo tauri build
```

Bundles are output to `src-tauri/target/release/bundle/`.

## URL selection

The launcher URL is resolved in this priority order:

1. `--url <URL>` CLI argument
2. `--local` CLI flag → `http://localhost:5173/`
3. `GOOPIE_URL` environment variable
4. If the user has explicitly enabled offline mode (see [Offline mode](#offline-mode)),
   that's honored unconditionally — no connectivity probe, no requests to
   `goopie.xyz` at all
5. Otherwise (the user prefers online): probe `https://goopie.xyz`'s
   reachability on this launch and fall back to the embedded offline bundle if
   it can't be reached

Options 1–3 are dev overrides and bypass offline-mode resolution entirely.

Examples:

```bash
./goopie-launcher --local           # Use local Vite dev server
GOOPIE_URL=http://localhost:8080 ./goopie-launcher
./goopie-launcher --url https://staging.goopie.xyz
```

## Offline mode

The launcher ships an embedded, statically-served copy of GoopieWebsite
(vendored as the `GoopieWebsite` git submodule, pinned to its `tauri-support`
branch) with enough functionality to browse and launch installed games.

Clone with submodules, or run `git submodule update --init` afterwards:

```bash
git clone --recurse-submodules <repo-url>
```

To (re)build the embedded offline bundle from the submodule:

```bash
./scripts/build-offline-site.sh
```

This runs automatically as part of `cargo tauri build` (via `beforeBuildCommand`),
but not for plain `cargo build`/`cargo tauri dev` — a placeholder page ships in
`src-tauri/offline-site/` so those keep working without a Node/bun toolchain.

## Configuration

| Platform | Location |
|----------|----------|
| Windows  | Registry `HKCU\Software\GoopieLauncher` |
| Linux    | `~/.config/GoopieLauncher/config.ini` |

Games are stored under:
- Windows: `%LOCALAPPDATA%\Goopie\Games\`
- Linux: `~/.local/share/Goopie/Games/`

These paths match the legacy C++ launcher so existing installations are automatically found.
