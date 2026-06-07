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

# GoopieWebsite reads its Firebase project config from VITE_FIREBASE_* at
# build time (see src/app/firebase.ts) and calls initializeApp()/getAuth()
# unconditionally on load — without these, apiKey is undefined and the app
# crashes to a blank page with `auth/invalid-api-key` before it can render
# anything, even offline UI. These are Firebase Web *config* values, not
# secrets — they're already public in the live site's deployed JS bundle
# (Firebase's security model relies on Firestore/Storage rules, not on
# hiding them), so it's safe to bake them in here for the embedded copy.
export VITE_FIREBASE_API_KEY="${VITE_FIREBASE_API_KEY:-AIzaSyCUbUOOC-Jkb51XmLFGJIzbHxaw-EUgZm0}"
export VITE_FIREBASE_AUTH_DOMAIN="${VITE_FIREBASE_AUTH_DOMAIN:-goopie-f3ef6.firebaseapp.com}"
export VITE_FIREBASE_PROJECT_ID="${VITE_FIREBASE_PROJECT_ID:-goopie-f3ef6}"
export VITE_FIREBASE_STORAGE_BUCKET="${VITE_FIREBASE_STORAGE_BUCKET:-goopie-f3ef6.firebasestorage.app}"
export VITE_FIREBASE_MESSAGING_SENDER_ID="${VITE_FIREBASE_MESSAGING_SENDER_ID:-514568294125}"
export VITE_FIREBASE_APP_ID="${VITE_FIREBASE_APP_ID:-1:514568294125:web:4c5064c3dc062c7ce086e4}"

echo "Building GoopieWebsite static bundle (using ${INSTALL[0]})..."
(cd "$SITE_SRC" && "${INSTALL[@]}" && "${BUILD[@]}")

echo "Copying $SITE_DIST -> $SITE_DEST"
rm -rf "$SITE_DEST"
mkdir -p "$SITE_DEST"
cp -r "$SITE_DIST"/. "$SITE_DEST"/

echo "Offline site bundle ready at $SITE_DEST"
