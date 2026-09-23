//! One-time migration of Flatpak-sandboxed user data back to the host home.
//!
//! Until this was fixed, a game spawned from the Flatpak inherited the sandbox's
//! `XDG_CONFIG_HOME`/`XDG_STATE_HOME` (and, before that, `XDG_DATA_HOME`), so it
//! wrote its settings, saves and progress into
//! `~/.var/app/xyz.goopie.launcher/…` instead of the host home. The same game
//! started from the AppImage — or from the Flatpak after the fix — looks in
//! `~/.config`, `~/.local/share` and `~/.local/state` and finds nothing.
//!
//! `games::resolve_launch` now points new launches at the host paths; this
//! module moves what earlier versions already wrote, at startup, so the fix is
//! invisible to people who have been playing for months.
//!
//! Inside the sandbox a plain rename can never do that move: `~/.var/app` is a
//! tmpfs with the app's own directory bind-mounted over it, so the source and
//! the host home are different mounts and `rename(2)`/`link(2)` fail with
//! `EXDEV` even though both live on the same disk (the first revision of this
//! migration assumed otherwise, moved nothing, and still marked itself done).
//! Hence the rules:
//!   - **Rename when possible, copy when not.** A rename is still tried first;
//!     on `EXDEV` each file is copied to a temporary name beside its
//!     destination, fsynced, linked into place, and only then removed from the
//!     sandbox. An interruption at any point leaves either the sandbox copy or
//!     a complete host copy — never a truncated file under the real name.
//!   - **Never overwrite.** When the destination already exists, directories are
//!     merged entry by entry and conflicting files are left in the sandbox. A
//!     file that exists on both sides is one the host copy is newer for (it was
//!     written after the fix shipped), so the host copy wins and nothing is
//!     destroyed.
//!   - **Never cross into a mount.** Flatpak bind-mounts some host files into
//!     the sandbox dirs (`config/user-dirs.dirs` is the host's own file,
//!     read-only); "moving" one would at best fail and at worst delete it.
//!   - **Leave the launcher's and the sandbox's own state alone.** `config.ini`,
//!     the playtime totals, the Drive token and the image cache are still
//!     resolved through the sandbox paths (`paths::config_file` and friends),
//!     WebKit keeps its storage under the app id, and entries like `glib-2.0`
//!     only mean anything inside the sandbox.
//!   - **Only done when it's done.** The run is recorded as complete only if
//!     nothing failed; otherwise it tries again on the next start. Conflicts
//!     are deliberate outcomes, not failures, so they don't cause retries.
//!
//! Two things are deliberately not migrated: `~/.var/app/<id>/cache` (shader
//! and asset caches that regenerate on their own) and the games library. The
//! library can be many gigabytes, which as a copy is anything but invisible,
//! and it needs no move to keep working — `paths::default_games_folder` keeps
//! resolving to the sandbox copy for as long as it exists.
//!
//! Only the Flatpak-specific half is Unix-gated; the move/merge core below is
//! portable so its unit tests run on every platform.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use crate::{config, paths};

/// Bumped whenever the migration changes in a way that has to re-run on
/// installs an earlier revision already marked done. Revision 1 could never
/// move anything from inside the sandbox (see the module docs).
#[cfg(not(windows))]
const MIGRATION_VERSION: u32 = 2;

/// Directory WebKit keeps the web view's storage in under each XDG base dir.
#[cfg(not(windows))]
const APP_ID: &str = "xyz.goopie.launcher";

/// Entries in the sandbox's XDG dirs that only make sense inside the sandbox:
/// GSettings' keyfile backend, the PulseAudio cookie, and the XDG user-dirs
/// files Flatpak mounts in from the host.
#[cfg(not(windows))]
const SANDBOX_ONLY: &[&str] = &["glib-2.0", "dconf", "pulse", "user-dirs.dirs", "user-dirs.locale"];

/// Runs the migration if this is a Flatpak install that hasn't had it yet.
///
/// Called once from `lib::run`, before anything reads the save locations. A
/// no-op outside Flatpak, and after the first run that completes cleanly.
#[cfg(not(windows))]
pub fn run_once() {
    if !paths::in_flatpak() {
        return;
    }
    if config::get_flatpak_migration_version() >= MIGRATION_VERSION {
        return;
    }

    let mut mover = Mover::new(mount_points());

    // Saves written before `XDG_DATA_HOME` was redirected, plus per-game
    // settings and state. `Goopie`/`GoopieLauncher` hold the launcher's own
    // files and stay put — see the module docs.
    mover.migrate_sandbox_dir("XDG_DATA_HOME", &[".local", "share"], &["Goopie", APP_ID]);
    mover.migrate_sandbox_dir("XDG_CONFIG_HOME", &[".config"], &["GoopieLauncher", APP_ID]);
    mover.migrate_sandbox_dir("XDG_STATE_HOME", &[".local", "state"], &[APP_ID]);

    if mover.failures == 0 {
        config::set_flatpak_migration_version(MIGRATION_VERSION);
        eprintln!("[flatpak] user-data migration complete");
    } else {
        eprintln!(
            "[flatpak] user-data migration left {} entries behind; will retry on next start",
            mover.failures
        );
    }
}

/// The sandbox directory `var` points at, but only when it really is one.
///
/// Returns `None` when the variable is unset/empty, when it doesn't resolve
/// under `~/.var/app/` (a host path needs no migration — and must not be walked
/// as if it were a sandbox dir), or when the directory doesn't exist.
#[cfg(not(windows))]
fn sandbox_dir(var: &str) -> Option<PathBuf> {
    let value = std::env::var(var).ok().filter(|v| !v.is_empty())?;
    let path = PathBuf::from(value);
    let sandbox_root = paths::flatpak_host_path(&[".var", "app"])?;
    if !path.starts_with(&sandbox_root) || !path.is_dir() {
        return None;
    }
    Some(path)
}

/// Every mount point visible to this process, from `/proc/self/mountinfo`.
///
/// Empty if that can't be read, which only loses the mount-point guard — the
/// bind mounts it protects are read-only, so touching one would fail anyway.
#[cfg(not(windows))]
fn mount_points() -> HashSet<PathBuf> {
    fs::read_to_string("/proc/self/mountinfo")
        .map(|info| {
            info.lines()
                .filter_map(|line| line.split(' ').nth(4))
                .map(|p| PathBuf::from(unescape_mountinfo(p)))
                .collect()
        })
        .unwrap_or_default()
}

/// Undoes mountinfo's octal escaping of space, tab, newline and backslash
/// (`\040` and friends).
#[cfg_attr(windows, allow(dead_code))]
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
        if let (b'\\', Some(d)) = (bytes[i], octal) {
            let n = u32::from(d[0] - b'0') * 64 + u32::from(d[1] - b'0') * 8 + u32::from(d[2] - b'0');
            out.push(n as u8);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

type RenameFn = fn(&Path, &Path) -> io::Result<()>;

/// Walks and moves sandbox entries, tallying what couldn't be moved.
#[cfg_attr(windows, allow(dead_code))] // Flatpak is Linux-only; tests still run everywhere.
struct Mover {
    /// Mount points never to move or descend into.
    mounts: HashSet<PathBuf>,
    /// `fs::rename`, swappable so tests can reproduce the sandbox's `EXDEV`.
    rename: RenameFn,
    /// Entries that should have moved but didn't; any makes the run retry.
    failures: usize,
}

#[cfg_attr(windows, allow(dead_code))]
impl Mover {
    fn new(mounts: HashSet<PathBuf>) -> Self {
        Mover { mounts, rename: |src, dst| fs::rename(src, dst), failures: 0 }
    }

    /// Moves the *contents* of the sandbox dir `var` into the host directory at
    /// `$HOME/<dest_parts>`, skipping the top-level entries named in `keep` and
    /// in [`SANDBOX_ONLY`].
    #[cfg(not(windows))]
    fn migrate_sandbox_dir(&mut self, var: &str, dest_parts: &[&str], keep: &[&str]) {
        let (Some(src), Some(dst)) = (sandbox_dir(var), paths::flatpak_host_path(dest_parts)) else {
            return;
        };
        let Ok(entries) = fs::read_dir(&src) else {
            return;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            if keep.iter().chain(SANDBOX_ONLY).any(|k| name == *k) {
                continue;
            }
            self.merge_move(&entry.path(), &dst.join(&name));
        }
    }

    /// Moves `src` to `dst`, merging directories and never overwriting.
    ///
    /// - `dst` free: one rename, whatever the size; across mounts, a directory
    ///   is recreated and filled entry by entry, a file copied then removed.
    /// - both directories: recurse per entry, then drop `src` if it came out empty.
    /// - anything else (file vs. file, file vs. directory, either side a symlink):
    ///   leave `src` alone. Losing a file to this migration is worse than leaving a
    ///   stale copy behind in the sandbox.
    fn merge_move(&mut self, src: &Path, dst: &Path) {
        if self.mounts.contains(src) {
            eprintln!("[flatpak] keeping {} in the sandbox: it is a mount point", src.display());
            return;
        }
        let Ok(src_meta) = fs::symlink_metadata(src) else {
            return;
        };

        if !exists(dst) {
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match (self.rename)(src, dst) {
                Ok(()) => {
                    eprintln!("[flatpak] migrated {} → {}", src.display(), dst.display());
                    return;
                }
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
                Err(e) => {
                    eprintln!("[flatpak] skipped {} ({e})", src.display());
                    self.failures += 1;
                    return;
                }
            }

            // Different mounts — always the case from inside the sandbox.
            if !src_meta.is_dir() {
                match copy_then_remove(src, dst, &src_meta) {
                    Ok(true) => eprintln!("[flatpak] migrated {} → {} (copied)", src.display(), dst.display()),
                    Ok(false) => eprintln!("[flatpak] keeping {} in the sandbox: not a file or symlink", src.display()),
                    Err(e) => {
                        eprintln!("[flatpak] skipped {} ({e})", src.display());
                        self.failures += 1;
                    }
                }
                return;
            }
            if let Err(e) = fs::create_dir(dst) {
                eprintln!("[flatpak] skipped {} ({e})", src.display());
                self.failures += 1;
                return;
            }
            let _ = fs::set_permissions(dst, src_meta.permissions());
            // Fall through: `dst` is now an empty directory to merge into.
        }

        let dst_is_dir = fs::symlink_metadata(dst).map(|m| m.is_dir()).unwrap_or(false);
        if !(src_meta.is_dir() && dst_is_dir) {
            eprintln!(
                "[flatpak] keeping {} in the sandbox: {} already exists",
                src.display(),
                dst.display()
            );
            return;
        }

        let Ok(entries) = fs::read_dir(src) else {
            self.failures += 1;
            return;
        };
        for entry in entries.flatten() {
            self.merge_move(&entry.path(), &dst.join(entry.file_name()));
        }
        // Succeeds only when every child made it across; a directory holding
        // skipped conflicts stays, with its contents intact.
        let _ = fs::remove_dir(src);
    }
}

/// Whether `path` exists, counting symlinks (even broken ones) as existing —
/// `Path::exists` follows them and would report a dangling link as free space
/// to write into.
#[cfg_attr(windows, allow(dead_code))]
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Copies the file or symlink `src` to the free path `dst`, then removes `src`.
///
/// The copy is written to a temporary name beside `dst` and hard-linked into
/// place, so `dst` only ever appears complete, and a `dst` that showed up in
/// the meantime is never overwritten (the link fails instead). Returns
/// `Ok(false)` for anything that is neither a file nor a symlink (sockets,
/// FIFOs), which is left where it is.
#[cfg_attr(windows, allow(dead_code))]
fn copy_then_remove(src: &Path, dst: &Path, src_meta: &fs::Metadata) -> io::Result<bool> {
    if src_meta.file_type().is_symlink() {
        copy_symlink(src, dst)?;
        fs::remove_file(src)?;
        return Ok(true);
    }
    if !src_meta.is_file() {
        return Ok(false);
    }

    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dst.with_file_name(format!(".{name}.goopie-migrate"));
    let result = (|| {
        fs::copy(src, &tmp)?;
        let file = fs::OpenOptions::new().write(true).open(&tmp)?;
        // Saves are judged by their timestamps (backups, cloud sync), so the
        // copy keeps the original's rather than looking freshly written.
        if let Ok(modified) = src_meta.modified() {
            file.set_modified(modified)?;
        }
        file.sync_all()?;
        fs::hard_link(&tmp, dst)
    })();
    let _ = fs::remove_file(&tmp);
    result?;

    fs::remove_file(src)?;
    Ok(true)
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst)
}

#[cfg(not(unix))]
fn copy_symlink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "symlinks are only migrated on Unix"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn mover() -> Mover {
        Mover::new(HashSet::new())
    }

    /// A mover that behaves like the sandbox: every rename crosses a mount.
    fn cross_device_mover() -> Mover {
        Mover {
            rename: |_, _| Err(io::Error::from(io::ErrorKind::CrossesDevices)),
            ..mover()
        }
    }

    #[test]
    fn moves_entries_whose_destination_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src/game/save.bin");
        write(&src, "save");

        mover().merge_move(&tmp.path().join("src/game"), &tmp.path().join("dst/game"));

        assert_eq!(fs::read_to_string(tmp.path().join("dst/game/save.bin")).unwrap(), "save");
        assert!(!tmp.path().join("src/game").exists());
    }

    #[test]
    fn merges_directories_and_never_overwrites_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/game/save.bin"), "sandbox");
        write(&tmp.path().join("src/game/extra.bin"), "only-in-sandbox");
        write(&tmp.path().join("dst/game/save.bin"), "host");

        mover().merge_move(&tmp.path().join("src/game"), &tmp.path().join("dst/game"));

        // The host copy of the conflicting file is untouched...
        assert_eq!(fs::read_to_string(tmp.path().join("dst/game/save.bin")).unwrap(), "host");
        // ...and the sandbox copy is kept rather than deleted.
        assert_eq!(fs::read_to_string(tmp.path().join("src/game/save.bin")).unwrap(), "sandbox");
        // The non-conflicting sibling still migrates.
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/game/extra.bin")).unwrap(),
            "only-in-sandbox"
        );
        assert!(!tmp.path().join("src/game/extra.bin").exists());
    }

    #[test]
    fn leaves_a_conflicting_file_alone_even_under_a_matching_tree() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/a/b/c.txt"), "sandbox");
        write(&tmp.path().join("dst/a/b/c.txt"), "host");

        let mut m = mover();
        m.merge_move(&tmp.path().join("src/a"), &tmp.path().join("dst/a"));

        assert_eq!(fs::read_to_string(tmp.path().join("dst/a/b/c.txt")).unwrap(), "host");
        assert!(tmp.path().join("src/a/b/c.txt").exists());
        // A conflict is a decision, not a failure: it must not force retries.
        assert_eq!(m.failures, 0);
    }

    #[test]
    fn copies_across_mounts_when_rename_fails_with_exdev() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/game/save.bin"), "save");
        write(&tmp.path().join("src/game/slots/1.bin"), "slot");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(tmp.path().join("src/game/save.bin"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let mut m = cross_device_mover();
        m.merge_move(&tmp.path().join("src/game"), &tmp.path().join("dst/game"));

        let dst = tmp.path().join("dst/game");
        assert_eq!(fs::read_to_string(dst.join("save.bin")).unwrap(), "save");
        assert_eq!(fs::read_to_string(dst.join("slots/1.bin")).unwrap(), "slot");
        assert_eq!(fs::metadata(dst.join("save.bin")).unwrap().modified().unwrap(), old);
        // Nothing left behind: not the sandbox tree, not a temporary file.
        assert!(!tmp.path().join("src/game").exists());
        assert_eq!(fs::read_dir(&dst).unwrap().count(), 2);
        assert_eq!(m.failures, 0);
    }

    #[test]
    fn copy_fallback_still_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/game/save.bin"), "sandbox");
        write(&tmp.path().join("src/game/extra.bin"), "only-in-sandbox");
        write(&tmp.path().join("dst/game/save.bin"), "host");

        cross_device_mover().merge_move(&tmp.path().join("src/game"), &tmp.path().join("dst/game"));

        assert_eq!(fs::read_to_string(tmp.path().join("dst/game/save.bin")).unwrap(), "host");
        assert_eq!(fs::read_to_string(tmp.path().join("src/game/save.bin")).unwrap(), "sandbox");
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/game/extra.bin")).unwrap(),
            "only-in-sandbox"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_fallback_recreates_symlinks_instead_of_following_them() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", tmp.path().join("src/link")).unwrap();

        cross_device_mover().merge_move(&tmp.path().join("src/link"), &tmp.path().join("dst/link"));

        assert_eq!(fs::read_link(tmp.path().join("dst/link")).unwrap(), Path::new("../elsewhere"));
        assert!(!exists(&tmp.path().join("src/link")));
    }

    #[test]
    fn never_touches_a_mount_point() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/user-dirs.dirs"), "host file");
        let mut m = Mover::new(HashSet::from([tmp.path().join("src/user-dirs.dirs")]));

        m.merge_move(&tmp.path().join("src/user-dirs.dirs"), &tmp.path().join("dst/user-dirs.dirs"));

        assert!(tmp.path().join("src/user-dirs.dirs").exists());
        assert!(!exists(&tmp.path().join("dst/user-dirs.dirs")));
    }

    #[test]
    fn counts_other_rename_errors_as_failures_and_leaves_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/save.bin"), "save");
        let mut m = Mover {
            rename: |_, _| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            ..mover()
        };

        m.merge_move(&tmp.path().join("src/save.bin"), &tmp.path().join("dst/save.bin"));

        assert_eq!(m.failures, 1);
        assert!(tmp.path().join("src/save.bin").exists());
    }

    #[test]
    fn unescapes_mountinfo_paths() {
        assert_eq!(unescape_mountinfo(r"/home/a\040b/c\134d"), r"/home/a b/c\d");
        assert_eq!(unescape_mountinfo("/plain"), "/plain");
        assert_eq!(unescape_mountinfo(r"/trailing\04"), r"/trailing\04");
    }
}
