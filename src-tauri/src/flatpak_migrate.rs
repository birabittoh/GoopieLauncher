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
//! module moves what earlier versions already wrote, once, at startup, so the
//! fix is invisible to people who have been playing for months.
//!
//! Three rules keep it safe:
//!   - **Move, never copy.** Everything here lives under `$HOME`, so a rename is
//!     atomic and instant no matter how many gigabytes are involved. If a rename
//!     fails (a separate mount for `~/.var`, a permission problem), the entry is
//!     left exactly where it is — a multi-gigabyte copy at startup would be
//!     anything but unnoticeable.
//!   - **Never overwrite.** When the destination already exists, directories are
//!     merged entry by entry and conflicting files are left in the sandbox. A
//!     file that exists on both sides is one the host copy is newer for (it was
//!     written after the fix shipped), so the host copy wins and nothing is
//!     destroyed.
//!   - **Leave the launcher's own state alone.** `config.ini`, the playtime
//!     totals, the Drive token and the image cache are still resolved through
//!     the sandbox paths (`paths::config_file` and friends), so moving them
//!     would orphan them.
//!
//! `~/.var/app/<id>/cache` is deliberately not migrated: it holds shader and
//! asset caches that regenerate on their own, and merging it would churn
//! gigabytes for no user-visible gain.

//! Only the Flatpak-specific half is Unix-gated; the move/merge core below is
//! portable so its unit tests run on every platform.

#[cfg(not(windows))]
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;

#[cfg(not(windows))]
use crate::{config, paths};

/// Runs the migration if this is a Flatpak install that hasn't had it yet.
///
/// Called once from `lib::run`, before anything reads the games folder or the
/// save locations. A no-op outside Flatpak, and after the first successful run.
#[cfg(not(windows))]
pub fn run_once() {
    if !paths::in_flatpak() {
        return;
    }
    if config::get_flatpak_migration_done() {
        return;
    }

    migrate_games_folder();

    // Saves written before `XDG_DATA_HOME` was redirected, plus per-game
    // settings and state. `Goopie`/`GoopieLauncher` hold the launcher's own
    // files and stay put — see the module docs.
    migrate_sandbox_dir("XDG_DATA_HOME", &[".local", "share"], &["Goopie"]);
    migrate_sandbox_dir("XDG_CONFIG_HOME", &[".config"], &["GoopieLauncher"]);
    migrate_sandbox_dir("XDG_STATE_HOME", &[".local", "state"], &[]);

    config::set_flatpak_migration_done(true);
    eprintln!("[flatpak] user-data migration complete");
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

/// Moves the *contents* of the sandbox dir `var` into the host directory at
/// `$HOME/<dest_parts>`, skipping the top-level entries named in `keep`.
#[cfg(not(windows))]
fn migrate_sandbox_dir(var: &str, dest_parts: &[&str], keep: &[&str]) {
    let (Some(src), Some(dst)) = (sandbox_dir(var), paths::flatpak_host_path(dest_parts)) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&src) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep.iter().any(|k| name.as_os_str() == OsStr::new(k)) {
            continue;
        }
        merge_move(&entry.path(), &dst.join(&name));
    }
}

/// Moves the games library out of the sandbox, so the Flatpak install ends up
/// using the same `~/.local/share/Goopie/Games` a native install does.
///
/// Only ever a plain rename of the whole tree: if the host already has a games
/// folder (a native install on the same machine), the two libraries are left
/// alone rather than merged — `paths::default_games_folder` keeps resolving to
/// the sandbox one, exactly as it does today, and nothing moves under the
/// user's feet.
#[cfg(not(windows))]
fn migrate_games_folder() {
    let Some(src_data) = sandbox_dir("XDG_DATA_HOME") else {
        return;
    };
    let src = src_data.join("Goopie").join("Games");
    if !src.is_dir() {
        return;
    }
    let Some(dst) = paths::flatpak_host_path(&[".local", "share", "Goopie", "Games"]) else {
        return;
    };
    if exists(&dst) {
        eprintln!(
            "[flatpak] leaving the games folder in the sandbox: {} already exists",
            dst.display()
        );
        return;
    }

    // A games folder the user pointed elsewhere themselves is theirs to manage.
    // `get_games_folder` falls back to the default, which is `src` precisely
    // because `src` exists — so equality here also covers "never configured".
    let configured = config::get_games_folder();
    if Path::new(&configured) != src.as_path() {
        return;
    }

    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::rename(&src, &dst) {
        eprintln!("[flatpak] could not move the games folder ({e}); leaving it in the sandbox");
        return;
    }

    // Pin the new location instead of relying on `default_games_folder`'s
    // "sandbox folder no longer exists" heuristic: if anything ever recreates
    // that directory, the library must not silently look empty again.
    config::set_games_path(&dst.to_string_lossy());
    eprintln!("[flatpak] moved the games folder to {}", dst.display());
}

/// Whether `path` exists, counting symlinks (even broken ones) as existing —
/// `Path::exists` follows them and would report a dangling link as free space
/// to write into.
#[cfg_attr(windows, allow(dead_code))]
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Moves `src` to `dst`, merging directories and never overwriting.
///
/// - `dst` free: one rename, whatever the size. On failure the entry stays put.
/// - both directories: recurse per entry, then drop `src` if it came out empty.
/// - anything else (file vs. file, file vs. directory, either side a symlink):
///   leave `src` alone. Losing a file to this migration is worse than leaving a
///   stale copy behind in the sandbox.
#[cfg_attr(windows, allow(dead_code))] // Flatpak is Linux-only; tests still run everywhere.
fn merge_move(src: &Path, dst: &Path) {
    let Ok(src_meta) = fs::symlink_metadata(src) else {
        return;
    };

    if !exists(dst) {
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::rename(src, dst) {
            Ok(()) => eprintln!("[flatpak] migrated {} → {}", src.display(), dst.display()),
            Err(e) => eprintln!("[flatpak] skipped {} ({e})", src.display()),
        }
        return;
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
        return;
    };
    for entry in entries.flatten() {
        merge_move(&entry.path(), &dst.join(entry.file_name()));
    }
    // Succeeds only when every child made it across; a directory holding
    // skipped conflicts stays, with its contents intact.
    let _ = fs::remove_dir(src);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn moves_entries_whose_destination_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src/game/save.bin");
        write(&src, "save");

        merge_move(&tmp.path().join("src/game"), &tmp.path().join("dst/game"));

        assert_eq!(fs::read_to_string(tmp.path().join("dst/game/save.bin")).unwrap(), "save");
        assert!(!tmp.path().join("src/game").exists());
    }

    #[test]
    fn merges_directories_and_never_overwrites_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/game/save.bin"), "sandbox");
        write(&tmp.path().join("src/game/extra.bin"), "only-in-sandbox");
        write(&tmp.path().join("dst/game/save.bin"), "host");

        merge_move(&tmp.path().join("src/game"), &tmp.path().join("dst/game"));

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

        merge_move(&tmp.path().join("src/a"), &tmp.path().join("dst/a"));

        assert_eq!(fs::read_to_string(tmp.path().join("dst/a/b/c.txt")).unwrap(), "host");
        assert!(tmp.path().join("src/a/b/c.txt").exists());
    }
}
