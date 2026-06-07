#!/usr/bin/env bash
# Build the GoopieWebsite submodule and copy its static output into
# src-tauri/offline-site/, where it is embedded into the launcher binary
# (see src-tauri/src/offline_site.rs) and served when the launcher is offline.
#
# Run manually after pulling submodule updates, or automatically via
# tauri.conf.json's `beforeBuildCommand` on `cargo tauri build`.
set -euo pipefail

cd "$(dirname "$0")/.."
SITE_SRC="GoopieWebsite"
SITE_DIST="$SITE_SRC/dist"
SITE_DEST="src-tauri/offline-site"

if [ ! -d "$SITE_SRC/src" ]; then
  echo "GoopieWebsite submodule is not checked out. Run: git submodule update --init" >&2
  exit 1
fi

# Prefer bun (faster, what local dev machines have); fall back to npm (what
# CI runners ship by default) — use whichever is actually installed.
if command -v bun >/dev/null 2>&1; then
  INSTALL=(bun install)
  BUILD=(bun x vite build)
elif command -v npm >/dev/null 2>&1; then
  INSTALL=(npm install)
  BUILD=(npx vite build)
else
  echo "Neither bun nor npm found on PATH — cannot build the offline site bundle." >&2
  exit 1
fi

echo "Building GoopieWebsite static bundle (using ${INSTALL[0]})..."
(cd "$SITE_SRC" && "${INSTALL[@]}" && "${BUILD[@]}")

echo "Copying $SITE_DIST -> $SITE_DEST"
rm -rf "$SITE_DEST"
mkdir -p "$SITE_DEST"
cp -r "$SITE_DIST"/. "$SITE_DEST"/

echo "Offline site bundle ready at $SITE_DEST"
