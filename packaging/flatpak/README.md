# Flatpak packaging

Goopie Launcher is **not** on Flathub. It ships as a Flatpak from a self-hosted
remote instead, published to GitHub Pages by the release workflow.

## For users

Install from the self-hosted remote (recommended — `flatpak update` works):

```sh
flatpak install --user https://birabittoh.github.io/GoopieLauncher/xyz.goopie.launcher.flatpakref
```

Or install a standalone bundle from a GitHub release (never auto-updates):

```sh
flatpak install --user ./Goopie-Launcher-linux-x86_64.flatpak
```

Both pull `org.gnome.Platform` from Flathub if it isn't installed already — that
is the only thing Flathub is used for. No remote has to be added by hand.

The Flatpak sets `GOOPIE_DISABLE_UPDATER=1`: the sandbox can't replace its own
binary, so updates come from `flatpak update`.

## For maintainers

`xyz.goopie.launcher.yml` fetches the launcher from a published GitHub release,
so it can be built from a clean checkout:

```sh
git submodule update --init packaging/flatpak/shared-modules
flatpak-builder --user --install-deps-from=flathub --force-clean \
  build packaging/flatpak/xyz.goopie.launcher.yml
```

CI can't do that (the release doesn't exist yet when it builds), so
`.github/workflows/_flatpak.yml` rewrites the manifest to package the binary
from the current build via `scripts/flatpak-local-manifest.py`.

The release workflow verifies that the newest `<release>` in
`xyz.goopie.launcher.metainfo.xml` matches the tag, so bump it alongside the
other version files. The `url`/`sha256` pinned in the manifest are only used by
manual from-scratch builds and can lag behind.

Publishing needs no secrets, but GitHub Pages must be enabled once under
**Settings → Pages → Source: GitHub Actions**. The `flatpak-repo` job then
deploys the ostree repo, the `.flatpakref` and a small landing page to
`https://birabittoh.github.io/GoopieLauncher/`. Each release replaces the whole
site, so old objects don't accumulate.

The Flatpak only wraps the published Linux binary, so it can be built after a
release has already gone out — no version bump needed. Run **Actions → Flatpak
(existing release) → Run workflow** with the tag (e.g. `v1.9.0`); it downloads
that release's `Goopie-Launcher-linux-{x86_64,aarch64}` assets, publishes the
remote, and attaches the bundles to the release. It's re-runnable (assets are
uploaded with `--clobber`).

The repo is unsigned — no GPG key is referenced in the `.flatpakref`, so
Flatpak skips signature verification and relies on HTTPS. Adding a key later
means republishing the repo with `--gpg-sign` and adding `GPGKey=` to the ref.
