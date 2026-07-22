# Goopie Launcher

A lightweight, native desktop launcher for [Goopie](https://goopie.xyz).

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

Then, clone:
```bash
git clone --recurse-submodules https://github.com/birabittoh/GoopieLauncher
cd GoopieLauncher
```

### Development

```bash
npm run dev
```

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

## Offline mode

The launcher ships an embedded, statically-served copy of GoopieWebsite
(vendored as the `GoopieWebsite` git submodule, pinned to its `tauri-support`
branch) with enough functionality to browse and launch installed games.


Build the embedded offline bundle from the submodule:

```bash
./scripts/build-offline-site.sh
```

Start the launcher in offline mode by using `--offline`.

## Configuration

| Platform | Location |
|----------|----------|
| Windows  | Registry `HKCU\Software\GoopieLauncher` |
| Linux    | `~/.config/GoopieLauncher/config.ini` |

Games are stored under:
- Windows: `%LOCALAPPDATA%\Goopie\Games\`
- Linux: `~/.local/share/Goopie/Games/`
