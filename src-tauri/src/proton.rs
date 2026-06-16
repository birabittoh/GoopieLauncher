//! Proton installation detection for Linux.
//!
//! Scans the standard Steam library locations for Proton installations (both
//! official Proton and community builds like GE-Proton) and exposes the list
//! to the bridge so the website can display a selector in Settings.
//!
//! All public items are present on every platform for bridge-compilation
//! purposes, but on non-Linux platforms `list_installations` always returns
//! an empty vec and the other helpers return `None`/empty.

/// A detected Proton installation.
#[derive(serde::Serialize)]
pub struct ProtonInstall {
    /// Human-readable name (contents of the `version` file if present,
    /// otherwise the directory name, e.g. "GE-Proton9-20" or "Proton 9.0").
    pub name: String,
    /// Absolute path to the directory that contains the `proton` launch script.
    pub path: String,
}

// ── Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::ProtonInstall;
    use std::path::{Path, PathBuf};

    /// Candidate roots that may contain Proton-like subdirectories.
    /// Called with the user's home directory so tests can inject a temp dir.
    pub fn scan_roots(home: &Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        // Well-known Steam data directories.
        let steam_roots = [
            home.join(".steam").join("steam"),
            home.join(".local").join("share").join("Steam"),
            home.join(".steam").join("root"),
        ];

        for steam in &steam_roots {
            // Official Proton releases live in steamapps/common.
            let common = steam.join("steamapps").join("common");
            if common.is_dir() {
                roots.push(common.clone());
            }
            // Extra Steam library folders from libraryfolders.vdf.
            let vdf = steam.join("steamapps").join("libraryfolders.vdf");
            if let Ok(contents) = std::fs::read_to_string(&vdf) {
                for extra in parse_library_paths(&contents) {
                    let extra_common = PathBuf::from(&extra).join("steamapps").join("common");
                    if extra_common.is_dir() && !roots.contains(&extra_common) {
                        roots.push(extra_common);
                    }
                }
            }
            // Custom compatibility tools (GE-Proton, etc.).
            let compat = steam.join("compatibilitytools.d");
            if compat.is_dir() {
                roots.push(compat);
            }
        }

        roots
    }

    /// Very lightweight VDF parser: extract all `"path"` values from
    /// `libraryfolders.vdf` without a full VDF parser dependency.
    fn parse_library_paths(contents: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut in_path_value = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case("\"path\"") {
                in_path_value = true;
                continue;
            }
            if in_path_value {
                // The next non-empty token after "path" is the value.
                if let Some(path) = extract_quoted(trimmed) {
                    paths.push(path);
                }
                in_path_value = false;
                continue;
            }
            // Handle inline `"path"  "value"` on the same line.
            if let Some(rest) = trimmed.strip_prefix("\"path\"") {
                if let Some(path) = extract_quoted(rest.trim()) {
                    paths.push(path);
                }
            }
        }
        paths
    }

    /// Extract the content of the first `"..."` in `s`.
    fn extract_quoted(s: &str) -> Option<String> {
        let start = s.find('"')? + 1;
        let rest = &s[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }

    /// Check whether `dir` is a valid Proton installation: it must contain an
    /// executable file named `proton`.
    fn is_proton_dir(dir: &Path) -> bool {
        let script = dir.join("proton");
        if !script.is_file() {
            return false;
        }
        // Verify the script is executable (Unix permissions).
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&script)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    /// Human-readable name for a Proton directory.
    ///
    /// Reads the `version` file if present (it typically contains a single line
    /// like `"Proton 9.0 (build 5208)"` or `"GE-Proton9-20"`); falls back to
    /// the directory name otherwise.
    fn proton_name(dir: &Path) -> String {
        if let Ok(v) = std::fs::read_to_string(dir.join("version")) {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.to_string_lossy().into_owned())
    }

    /// Scan `roots` for Proton installations and return them sorted newest-first
    /// (reverse-alphabetical by name, which works well for versioned names like
    /// `GE-Proton9-20` > `GE-Proton9-10`).
    pub fn find_in_roots(roots: &[PathBuf]) -> Vec<ProtonInstall> {
        let mut found: Vec<ProtonInstall> = Vec::new();
        let mut seen_paths: Vec<String> = Vec::new();

        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else { continue };
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                // Ignore directories whose name doesn't hint at Proton.
                let dir_name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if !dir_name.contains("proton") {
                    continue;
                }
                if !is_proton_dir(&dir) {
                    continue;
                }
                // Resolve symlinks so the same physical install isn't listed
                // multiple times: Steam roots (~/.steam/steam, ~/.steam/root,
                // ~/.local/share/Steam) are usually symlinks to one another, and
                // tools like ProtonPlus add "…Latest" symlinks pointing at the
                // real versioned dir. Canonicalizing collapses all of those to a
                // single entry under the original build's name/path.
                let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
                let canonical = real.to_string_lossy().into_owned();
                if seen_paths.contains(&canonical) {
                    continue;
                }
                seen_paths.push(canonical.clone());
                found.push(ProtonInstall {
                    name: proton_name(&real),
                    path: canonical,
                });
            }
        }

        // Reverse-alphabetical by name → newest GE-Proton / Proton first.
        found.sort_by(|a, b| b.name.cmp(&a.name));
        found
    }

    pub fn list_installations_impl() -> Vec<ProtonInstall> {
        let home = match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => PathBuf::from(h),
            _ => return Vec::new(),
        };
        let roots = scan_roots(&home);
        find_in_roots(&roots)
    }

    pub fn steam_client_install_path_impl() -> Option<String> {
        let home = PathBuf::from(std::env::var("HOME").ok()?);
        let candidates = [
            home.join(".steam").join("steam"),
            home.join(".local").join("share").join("Steam"),
            home.join(".steam").join("root"),
        ];
        for p in &candidates {
            if p.is_dir() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        fn make_proton_dir(root: &Path, name: &str, version: Option<&str>) {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            let script = dir.join("proton");
            fs::write(&script, "#!/bin/sh\nexec wine \"$@\"\n").unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            if let Some(v) = version {
                fs::write(dir.join("version"), v).unwrap();
            }
        }

        #[test]
        fn detects_proton_dirs_in_roots() {
            let tmp = tempfile::tempdir().unwrap();
            let common = tmp.path().join("steamapps").join("common");
            fs::create_dir_all(&common).unwrap();
            make_proton_dir(&common, "Proton 8.0", Some("Proton 8.0 (build 1234)"));
            make_proton_dir(&common, "GE-Proton9-20", None);
            // Non-Proton dir — should be ignored.
            fs::create_dir_all(common.join("Half-Life")).unwrap();

            let roots = vec![common];
            let installs = find_in_roots(&roots);
            assert_eq!(installs.len(), 2);
            // "Proton 8.0 (build 1234)" sorts first in reverse-alpha ('P' > 'G').
            assert!(installs[0].name.contains("Proton 8.0"));
            assert!(installs[1].name.contains("GE-Proton9-20"));
        }

        #[test]
        fn version_file_used_for_name() {
            let tmp = tempfile::tempdir().unwrap();
            make_proton_dir(tmp.path(), "Proton 9.0", Some("Proton 9.0 (build 5208)"));
            let installs = find_in_roots(&[tmp.path().to_path_buf()]);
            assert_eq!(installs.len(), 1);
            assert_eq!(installs[0].name, "Proton 9.0 (build 5208)");
        }

        #[test]
        fn ignores_non_executable_proton_script() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("Proton 7.0");
            fs::create_dir_all(&dir).unwrap();
            let script = dir.join("proton");
            fs::write(&script, "#!/bin/sh\n").unwrap();
            // 0o644 — not executable.
            fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
            let installs = find_in_roots(&[tmp.path().to_path_buf()]);
            assert!(installs.is_empty());
        }

        #[test]
        fn deduplicates_paths() {
            let tmp = tempfile::tempdir().unwrap();
            let common = tmp.path().join("steamapps").join("common");
            fs::create_dir_all(&common).unwrap();
            make_proton_dir(&common, "Proton 9.0", None);
            // Pass the same root twice.
            let roots = vec![common.clone(), common];
            let installs = find_in_roots(&roots);
            assert_eq!(installs.len(), 1);
        }

        #[test]
        fn deduplicates_symlinked_latest_build() {
            let tmp = tempfile::tempdir().unwrap();
            let common = tmp.path().join("steamapps").join("common");
            fs::create_dir_all(&common).unwrap();
            make_proton_dir(&common, "GE-Proton10-34", Some("GE-Proton10-34"));
            // ProtonPlus-style "Latest" symlink pointing at the real build dir.
            std::os::unix::fs::symlink(
                common.join("GE-Proton10-34"),
                common.join("Proton-GE Latest"),
            )
            .unwrap();

            let installs = find_in_roots(&[common]);
            assert_eq!(installs.len(), 1);
            // Listed under the real build, not the "Latest" alias.
            assert_eq!(installs[0].name, "GE-Proton10-34");
        }

        #[test]
        fn deduplicates_across_symlinked_roots() {
            let tmp = tempfile::tempdir().unwrap();
            let real_common = tmp.path().join("real").join("steamapps").join("common");
            fs::create_dir_all(&real_common).unwrap();
            make_proton_dir(&real_common, "GE-Proton10-34", Some("GE-Proton10-34"));
            // A second "root" that is really a symlink to the same common dir
            // (mirrors ~/.steam/steam vs ~/.local/share/Steam aliasing).
            let alias_common = tmp.path().join("alias-common");
            std::os::unix::fs::symlink(&real_common, &alias_common).unwrap();

            let installs = find_in_roots(&[real_common, alias_common]);
            assert_eq!(installs.len(), 1);
        }

        #[test]
        fn parse_library_paths_inline() {
            let vdf = r#"
"libraryfolders"
{
    "0"
    {
        "path"  "/extra/steam"
    }
}
"#;
            let paths = parse_library_paths(vdf);
            assert_eq!(paths, vec!["/extra/steam"]);
        }
    }
}

// ── Public API (cross-platform) ───────────────────────────────────────────────

/// Detect all Proton installations on the current system.
///
/// On non-Linux platforms, always returns an empty vec.
pub fn list_installations() -> Vec<ProtonInstall> {
    #[cfg(target_os = "linux")]
    { imp::list_installations_impl() }
    #[cfg(not(target_os = "linux"))]
    { Vec::new() }
}

/// Path to the Steam client installation directory, used as
/// `STEAM_COMPAT_CLIENT_INSTALL_PATH` when invoking Proton.
///
/// Returns `None` on non-Linux platforms or when Steam is not found.
// Called from games::play_with_proton which is #[cfg(target_os = "linux")],
// so it appears unused on other platforms.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn steam_client_install_path() -> Option<String> {
    #[cfg(target_os = "linux")]
    { imp::steam_client_install_path_impl() }
    #[cfg(not(target_os = "linux"))]
    { None }
}
