## Update 2: the real root cause was never "a missing library"

A user hit the same symptom again on a different host (CachyOS/Arch, not the
CI's Ubuntu 24.04), this time for `libicudata.so.74`. Debugging live against
their actual downloaded AppImage (extract, patch, re-run, repeat) turned up
two distinct bugs stacked on top of each other — bundling one more library
was never going to be a complete fix:

1. **`WebKit*Process` binaries have no RPATH and keep their original system
   interpreter.** They're plain copies of the upstream webkit2gtk helper
   executables. quick-sharun's LD_LIBRARY_PATH plumbing only covers the
   *main* process — it execs `goopie-launcher` via an explicit
   `ld-linux --library-path ...` invocation, which never touches a real env
   var, so nothing propagates to processes WebKit forks internally. Those
   helpers fall through entirely to the *host's* system libraries. This is
   why the bug is host-dependent: it only surfaces once a host's system
   package (ICU, libxml2, whatever) drifts far enough from what this WebKit
   build was compiled against. Confirmed by checking `/proc/<pid>/environ` of
   the running main process — no `LD_LIBRARY_PATH` there at all, yet the main
   process's own libs resolve fine, because it's loaded via a one-off
   `ld.so --library-path` argument, not an inherited env var.

2. **Fixing bug 1 with a naive RPATH isn't enough either.** patchelf sets
   `DT_RUNPATH` by default, which only applies to an ELF's *direct*
   dependencies — not transitive ones. `libxml2.so.2` (missing on the test
   host, which only has Arch's incompatible `libxml2.so.16`) is a dependency
   of `libwebkit2gtk-4.1.so.0`, not of `WebKitWebProcess` itself, so RUNPATH
   never got consulted for it and the loader fell back to system search
   paths. Needs classic `DT_RPATH` (`patchelf --force-rpath`), which *is*
   applied transitively for the whole load.
3. **And once DT_RPATH pulls in the bundle for transitive deps, it also pulls
   in the bundle's own `libc.so.6`** (since RPATH is now searched
   process-wide) — while the binary's PT_INTERP is still the *host's* dynamic
   loader, since it was never repatched. Mismatched libc/ld.so pairing breaks
   glibc's private ABI: `undefined symbol: __nptl_change_stack_perm, version
   GLIBC_PRIVATE`. Fix: repoint PT_INTERP at the bundled
   `ld-linux-x86-64.so.2` too, so loader and libc are a matched pair.

PT_INTERP is resolved directly by the kernel with no `$ORIGIN` or env-var
expansion, so it can't be pointed at the AppImage's mount path (only known at
run time, changes every launch). quick-sharun already solves exactly this
problem for other hardcoded absolute paths it finds at build time — it drops
`*.hook` scripts in `bin/` that symlink a fixed `/tmp/<token>` to `$APPDIR/lib`
(etc.) on every launch, and patches the relevant binaries to reference that
fixed token instead. The CI step now adds its *own* hook
(`00-goopie-webkit-libpath.hook`, symlinking `/tmp/goopie-webkit-lib` →
`$APPDIR/lib`) rather than depending on quick-sharun's own token existing
(it's randomly generated per-build and only appears if quick-sharun's own
hardcoded-path detection fires), and points `WebKit*Process`'s new PT_INTERP
at that fixed path.

Also fixed along the way: the missing-library detection loop (see "Update 1"
below) globbed `usr/lib/*/webkit2gtk-4.1/WebKit*Process`, assuming a
multiarch subdirectory that doesn't actually exist in this bundler's output
(it's a flat `lib/webkit2gtk-4.1/`) — the glob silently matched nothing, so
that whole detection loop had been a no-op since it was written.

All of this was verified end-to-end against the user's actual downloaded
AppImage (patched the extracted AppDir in place, re-ran it, watched
`WebKitNetworkProcess`/`WebKitWebProcess` start and stay up, confirmed the
window rendered real content) before porting the fix into
`.github/workflows/_build.yml`.

## Update 1: generalized to any missing WebKit runtime dep

After the DwarFS/`uruntime` fix below, a second missing library surfaced at
runtime on a user's machine: `libicudata.so.74` (WebKit's ICU dependency),
failing with the same "cannot open shared object file" error, but only for
`WebKitNetworkProcess`/`WebKitWebProcess` — same failure mode as libjpeg, just
a different library that `quick-sharun.sh` didn't detect.

Rather than hardcode a second library name, the injection step in
`.github/workflows/_build.yml` ("Inject missing shared libraries into
AppImage") now runs `ldd` against the extracted WebKit subprocess binaries,
collects whatever it reports as "not found", and copies each one in from the
host (`cp -L` to dereference the soname symlink, so the AppDir gets a real
file rather than a potentially-dangling symlink). `libicu74` was also added
to the apt install list alongside `libjpeg-turbo8` so the host is guaranteed
to have it. This turned out not to be sufficient on its own — see "Update 2"
above for the rest of the story.

## Resolution

Every earlier attempt at the post-build injection step assumed the AppImage's
payload was a **squashfs** image and tried to locate/unpack it by hand (a
stored offset, a magic-byte search, ELF section-header math). All of them
failed at the unpack step, with `unsquashfs` reporting an invalid superblock.

The actual bundler chain is: Tauri's `feat/truly-portable-appimage` branch
shells out to pkgforge-dev's `quick-sharun.sh`, which in turn packs the
finished AppDir using pkgforge-dev's own `appimagetool` (a Rust rewrite, not
the classic AppImageKit one). That tool embeds the payload as a **DwarFS**
image, not squashfs, played back through the `uruntime` runtime — so there
was never a squashfs superblock to find, no matter how correctly the offset
was computed.

`uruntime` is format-agnostic from the CLI: it exposes a classic-AppImageKit-
compatible `--appimage-extract` that extracts either backing format to
`./squashfs-root` (name kept for compat), so the fix lets the AppImage
extract itself instead of reimplementing that logic. Repacking goes through
the same `appimagetool` binary `quick-sharun.sh` uses, so it re-embeds
whichever format the tool defaults to. See `.github/workflows/_build.yml`,
"Inject libjpeg.so.8 into AppImage".

Also placed the injected library next to the other WebKit shared libs
(auto-detected via wherever `libwebkit2gtk-4.1.so*` landed) rather than a
flat `usr/lib/`, since sharun preserves the original multiarch subdirectory
(`usr/lib/x86_64-linux-gnu/`) and only that directory is on the bundle's
`LD_LIBRARY_PATH`.

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
