#!/usr/bin/env bash
# Run from inside the Dockerfile.appimage-test container, with the repo
# mounted at /work. Reproduces the "Build (Linux)" + "Inject libjpeg.so.8"
# steps from .github/workflows/_build.yml to test them without a real CI run.
set -eux

cd /work

export VERSION="test"
export GOOPIE_OAUTH_CLIENT_ID="test"
export GOOPIE_OAUTH_CLIENT_SECRET="test"
export GOOPIE_RELEASES_API=""
export GOOPIE_DISCORD_CLIENT_ID=""
export TAURI_BUNDLER_NEW_APPIMAGE_FORMAT="true"

script -q -c "cargo tauri build --bundles appimage" /dev/null

# Mirrors the "Inject libjpeg.so.8 into AppImage" step in
# .github/workflows/_build.yml — keep in sync with that step.
APPIMAGE="$(ls src-tauri/target/release/bundle/appimage/*.AppImage | head -1)"
cd "$(dirname "$APPIMAGE")"
APPIMAGE_NAME="$(basename "$APPIMAGE")"
chmod +x "./$APPIMAGE_NAME"

"./$APPIMAGE_NAME" --appimage-extract

LIBDIR="$(dirname "$(find squashfs-root -name 'libwebkit2gtk-4.1.so*' | head -1)")"
mkdir -p "$LIBDIR"
cp /usr/lib/x86_64-linux-gnu/libjpeg.so.8 "$LIBDIR/"

ARCH="$(uname -m)"
APPIMAGETOOL="/tmp/appimagetool"
if [ ! -x "$APPIMAGETOOL" ]; then
  curl -Lso "$APPIMAGETOOL" "https://github.com/pkgforge-dev/appimagetool/releases/latest/download/appimagetool-${ARCH}-linux"
  chmod +x "$APPIMAGETOOL"
fi

rm -f "./$APPIMAGE_NAME"
"$APPIMAGETOOL" squashfs-root -o . -n "$APPIMAGE_NAME"
chmod +x "./$APPIMAGE_NAME"

echo "=== Verifying repacked AppImage ==="
ls -la "./$APPIMAGE_NAME"
"./$APPIMAGE_NAME" --appimage-extract libjpeg.so.8 2>&1 || true
find squashfs-root -name 'libjpeg*' 2>&1 || true
rm -rf squashfs-root/
echo "=== Done ==="
