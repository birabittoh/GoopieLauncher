## Resolution

The post-build injection step (extract → add lib → repack) was on the right
track, but it dropped `libjpeg.so.8` into a flat `usr/lib/`, not the
multiarch subdirectory (`usr/lib/x86_64-linux-gnu/`) where sharun actually
put every other WebKit dependency. sharun's `LD_LIBRARY_PATH` only covers the
directories it populated during bundling, so the injected library sat outside
it and the WebKit subprocesses still couldn't find it — this is why the
Steam Deck testing kept failing even after the injection commit landed.

Fix: locate the directory sharun already put `libwebkit2gtk-4.1.so*` in
(inside the extracted squashfs) and copy `libjpeg.so.8` there instead of a
hardcoded `usr/lib`. See `.github/workflows/_build.yml`, "Inject libjpeg.so.8
into AppImage".

# libjpeg.so.8 missing from sharun-based AppImage

## The problem

The new sharun-based AppImage bundler (Tauri PR #12491) produces an AppImage
whose WebKit subprocesses (`WebKitNetworkProcess`, `WebKitWebProcess`) fail
with:

```
error while loading shared libraries: libjpeg.so.8: cannot open shared object file: No such file or directory
```

The main binary works fine — only the WebKit subprocesses are affected.

## Build environment

- OS: `ubuntu-24.04` (GitHub Actions)
- Bundler: `quick-sharun.sh` from pkgforge-dev/Anylinux-AppImages
- Runtime: sharun-based (NOT appimagetool/linuxdeploy)
- Extra apt packages installed: `libjpeg-turbo8`, `xvfb`

## What was tried (and why it didn't work)

### 1. Install libjpeg-turbo8 on the build system

The library is present on the build system and `ldd` on the WebKit processes
finds it there. The issue is that `quick-sharun.sh` either does not detect it
or does not bundle it correctly.

### 2. Wrap build with `script -q -c` (provide a pseudo-TTY)

Fixes `set -m` (job control) hangs — quick-sharun needs a TTY for its strace
mode process-group management — but does not help with library detection.

### 3. Install xvfb (virtual display for strace mode)

quick-sharun's strace mode detects `xvfb-run` and uses it to give traced
binaries a virtual display. This helps WebKit processes start, but they still
do not trigger JPEG loading (no actual JPEG is processed during the strace
window), so `libjpeg.so.8` remains undetected by `LD_DEBUG=libs`.

### 4. `LD_PRELOAD=libjpeg.so.8` in the build env

The idea was to force-load libjpeg at startup so the `LD_DEBUG=libs` trace
would detect it. Did not work — either the env var did not propagate through
the `script` → `xvfb-run` → `env` chain, or the dynamic linker silently
skipped the soname (which it does when not found).

Note: using the full path like `/usr/lib/x86_64-linux-gnu/libjpeg.so.8`
would be architecture-specific (breaks on aarch64) but was not tested.

## Likely root cause

The WebKit subprocess binaries (`WebKitNetworkProcess`, `WebKitWebProcess`)
have `libjpeg.so.8` as a DT_NEEDED entry (i.e., it shows up in `ldd` output).
They ARE deployed by quick-sharun (discovered via `$WEBKIT2GTK_DIR` → `ADD_DIR`
→ `_lib4bin_main`). The `_lib4bin_collect_ldd` function should pick up
`libjpeg.so.8` from their ldd output and deploy it.

Possible failure modes:

1. **`ldd` on the WebKit binaries does find libjpeg but the path is filtered
   out** — check the awk/sed pipeline in `_lib4bin_ldd_libs` and
   `_lib4bin_deploy_shared_libs`.

2. **The library IS deployed but the WebKit subprocesses cannot find it** —
   sharun's `LD_LIBRARY_PATH` rewriting might not cover the subprocess scope,
   or an RPATH in the WebKit binaries points to an absolute host path.

3. **The library is a symlink (libjpeg.so.8 → libjpeg.so.8.3.0) and the
   symlink handling in `_lib4bin_deploy_shared_libs` produces a broken
   link in the AppDir.**

4. **A stale or buggy version of quick-sharun.sh is being downloaded**
   (the URL uses `refs/heads/main`, not a pinned commit).

## What the next agent should do

1. **Confirm whether libjpeg.so.8 is actually inside the built AppImage:**
   ```
   ./Goopie-Launcher.AppImage --appimage-extract
   find squashfs-root -name 'libjpeg*'
   ```

2. **Check ldd on the WebKit binaries inside the AppImage:**
   ```
   ldd squashfs-root/usr/lib/x86_64-linux-gnu/webkit2gtk-4.1/WebKitNetworkProcess
   ```

3. **Review the full quick-sharun.sh output in CI logs** to see whether
   libjpeg appears in the "Collecting dependencies" / "Deploying shared
   libraries" sections.

4. **If libjpeg is not deployed, try passing it via an env var that
   quick-sharun natively supports** (like `DEPLOY_GDK=1` and adding the
   JPEG pixbuf loader explicitly).

5. **If libjpeg is deployed but WebKit can't find it**, check the sharun
   `lib.path` and `LD_LIBRARY_PATH` configuration inside the AppImage.
   The WebKit subprocesses might need the library in a specific
   subdirectory.

6. **As a last resort**, pin the quick-sharun.sh download to a specific
   commit hash instead of `refs/heads/main` to rule out upstream regressions.
