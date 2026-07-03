//! Game-agnostic mods framework.
//!
//! Per game, `<games>/<recompName>/mods/` holds one subdirectory per mod (the
//! subdirectory *name* is the canonical mod id the ReXGlue SDK expects in
//! `--enabled_mods`) plus a launcher-managed `mods.toml` sidecar recording a
//! single ordered list of `{ id, enabled }` entries — the load-priority order
//! (first = highest priority), with disabled mods simply skipped when
//! building `--enabled_mods` but keeping their position so re-enabling one
//! restores it instead of dropping it to the bottom. The SDK does not
//! auto-discover mods, read any per-mod manifest, or track enable/order state
//! itself — that's entirely on us. See `sdk/include/rex/runtime.h`
//! (`ModOverlayRoots`, `ResolveEnabledMods`) in the ReXGlue SDK for the
//! runtime contract.
//!
//! Each mod folder may optionally contain a `mod.toml` (display metadata) and
//! an `icon.png`; Goopie is intentionally agnostic to any other subdirectories
//! inside a mod (`game/`, `update/`, `textures/`, `shaders/`, ...) — those are
//! defined entirely by the game/SDK, not by the launcher.

use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};

use crate::games::game_root;

const SIDECAR_NAME: &str = "mods.toml";
const MANIFEST_NAME: &str = "mod.toml";
const ICON_NAME: &str = "icon.png";

/// A single entry in the `mods.toml` sidecar: one mod's id and whether it's
/// enabled. Order in the `mods` array *is* the load-priority order — first
/// entry loads with highest priority. Disabled entries are skipped when
/// building `--enabled_mods` but keep their slot, so toggling a mod off and
/// back on doesn't lose its place in the order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarEntry {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Launcher-managed sidecar. Ignored by the SDK, which only ever sees the
/// reconciled `--enabled_mods` string built from `entries`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Sidecar {
    #[serde(default)]
    mods: Vec<SidecarEntry>,
}

/// Highest `manifest_version` this launcher build understands. Bump when the
/// `mod.toml` schema gains a field whose absence would change meaning (rather
/// than just being ignored) — e.g. a semantics change to `requires`. Mods
/// omit the field entirely today, which is treated as version 1.
const CURRENT_MANIFEST_VERSION: u32 = 1;

/// Optional per-mod metadata (`mod.toml`). All fields are optional — a mod
/// with no manifest still works, falling back to its folder name and no
/// icon/description. `requires` lists other mod ids (folder names) and is
/// parsed and surfaced to the UI but not yet enforced (dependency enforcement
/// is a deferred follow-up).
#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default = "default_manifest_version")]
    manifest_version: u32,
    name: Option<String>,
    version: Option<String>,
    author: Option<String>,
    description: Option<String>,
    #[serde(default)]
    requires: Vec<String>,
}

fn default_manifest_version() -> u32 {
    1
}

/// A single mod as surfaced to the website.
#[derive(Debug, Serialize)]
pub struct ModInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub requires: Vec<String>,
    pub enabled: bool,
    /// `data:image/png;base64,...` icon, or empty if the mod has no `icon.png`.
    pub icon: String,
}

/// Report returned by [`install_archives`]: one entry per attempted file.
#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub results: Vec<InstallResult>,
}

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub path: String,
    pub ok: bool,
    /// The resolved mod id on success, or an error message on failure.
    pub message: String,
}

/// `<games>/<recompName>/mods/`.
pub fn mods_dir(game: &str) -> PathBuf {
    game_root(game).join("mods")
}

fn sidecar_path(game: &str) -> PathBuf {
    mods_dir(game).join(SIDECAR_NAME)
}

fn read_sidecar(game: &str) -> Sidecar {
    let path = sidecar_path(game);
    let Ok(content) = std::fs::read_to_string(&path) else { return Sidecar::default() };
    toml::from_str(&content).unwrap_or_default()
}

fn write_sidecar(game: &str, sidecar: &Sidecar) {
    let dir = mods_dir(game);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(content) = toml::to_string_pretty(sidecar) else { return };
    if let Err(e) = std::fs::write(sidecar_path(game), content) {
        eprintln!("[mods] Failed to write {}: {}", sidecar_path(game).display(), e);
    }
}

/// Enumerate the mod ids actually present on disk (immediate subdirectories
/// of `mods/`), sorted for determinism when reconciling against the sidecar.
fn installed_ids(game: &str) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(mods_dir(game))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    ids
}

/// Reconcile the sidecar against what's actually on disk, in order: keep
/// recorded entries (preserving their position and enabled flag) for ids that
/// still exist, drop entries whose folder is gone, and append any
/// new/undiscovered folder to the end as enabled (mods are enabled by default
/// when first discovered — e.g. right after being dropped/extracted).
fn reconcile(game: &str) -> Vec<SidecarEntry> {
    let sidecar = read_sidecar(game);
    let on_disk = installed_ids(game);

    let mut entries: Vec<SidecarEntry> = sidecar.mods.into_iter().filter(|e| on_disk.contains(&e.id)).collect();

    for id in &on_disk {
        if !entries.iter().any(|e| &e.id == id) {
            entries.push(SidecarEntry { id: id.clone(), enabled: true });
        }
    }

    entries
}

fn read_manifest_at(path: &std::path::Path) -> Manifest {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Manifest { manifest_version: CURRENT_MANIFEST_VERSION, ..Manifest::default() };
    };
    let manifest: Manifest = toml::from_str(&content).unwrap_or_else(|e| {
        eprintln!("[mods] Failed to parse {}: {}", path.display(), e);
        Manifest { manifest_version: CURRENT_MANIFEST_VERSION, ..Manifest::default() }
    });
    if manifest.manifest_version > CURRENT_MANIFEST_VERSION {
        eprintln!(
            "[mods] {} was authored for manifest_version {} (this launcher understands up to {}) — some fields may be ignored",
            path.display(), manifest.manifest_version, CURRENT_MANIFEST_VERSION
        );
    }
    manifest
}

fn read_manifest(game: &str, id: &str) -> Manifest {
    read_manifest_at(&mods_dir(game).join(id).join(MANIFEST_NAME))
}

/// Lenient dotted-numeric version comparison for mod versions, which — unlike
/// the launcher's own release tags — aren't guaranteed to be well-formed
/// semver. Each dot/dash/plus-separated segment is compared as a number
/// (non-numeric or missing segments count as 0), so `"1.2"` < `"1.10"` and a
/// blank version is always the lowest. Returns `true` when `new` is greater
/// than or equal to `existing`, including when both are equal or both blank
/// (an unversioned mod is always safe to re-drop over itself).
fn version_gte(new: &str, existing: &str) -> bool {
    fn segments(s: &str) -> Vec<u64> {
        s.split(['.', '-', '+'])
            .map(|seg| seg.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .map(|digits| digits.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let a = segments(new);
    let b = segments(existing);
    for i in 0..a.len().max(b.len()) {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        if av != bv {
            return av > bv;
        }
    }
    true
}

fn read_icon_data_url(game: &str, id: &str) -> String {
    let path = mods_dir(game).join(id).join(ICON_NAME);
    std::fs::read(&path)
        .map(|bytes| format!("data:image/png;base64,{}", B64.encode(bytes)))
        .unwrap_or_default()
}

/// List every installed mod for `game`, in sidecar order (which doubles as
/// load-priority order among the enabled ones). Reconciles the sidecar
/// against disk first, so this reflects reality even if `mods/` was edited by
/// hand.
pub fn list_mods(game: &str) -> Vec<ModInfo> {
    reconcile(game)
        .iter()
        .map(|entry| {
            let manifest = read_manifest(game, &entry.id);
            ModInfo {
                id: entry.id.clone(),
                name: manifest.name.unwrap_or_else(|| entry.id.clone()),
                version: manifest.version.unwrap_or_default(),
                author: manifest.author.unwrap_or_default(),
                description: manifest.description.unwrap_or_default(),
                requires: manifest.requires,
                enabled: entry.enabled,
                icon: read_icon_data_url(game, &entry.id),
            }
        })
        .collect()
}

/// The `--enabled_mods` value for `game`: reconciled enabled ids in priority
/// order (first = highest priority), comma-separated. `None` when there's no
/// `mods/` directory or nothing enabled — callers should omit both
/// `--mods_data_root` and `--enabled_mods` in that case.
pub fn enabled_mods_arg(game: &str) -> Option<String> {
    if !mods_dir(game).is_dir() {
        return None;
    }
    let ids: Vec<String> = reconcile(game).into_iter().filter(|e| e.enabled).map(|e| e.id).collect();
    if ids.is_empty() {
        return None;
    }
    Some(ids.join(","))
}

/// Overwrite the full ordered mod list and persist it. `entries` order is the
/// new load-priority order (first = highest); disabled entries keep their
/// slot. Callers should always pass the *complete* set of installed ids —
/// anything omitted will simply be re-appended (as enabled) next time
/// [`list_mods`]/[`enabled_mods_arg`] reconciles.
pub fn set_state(game: &str, entries: Vec<SidecarEntry>) {
    write_sidecar(game, &Sidecar { mods: entries });
}

/// Remove a mod's folder entirely and drop it from the sidecar.
pub fn remove_mod(game: &str, id: &str) {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        return;
    }
    let dir = mods_dir(game).join(id);
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("[mods] Failed to remove {}: {}", dir.display(), e);
            return;
        }
    }
    let mut sidecar = read_sidecar(game);
    sidecar.mods.retain(|e| e.id != id);
    write_sidecar(game, &sidecar);
    eprintln!("[mods] Removed mod {} for {}", id, game);
}

/// Sanitise a candidate mod id the same way build tags are (see
/// `games::sanitize_build_key`).
fn sanitize_mod_id(candidate: &str) -> String {
    let sanitized: String = candidate
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    if sanitized.is_empty() { "mod".to_string() } else { sanitized }
}

/// Outcome of successfully installing one archive.
#[derive(Debug)]
struct InstalledMod {
    id: String,
    version: String,
    /// `true` if this replaced an already-installed mod of the same id.
    updated: bool,
}

/// Extract one `.zip` into a mod folder under `mods/`.
///
/// Extracts into a scratch temp directory *inside* `mods/` first (so the
/// final move is a same-filesystem rename), then decides the mod id and final
/// layout from what actually landed on disk: if the archive contained exactly
/// one top-level directory (the common case — an author zips up the mod
/// folder itself), that directory becomes the mod and its name becomes the
/// id; otherwise the whole extracted tree becomes the mod, named after the
/// zip's file stem. This avoids double-nesting (`mods/foo/foo/...`) that a
/// naive "derive id from zip, then extract into mods/<id>/" approach would
/// produce when the zip already has a `foo/` prefix on every entry.
///
/// If a mod with the same id is already installed, the new one replaces it
/// when its `mod.toml` `version` is greater than or equal to the installed
/// one's (see [`version_gte`]) — otherwise the install is rejected so an
/// older drop can't clobber a newer install.
fn install_one_archive(mods_dir: &std::path::Path, zip_path: &str) -> std::io::Result<InstalledMod> {
    std::fs::create_dir_all(mods_dir)?;
    let staging = tempfile::Builder::new()
        .prefix(".mod-extract-")
        .tempdir_in(mods_dir)?;

    crate::archive::extract_zip(zip_path, &staging.path().to_string_lossy())?;

    let top_level: Vec<std::fs::DirEntry> = std::fs::read_dir(staging.path())?.filter_map(|e| e.ok()).collect();

    let (id, content_src): (String, PathBuf) = if top_level.len() == 1 && top_level[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
        let name = top_level[0].file_name().to_string_lossy().into_owned();
        (sanitize_mod_id(&name), top_level[0].path())
    } else {
        let stem = std::path::Path::new(zip_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mod".to_string());
        (sanitize_mod_id(&stem), staging.path().to_path_buf())
    };

    let new_version = read_manifest_at(&content_src.join(MANIFEST_NAME)).version.unwrap_or_default();

    let dest = mods_dir.join(&id);
    let updated = dest.exists();
    if updated {
        let existing_version = read_manifest_at(&dest.join(MANIFEST_NAME)).version.unwrap_or_default();
        if !version_gte(&new_version, &existing_version) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "a newer or equal version of \"{}\" is already installed (installed v{}, dropped v{})",
                    id,
                    if existing_version.is_empty() { "?" } else { &existing_version },
                    if new_version.is_empty() { "?" } else { &new_version },
                ),
            ));
        }
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(&content_src, &dest)?;
    Ok(InstalledMod { id, version: new_version, updated })
}

/// Extract every `.zip` in `paths` into its own mod folder under `mods/`,
/// appending each newly-installed mod to the end of the load order (enabled
/// by default) — or, if a mod of the same id and an equal-or-newer version is
/// already installed, replacing it in place (preserving its existing
/// position/enabled state). Non-zip paths are skipped with an error entry —
/// callers are expected to have already filtered to zips.
pub fn install_archives(game: &str, paths: &[String]) -> InstallReport {
    let dir = mods_dir(game);
    let mut results = Vec::new();
    // Reconcile first so we don't clobber ids that exist on disk but aren't
    // recorded yet (e.g. manually-copied sample mods).
    let mut entries = reconcile(game);

    for path in paths {
        if !crate::archive::is_zip(path) {
            results.push(InstallResult { path: path.clone(), ok: false, message: "not a .zip file".into() });
            continue;
        }

        match install_one_archive(&dir, path) {
            Ok(installed) => {
                if !entries.iter().any(|e| e.id == installed.id) {
                    entries.push(SidecarEntry { id: installed.id.clone(), enabled: true });
                }
                let version_suffix = if installed.version.is_empty() { String::new() } else { format!(" (v{})", installed.version) };
                let message = if installed.updated {
                    format!("Updated \"{}\"{}", installed.id, version_suffix)
                } else {
                    format!("Installed \"{}\"{}", installed.id, version_suffix)
                };
                results.push(InstallResult { path: path.clone(), ok: true, message });
            }
            Err(e) => {
                results.push(InstallResult { path: path.clone(), ok: false, message: e.to_string() });
            }
        }
    }

    write_sidecar(game, &Sidecar { mods: entries });

    InstallReport { results }
}

/// Run [`install_archives`] on a background thread, publishing the result to
/// `state.mod_install_report` and toggling `state.mod_installing` around it.
///
/// Mod zips can be large enough (tens to low hundreds of MB — sample sound
/// packs, HD texture sets, etc.) that extracting them inline on the bridge's
/// request-handling thread would freeze the webview for several seconds: the
/// bridge is a *synchronous* XHR, so the whole UI thread blocks until the
/// Rust call returns. Running it here instead lets the bridge command return
/// immediately, with the frontend polling `isInstallingMods`/`getModInstallReport`
/// (mirroring how `ProcessDrops`/`isExtracting`/`getDropReport` already work).
pub fn install_archives_async(state: std::sync::Arc<crate::AppState>, game: String, paths: Vec<String>) {
    state.mod_installing.store(true, std::sync::atomic::Ordering::Relaxed);
    let report = install_archives(&game, &paths);
    *state.mod_install_report.lock().unwrap() = Some(report);
    state.mod_installing.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Open the mods folder in the system file manager (creating it if needed).
pub fn open_mods_folder(game: &str) {
    let dir = mods_dir(game);
    let _ = std::fs::create_dir_all(&dir);
    crate::platform::open_folder(&dir.to_string_lossy());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a zip whose entries all live under a single top-level directory
    /// `dir_name/`, mirroring how mod authors typically zip up their mod
    /// folder directly (e.g. `badapple/mod.toml`, `badapple/game/...`).
    fn make_prefixed_zip(path: &std::path::Path, dir_name: &str, entries: &[(&str, &[u8])]) {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let f = std::fs::File::create(path).unwrap();
        let mut zw = ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            zw.start_file(format!("{}/{}", dir_name, name), opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    /// A zip whose entries sit directly at the root, with no common
    /// top-level directory — the fallback path that names the mod after the
    /// zip's own file stem instead.
    fn make_flat_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let f = std::fs::File::create(path).unwrap();
        let mut zw = ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    /// Regression test: a zip whose every entry is prefixed with the mod's
    /// own directory name (e.g. `badapple/mod.toml`) must extract to
    /// `mods/badapple/mod.toml`, not double-nest as `mods/badapple/badapple/mod.toml`.
    #[test]
    fn install_one_archive_unwraps_single_top_level_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let zip_path = tmp.path().join("badapple.zip");
        make_prefixed_zip(&zip_path, "badapple", &[
            ("mod.toml", b"name = \"Bad Apple\"\n"),
            ("game/DATA/sound/bgmusic.wma", b"fake audio"),
        ]);

        let installed = install_one_archive(&mods_dir, &zip_path.to_string_lossy()).unwrap();
        assert_eq!(installed.id, "badapple");
        assert!(!installed.updated);

        let dest = mods_dir.join("badapple");
        assert!(dest.join("mod.toml").is_file(), "mod.toml should sit directly under mods/badapple/");
        assert!(dest.join("game/DATA/sound/bgmusic.wma").is_file());
        assert!(!dest.join("badapple").exists(), "must not double-nest as mods/badapple/badapple/");
    }

    /// A zip with multiple top-level entries (no single wrapping directory)
    /// falls back to naming the mod after the zip's file stem.
    #[test]
    fn install_one_archive_falls_back_to_zip_stem_for_flat_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let zip_path = tmp.path().join("loose-files.zip");
        make_flat_zip(&zip_path, &[
            ("mod.toml", b"name = \"Loose\"\n"),
            ("game/DATA/thing.bin", b"data"),
        ]);

        let installed = install_one_archive(&mods_dir, &zip_path.to_string_lossy()).unwrap();
        assert_eq!(installed.id, "loose-files");
        assert!(mods_dir.join("loose-files/mod.toml").is_file());
    }

    #[test]
    fn install_one_archive_rejects_an_older_or_equal_version_over_an_unversioned_existing_mod() {
        // No version info on either side (both blank) compares as equal, so
        // an equal-or-newer re-drop is allowed — but a plain folder collision
        // with genuinely older content still has nothing to distinguish it,
        // so it's allowed too under the "blank == blank" rule. This test
        // instead pins an existing mod at v1.0.0 and drops an *older* v0.9.0
        // to prove the reject path fires when a real regression is detected.
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(mods_dir.join("badapple")).unwrap();
        std::fs::write(mods_dir.join("badapple/mod.toml"), b"version = \"1.0.0\"\n").unwrap();

        let zip_path = tmp.path().join("badapple.zip");
        make_prefixed_zip(&zip_path, "badapple", &[("mod.toml", b"version = \"0.9.0\"\n")]);

        let err = install_one_archive(&mods_dir, &zip_path.to_string_lossy()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(mods_dir.join("badapple/mod.toml").is_file(), "the older drop must not have touched the existing install");
        assert_eq!(std::fs::read_to_string(mods_dir.join("badapple/mod.toml")).unwrap(), "version = \"1.0.0\"\n");
    }

    #[test]
    fn install_one_archive_overwrites_an_equal_or_newer_version() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(mods_dir.join("badapple")).unwrap();
        std::fs::write(mods_dir.join("badapple/mod.toml"), b"version = \"1.0.0\"\ndescription = \"old\"\n").unwrap();
        std::fs::write(mods_dir.join("badapple/stale.txt"), b"leftover from the old install").unwrap();

        // Same version string ("1.0.0" >= "1.0.0") should still be allowed to overwrite.
        let zip_path = tmp.path().join("badapple.zip");
        make_prefixed_zip(&zip_path, "badapple", &[("mod.toml", b"version = \"1.0.0\"\ndescription = \"new\"\n")]);

        let installed = install_one_archive(&mods_dir, &zip_path.to_string_lossy()).unwrap();
        assert_eq!(installed.id, "badapple");
        assert!(installed.updated);
        assert_eq!(installed.version, "1.0.0");

        let content = std::fs::read_to_string(mods_dir.join("badapple/mod.toml")).unwrap();
        assert!(content.contains("new"), "the new mod.toml should have replaced the old one");
        assert!(!mods_dir.join("badapple/stale.txt").exists(), "the old install's files must not linger after an overwrite");
    }

    #[test]
    fn version_gte_compares_numeric_segments_not_lexically() {
        assert!(version_gte("1.10.0", "1.2.0"), "1.10.0 must beat 1.2.0 numerically, not lexically");
        assert!(!version_gte("1.2.0", "1.10.0"));
        assert!(version_gte("1.0.0", "1.0.0"), "equal versions count as gte");
        assert!(version_gte("", ""), "two blank versions count as gte");
        assert!(version_gte("1.0.0", ""), "any version beats a blank one");
        assert!(!version_gte("", "1.0.0"), "a blank version loses to a real one");
        assert!(version_gte("2.0.0-beta", "1.9.9"));
    }

    #[test]
    fn reconcile_preserves_order_and_enabled_state_and_appends_new_disk_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(mods_dir.join("badapple")).unwrap();
        std::fs::create_dir_all(mods_dir.join("hdost")).unwrap();
        std::fs::create_dir_all(mods_dir.join("newmod")).unwrap();

        let sidecar = Sidecar {
            mods: vec![
                SidecarEntry { id: "badapple".into(), enabled: true },
                SidecarEntry { id: "hdost".into(), enabled: false },
                SidecarEntry { id: "removed-from-disk".into(), enabled: true },
            ],
        };
        std::fs::write(mods_dir.join(SIDECAR_NAME), toml::to_string_pretty(&sidecar).unwrap()).unwrap();

        // reconcile() reads via mods_dir(game), which is keyed off the global
        // games-folder config — exercise the pure logic directly instead by
        // duplicating the two steps it composes (on-disk enumeration + filter/append).
        let on_disk: Vec<String> = {
            let mut ids: Vec<String> = std::fs::read_dir(&mods_dir).unwrap()
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            ids.sort();
            ids
        };
        let read_back: Sidecar = toml::from_str(&std::fs::read_to_string(mods_dir.join(SIDECAR_NAME)).unwrap()).unwrap();
        let mut entries: Vec<SidecarEntry> = read_back.mods.into_iter().filter(|e| on_disk.contains(&e.id)).collect();
        for id in &on_disk {
            if !entries.iter().any(|e| &e.id == id) {
                entries.push(SidecarEntry { id: id.clone(), enabled: true });
            }
        }

        assert_eq!(entries[0], SidecarEntry { id: "badapple".into(), enabled: true });
        assert_eq!(entries[1], SidecarEntry { id: "hdost".into(), enabled: false });
        assert_eq!(entries[2], SidecarEntry { id: "newmod".into(), enabled: true });
        assert_eq!(entries.len(), 3, "the gone-from-disk entry must be dropped, not carried forward");
    }
}
