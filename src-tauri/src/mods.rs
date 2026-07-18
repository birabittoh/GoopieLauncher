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
///
/// Bumped to 2 for version-constrained `requires` entries (`"name >= x.y.z"`)
/// and the new `game_version` key — see `../rexglue-sdk/docs/mod-system.md`.
const CURRENT_MANIFEST_VERSION: u32 = 2;

/// Optional per-mod metadata (`mod.toml`). All fields are optional — a mod
/// with no manifest still works, falling back to its folder name and no
/// icon/description. `requires`/`conflicts`/`load_after`/`platform` are each a
/// comma-separated *string* in the file (matching the SDK's own
/// `enabled_mods` convention — see `../rexglue-sdk/docs/mod-system.md`), not a
/// TOML array; [`de_comma_list`] parses that shape into a `Vec<String>`.
#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default = "default_manifest_version")]
    manifest_version: u32,
    name: Option<String>,
    version: Option<String>,
    author: Option<String>,
    description: Option<String>,
    /// DLL/SO stem under `code/`; present => this is a code mod. Its actual
    /// value isn't used by the launcher (the SDK loads it by convention), only
    /// whether it's set.
    code: Option<String>,
    /// Hard dependency: must be enabled and ordered before this mod, or the
    /// SDK fails to start (see [`validate`]). Each entry may optionally pin a
    /// minimum version of the named mod (`"name >= 1.0.0"`).
    #[serde(default, deserialize_with = "de_requires")]
    requires: Vec<ModRequirement>,
    /// Hard mutual exclusion: the SDK fails to start if both are enabled.
    #[serde(default, deserialize_with = "de_comma_list")]
    conflicts: Vec<String>,
    /// Soft ordering hint: only warned about if violated, never blocks.
    #[serde(default, deserialize_with = "de_comma_list")]
    load_after: Vec<String>,
    /// Which platform(s) this code mod's `code/` currently ships a binary
    /// for (e.g. `"windows-x64,linux-x64"`), written by the mod's own build
    /// tooling. Meaningless for asset-only mods (no `code`).
    #[serde(default, deserialize_with = "de_comma_list")]
    platform: Vec<String>,
    /// Minimum host application version, e.g. `"1.2.0"` or `">= 1.2.0"` (both
    /// mean the same thing; no other comparison operator is supported). `None`
    /// when the key is absent. See [`parse_game_version_constraint`].
    game_version: Option<String>,
}

/// One `requires` entry: a dependency's folder name plus an optional
/// "must be at least this version" constraint (`"name >= 1.0.0"`). A bare
/// `"name"` (no `>=`) parses to `min_version: None`, meaning any enabled,
/// correctly-ordered version satisfies it — matching the SDK's
/// `rex::system::ModRequirement`.
#[derive(Debug, Clone, PartialEq)]
struct ModRequirement {
    name: String,
    min_version: Option<String>,
}

fn default_manifest_version() -> u32 {
    1
}

/// Parses a comma-separated list field (`requires = "a, b"`), the same shape
/// the SDK itself uses for `enabled_mods`/`requires`/`conflicts`/`load_after`
/// — trimmed of whitespace, empty segments dropped. Accepts a missing key
/// (via `#[serde(default, ...)]` on the field) or an explicit empty string,
/// both parsing to `vec![]`.
fn de_comma_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Ok(s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
}

/// Splits one `requires` entry into a mod name and an optional minimum-version
/// constraint: `"game_symbols >= 1.0.0"` -> `{name: "game_symbols",
/// min_version: Some("1.0.0")}`; a bare `"game_symbols"` -> `min_version:
/// None` (unconstrained). Mirrors the SDK's `ParseRequirement`.
fn parse_requirement(entry: &str) -> ModRequirement {
    match entry.split_once(">=") {
        None => ModRequirement { name: entry.trim().to_string(), min_version: None },
        Some((name, version)) => {
            let version = version.trim();
            ModRequirement {
                name: name.trim().to_string(),
                min_version: if version.is_empty() { None } else { Some(version.to_string()) },
            }
        }
    }
}

/// Renders a [`ModRequirement`] back to display form for [`ModInfo::requires`]
/// (`"name"` or `"name >= 1.0.0"`), the inverse of [`parse_requirement`].
fn format_requirement(req: &ModRequirement) -> String {
    match &req.min_version {
        Some(v) => format!("{} >= {}", req.name, v),
        None => req.name.clone(),
    }
}

/// Parses `requires` (a comma-separated list, each entry optionally
/// `"name >= x.y.z"`) into structured requirements. Builds on the same
/// comma-splitting as [`de_comma_list`].
fn de_requires<'de, D>(deserializer: D) -> Result<Vec<ModRequirement>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Ok(s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(parse_requirement).collect())
}

/// Parses `mod.toml`'s `game_version` key into a bare minimum-version string.
/// Both `"1.2.0"` and `">= 1.2.0"` mean the same thing ("must be at least
/// 1.2.0") — no other comparison operator is supported, matching the SDK's
/// `ParseGameVersionConstraint`. Returns `None` for an absent/blank key.
fn parse_game_version_constraint(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let value = value.strip_prefix(">=").map(str::trim).unwrap_or(value);
    if value.is_empty() { None } else { Some(value.to_string()) }
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
    pub conflicts: Vec<String>,
    pub load_after: Vec<String>,
    /// Platform target(s) this mod's `code/` ships a binary for (e.g.
    /// `["windows-x64", "linux-x64"]`). Always empty for asset-only mods.
    pub platform: Vec<String>,
    /// `true` when the manifest declares a `code` stem (a native DLL/SO mod).
    pub is_code: bool,
    pub enabled: bool,
    /// `data:image/png;base64,...` icon, or empty if the mod has no `icon.png`.
    pub icon: String,
    /// Minimum host application version this mod requires (e.g. `"1.2.0"`),
    /// or empty if the manifest declares no `game_version`. See [`validate`].
    pub game_version: String,
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

/// Lenient dotted-numeric version comparison, used both for mod versions
/// (which — unlike the launcher's own release tags — aren't guaranteed to be
/// well-formed semver) and installed-build version tags (which may carry a
/// leading `v`, e.g. `"v1.2.0"`). A leading `v`/`V` is stripped, then each
/// dot/dash/plus-separated segment is compared as a number (non-numeric or
/// missing segments count as 0), so `"1.2"` < `"1.10"` and `"1.0"` ==
/// `"1.0.0"`. A blank version is always the lowest. Mirrors the SDK's
/// `CompareVersions`/`ParseVersionComponents`.
pub(crate) fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn segments(s: &str) -> Vec<u64> {
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        s.split(['.', '-', '+'])
            .map(|seg| seg.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .map(|digits| digits.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let av = segments(a);
    let bv = segments(b);
    for i in 0..av.len().max(bv.len()) {
        let x = av.get(i).copied().unwrap_or(0);
        let y = bv.get(i).copied().unwrap_or(0);
        let ord = x.cmp(&y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Returns `true` when `new` is greater than or equal to `existing`,
/// including when both are equal or both blank (an unversioned mod is always
/// safe to re-drop over itself). Thin wrapper over [`cmp_versions`].
fn version_gte(new: &str, existing: &str) -> bool {
    cmp_versions(new, existing) != std::cmp::Ordering::Less
}

/// Whether `s` is a usable dotted-numeric version (optionally `v`-prefixed) —
/// every `.`-separated segment non-empty and all-ASCII-digit, mirroring the
/// SDK's `ParseVersionComponents`. Distinct from [`cmp_versions`], which is
/// lenient (missing/non-numeric segments count as 0) so it can always compare
/// two mod versions for the install-overwrite check; this stricter check
/// instead decides whether a `game_version`/`requires` version *constraint*
/// can be verified at all, per the SDK's "can't-verify -> warn, not error"
/// contract.
fn is_parseable_version(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    s.split('.').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

/// Formats a version string for display with exactly one leading "v",
/// regardless of whether `s` already has one (e.g. both "1.2.6" and
/// "v1.2.6" display as "v1.2.6").
fn display_version(s: &str) -> String {
    format!("v{}", s.trim().strip_prefix(['v', 'V']).unwrap_or(s.trim()))
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
                requires: manifest.requires.iter().map(format_requirement).collect(),
                conflicts: manifest.conflicts,
                load_after: manifest.load_after,
                platform: manifest.platform,
                is_code: manifest.code.map(|c| !c.is_empty()).unwrap_or(false),
                enabled: entry.enabled,
                icon: read_icon_data_url(game, &entry.id),
                game_version: parse_game_version_constraint(manifest.game_version.as_deref()).unwrap_or_default(),
            }
        })
        .collect()
}

/// Which OS this launcher is running on, in the prefix convention a code
/// mod's `platform` list uses (e.g. `"windows"`) — see the `platform`
/// section of `../rexglue-sdk/docs/mod-system.md`.
fn host_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

/// Which architecture this launcher is running on, in the suffix convention
/// a code mod's `platform` list uses (e.g. `"x64"`, `"arm64"`).
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The full platform id this launcher is running on (e.g. `"windows-x64"`,
/// `"windows-arm64"`), matching the exact id a code mod's `platform` list
/// entries use — see the `platform` section of `../rexglue-sdk/docs/mod-system.md`.
fn host_platform() -> String {
    format!("{}-{}", host_os(), host_arch())
}

/// One problem found by [`validate`]: `"error"` blocks launch, `"warning"` is
/// purely informational (matching the SDK's own `requires`-hard-fails /
/// `load_after`-only-warns distinction).
#[derive(Debug, Serialize)]
pub struct Issue {
    /// The mod id this issue is anchored to, for per-row UI highlighting.
    pub id: String,
    pub kind: &'static str,
    pub message: String,
}

/// Result of [`validate`]: `ok` is `true` iff there are no `"error"`-kind
/// issues, i.e. it's safe to build `--enabled_mods` and launch.
#[derive(Debug, Serialize)]
pub struct Validation {
    pub ok: bool,
    pub issues: Vec<Issue>,
}

fn err_issue(id: &str, message: String) -> Issue {
    Issue { id: id.to_string(), kind: "error", message }
}

fn warn_issue(id: &str, message: String) -> Issue {
    Issue { id: id.to_string(), kind: "warning", message }
}

/// Validate the *enabled* subset of `game`'s mods, singularly and together,
/// against the SDK's dependency rules (`requires`/`conflicts`/`load_after`,
/// see `../rexglue-sdk/docs/mod-system.md`), plus a launcher-specific check
/// that a code mod actually ships a binary for this host OS. Disabled mods
/// are never inspected — disabling (or deleting, or updating) a broken mod is
/// always a valid way to resolve it, matching the SDK's own semantics where
/// only `enabled_mods` is checked.
///
/// `installed_game_version` is the version of the game build being checked
/// against (see `games::installed_game_version`/`games::get_installed_version`),
/// used to validate each enabled mod's `game_version` constraint. Pass an
/// empty string if it's unknown (e.g. nothing installed yet) — that's treated
/// the same as an unset `RuntimeConfig::game_version` on the SDK side: any
/// mod declaring `game_version` gets a can't-verify warning, never an error.
pub fn validate(game: &str, installed_game_version: &str) -> Validation {
    let entries = reconcile(game);
    let enabled: Vec<(String, Manifest)> = entries
        .iter()
        .filter(|e| e.enabled)
        .map(|e| (e.id.clone(), read_manifest(game, &e.id)))
        .collect();
    validate_enabled(&enabled, installed_game_version)
}

/// Pure core of [`validate`]: takes the already-resolved enabled mods (in
/// priority order) and their manifests, decoupled from disk/config so it's
/// unit-testable without a games-folder fixture.
fn validate_enabled(enabled: &[(String, Manifest)], installed_game_version: &str) -> Validation {
    let index_of = |id: &str| enabled.iter().position(|(mid, _)| mid == id);
    let host = host_platform();

    let mut issues = Vec::new();

    for (i, (id, m)) in enabled.iter().enumerate() {
        if m.requires.iter().any(|r| r.name == *id) {
            issues.push(err_issue(id, format!("\"{id}\" lists itself in requires — remove the self-reference.")));
        }
        if m.conflicts.iter().any(|c| c == id) {
            issues.push(err_issue(id, format!("\"{id}\" lists itself in conflicts — remove the self-reference.")));
        }

        let is_code = m.code.as_deref().is_some_and(|c| !c.is_empty());
        if is_code {
            if m.platform.is_empty() {
                issues.push(err_issue(id, format!(
                    "\"{id}\" is a code mod but declares no platform binaries; it can't load. Update or remove it."
                )));
            } else if !m.platform.iter().any(|p| *p == host) {
                issues.push(err_issue(id, format!(
                    "\"{id}\" has no binary for this platform (ships: {}). Update, disable, or remove it.",
                    m.platform.join(", ")
                )));
            }
        }

        for req in &m.requires {
            if req.name == *id {
                continue; // already reported above
            }
            match index_of(&req.name) {
                None => issues.push(err_issue(id, format!(
                    "\"{id}\" requires \"{}\", which isn't enabled. Enable/install it, or disable \"{id}\".", req.name
                ))),
                Some(j) if j > i => issues.push(err_issue(id, format!(
                    "\"{}\" must load before \"{id}\". Click Auto-sort, or drag it higher.", req.name
                ))),
                Some(j) => {
                    // Correctly enabled and ordered — check the optional
                    // version pin, if any. A constraint that can't be
                    // verified (either side isn't a valid dotted version, or
                    // the dependency has no `version` at all) is accepted
                    // with a warning rather than blocking, mirroring the
                    // SDK's `ValidateModDependencies`.
                    if let Some(min_version) = &req.min_version {
                        let (dep_id, dep_manifest) = &enabled[j];
                        let dep_version = dep_manifest.version.as_deref().unwrap_or("");
                        if !is_parseable_version(min_version) {
                            issues.push(warn_issue(id, format!(
                                "\"{id}\" requires \"{dep_id}\" >= {min_version}, but \"{min_version}\" isn't a valid version (e.g. \"1.0.0\") — can't verify."
                            )));
                        } else if !is_parseable_version(dep_version) {
                            issues.push(warn_issue(id, format!(
                                "\"{id}\" requires \"{dep_id}\" >= {min_version}, but \"{dep_id}\" has no valid version in its mod.toml — can't verify."
                            )));
                        } else if cmp_versions(dep_version, min_version) == std::cmp::Ordering::Less {
                            issues.push(err_issue(id, format!(
                                "\"{id}\" requires \"{dep_id}\" >= {min_version}, but the enabled \"{dep_id}\" is only version {dep_version}."
                            )));
                        }
                    }
                }
            }
        }

        if let Some(min_version) = parse_game_version_constraint(m.game_version.as_deref()) {
            if !is_parseable_version(&min_version) {
                issues.push(warn_issue(id, format!(
                    "\"{id}\" has game_version = \"{min_version}\", which isn't a valid version (e.g. \"1.0.0\") — can't verify."
                )));
            } else if !is_parseable_version(installed_game_version) {
                issues.push(warn_issue(id, format!(
                    "\"{id}\" requires game {} or newer, but the installed game version is unknown — can't verify.",
                    display_version(&min_version)
                )));
            } else {
                match cmp_versions(installed_game_version, &min_version) {
                    std::cmp::Ordering::Less => issues.push(err_issue(id, format!(
                        "\"{id}\" requires game {} or newer; the installed game is {}. Update the game, or disable this mod.",
                        display_version(&min_version), display_version(installed_game_version)
                    ))),
                    std::cmp::Ordering::Greater => issues.push(warn_issue(id, format!(
                        "\"{id}\" targets game {}; the installed game is {}, which may not be fully compatible.",
                        display_version(&min_version), display_version(installed_game_version)
                    ))),
                    std::cmp::Ordering::Equal => {}
                }
            }
        }

        for after in &m.load_after {
            if after == id {
                continue;
            }
            match index_of(after) {
                None => issues.push(warn_issue(id, format!(
                    "\"{id}\" works better loaded after \"{after}\", which isn't enabled."
                ))),
                Some(j) if j > i => issues.push(warn_issue(id, format!(
                    "\"{id}\" works better loaded after \"{after}\" (currently loads first)."
                ))),
                _ => {}
            }
        }
    }

    // `conflicts` is a hard error regardless of order or which side declares
    // it — collect as unordered pairs first so a mutual (or one-sided)
    // declaration only produces one issue per mod, not a duplicate.
    let mut conflict_pairs: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for (id, m) in enabled {
        for conf in &m.conflicts {
            if conf != id && index_of(conf).is_some() {
                let pair = if id < conf { (id.clone(), conf.clone()) } else { (conf.clone(), id.clone()) };
                conflict_pairs.insert(pair);
            }
        }
    }
    for (a, b) in &conflict_pairs {
        issues.push(err_issue(a, format!("\"{a}\" conflicts with \"{b}\". Disable or remove one of them.")));
        issues.push(err_issue(b, format!("\"{b}\" conflicts with \"{a}\". Disable or remove one of them.")));
    }

    let ok = !issues.iter().any(|i| i.kind == "error");
    Validation { ok, issues }
}

/// Stable dependency-respecting sort of `entries`: a mod named in another
/// enabled mod's `requires` or `load_after` moves before it, resolving the
/// order-type issues [`validate`] reports. Preserves existing relative order
/// otherwise (a stable topological sort), so a user's manual priority survives
/// except where a dependency forces a move. Disabled entries are left in
/// their original absolute slot — only the *enabled* subset participates in
/// the dependency graph, mirroring the SDK's "requires/load_after are checked
/// against the `enabled_mods` index" contract. A cycle (only possible via
/// `load_after`; a `requires` cycle can't exist per the SDK doc) is broken by
/// falling back to original order for whichever node is stuck — the residual
/// still surfaces as a `load_after` warning, never blocks.
fn sort_entries(game: &str, entries: Vec<SidecarEntry>) -> Vec<SidecarEntry> {
    let enabled_ids: Vec<String> = entries.iter().filter(|e| e.enabled).map(|e| e.id.clone()).collect();
    let manifests: std::collections::HashMap<String, Manifest> =
        enabled_ids.iter().map(|id| (id.clone(), read_manifest(game, id))).collect();
    sort_entries_with(entries, &manifests)
}

/// Pure core of [`sort_entries`]: takes pre-fetched manifests, decoupled from
/// disk/config so it's unit-testable without a games-folder fixture.
fn sort_entries_with(entries: Vec<SidecarEntry>, manifests: &std::collections::HashMap<String, Manifest>) -> Vec<SidecarEntry> {
    let enabled_ids: Vec<String> = entries.iter().filter(|e| e.enabled).map(|e| e.id.clone()).collect();

    let default_manifest = Manifest::default();

    // Edge "dep -> mod" for each requires/load_after target that's also enabled.
    let deps: std::collections::HashMap<&str, Vec<&str>> = enabled_ids
        .iter()
        .map(|id| {
            let m = manifests.get(id).unwrap_or(&default_manifest);
            let wants: Vec<&str> = m
                .requires
                .iter()
                .map(|r| r.name.as_str())
                .chain(m.load_after.iter().map(|s| s.as_str()))
                .filter(|r| *r != id && enabled_ids.iter().any(|e| e == *r))
                .collect();
            (id.as_str(), wants)
        })
        .collect();

    // Stable Kahn's-algorithm topo sort: repeatedly take the earliest
    // (by original order) node whose dependencies are all already placed;
    // if none is ready (a cycle), take the earliest remaining node anyway so
    // sorting always terminates instead of hanging or erroring.
    let mut remaining: Vec<&str> = enabled_ids.iter().map(|s| s.as_str()).collect();
    let mut placed: Vec<&str> = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let idx = remaining
            .iter()
            .position(|id| deps[id].iter().all(|dep| placed.contains(dep)))
            .unwrap_or(0);
        placed.push(remaining.remove(idx));
    }

    // Rebuild the full list: enabled slots take the new order from `placed`;
    // disabled entries keep their original absolute position.
    let mut placed_iter = placed.into_iter();
    entries
        .into_iter()
        .map(|e| {
            if e.enabled {
                let id = placed_iter.next().expect("placed has one entry per enabled id");
                SidecarEntry { id: id.to_string(), enabled: true }
            } else {
                e
            }
        })
        .collect()
}

/// Reorder `game`'s current mod list to satisfy dependency ordering
/// (`requires`/`load_after`) as far as possible, and persist it. Used by the
/// Mods panel's "Auto-sort" button, and called automatically after every mod
/// install (see [`install_archives`]) so dropping several mods at once (e.g.
/// a mod and the library it requires) lands them in a valid order without the
/// player manually dragging rows.
pub fn auto_sort(game: &str) {
    let entries = reconcile(game);
    let sorted = sort_entries(game, entries);
    write_sidecar(game, &Sidecar { mods: sorted });
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
/// `desired_id`, when set, overrides the zip/top-level-dir-derived id
/// entirely (sanitised the same way) — used by [`install_from_url`] so a mod
/// installed from a catalog entry always lands under the catalog's
/// deterministic `modId`, regardless of what the release's zip happens to be
/// named/structured like.
///
/// If a mod with the same id is already installed, the new one replaces it
/// when its `mod.toml` `version` is greater than or equal to the installed
/// one's (see [`version_gte`]) — otherwise the install is rejected so an
/// older drop can't clobber a newer install.
fn install_one_archive(mods_dir: &std::path::Path, zip_path: &str, desired_id: Option<&str>) -> std::io::Result<InstalledMod> {
    std::fs::create_dir_all(mods_dir)?;
    let staging = tempfile::Builder::new()
        .prefix(".mod-extract-")
        .tempdir_in(mods_dir)?;

    crate::archive::extract_zip(zip_path, &staging.path().to_string_lossy())?;

    let top_level: Vec<std::fs::DirEntry> = std::fs::read_dir(staging.path())?.filter_map(|e| e.ok()).collect();

    let (derived_id, content_src): (String, PathBuf) = if top_level.len() == 1 && top_level[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
        let name = top_level[0].file_name().to_string_lossy().into_owned();
        (sanitize_mod_id(&name), top_level[0].path())
    } else {
        let stem = std::path::Path::new(zip_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mod".to_string());
        (sanitize_mod_id(&stem), staging.path().to_path_buf())
    };
    let id = match desired_id {
        Some(d) => sanitize_mod_id(d),
        None => derived_id,
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

        match install_one_archive(&dir, path, None) {
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

    // Auto-sort so a multi-mod drop (e.g. a mod alongside the library it
    // `requires`) lands in a dependency-valid order without the player having
    // to manually drag rows — see `sort_entries`.
    let sorted = sort_entries(game, entries);
    write_sidecar(game, &Sidecar { mods: sorted });

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

/// Download a zip from `url` (e.g. a GitHub release asset's
/// `browser_download_url`) into a temp file, wrapping [`download::download_file`]
/// with a caller-supplied progress callback and always cleaning up the temp
/// file afterward regardless of outcome.
fn download_to_temp_zip(url: &str, progress: Option<&crate::download::ProgressCallback>) -> Result<tempfile::TempPath, String> {
    let tmp = tempfile::Builder::new()
        .prefix("goopie-mod-")
        .suffix(".zip")
        .tempfile()
        .map_err(|e| format!("failed to create temp file: {e}"))?;
    let path = tmp.into_temp_path();
    crate::download::download_file(url, &path.to_string_lossy(), progress)
        .map_err(|e| format!("download failed: {e}"))?;
    Ok(path)
}

/// Download a mod zip from `url` and install it under `game`'s `mods/`,
/// forcing the extracted folder's id to `desired_id` (see [`install_one_archive`])
/// instead of deriving it from the zip/top-level-dir name — used for
/// catalog-sourced installs, where the catalog's deterministic `modId` is how
/// installed state is correlated back to the Firestore entry. Auto-sorts
/// afterward, same as [`install_archives`]. Returns a single-entry
/// [`InstallReport`] so callers can reuse the same report shape/UI as a local
/// zip install.
///
/// `expected_checksum`, when set, is the SHA-256 hex digest the catalog
/// recorded for this asset at approval time (see [`compute_url_checksum`]).
/// The downloaded bytes are hashed and compared before extraction, so a
/// release asset swapped out after approval (e.g. a compromised GitHub repo)
/// is rejected instead of silently installed.
pub fn install_from_url(game: &str, url: &str, desired_id: &str, expected_checksum: Option<&str>) -> InstallReport {
    install_from_url_with_progress(game, url, desired_id, expected_checksum, None)
}

fn install_from_url_with_progress(
    game: &str,
    url: &str,
    desired_id: &str,
    expected_checksum: Option<&str>,
    progress: Option<&crate::download::ProgressCallback>,
) -> InstallReport {
    let dir = mods_dir(game);
    let mut entries = reconcile(game);

    let zip_path = match download_to_temp_zip(url, progress) {
        Ok(path) => path,
        Err(e) => {
            return InstallReport {
                results: vec![InstallResult { path: url.to_string(), ok: false, message: e }],
            };
        }
    };

    if let Some(expected) = expected_checksum {
        match crate::download::sha256_file(&zip_path.to_string_lossy()) {
            Some(actual) if actual.eq_ignore_ascii_case(expected) => {}
            Some(actual) => {
                return InstallReport {
                    results: vec![InstallResult {
                        path: url.to_string(),
                        ok: false,
                        message: format!(
                            "checksum mismatch: expected {expected}, got {actual} — the release asset may have changed since this mod was approved"
                        ),
                    }],
                };
            }
            None => {
                return InstallReport {
                    results: vec![InstallResult { path: url.to_string(), ok: false, message: "failed to hash downloaded file".to_string() }],
                };
            }
        }
    }

    let result = match install_one_archive(&dir, &zip_path.to_string_lossy(), Some(desired_id)) {
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
            InstallResult { path: url.to_string(), ok: true, message }
        }
        Err(e) => InstallResult { path: url.to_string(), ok: false, message: e.to_string() },
    };

    let sorted = sort_entries(game, entries);
    write_sidecar(game, &Sidecar { mods: sorted });

    InstallReport { results: vec![result] }
}

/// Run [`install_from_url`] on a background thread, publishing the result to
/// `state.mod_install_report` and toggling `state.mod_installing` around it —
/// same pattern as [`install_archives_async`]. Additionally drives
/// `state.download_progress`/`download_string` (the same channel
/// `games::update` uses, polled via `getDownloadProgress`) while the zip
/// downloads, since unlike a local sideload this involves a network transfer
/// worth showing progress for.
pub fn install_from_url_async(state: std::sync::Arc<crate::AppState>, game: String, url: String, desired_id: String, expected_checksum: Option<String>) {
    state.mod_installing.store(true, std::sync::atomic::Ordering::Relaxed);
    state.download_progress.store(0, std::sync::atomic::Ordering::Relaxed);

    let state_cb = std::sync::Arc::clone(&state);
    let progress_cb: crate::download::ProgressCallback = Box::new(move |dl, tot| {
        state_cb.set_download_progress(dl, tot);
    });

    let report = install_from_url_with_progress(&game, &url, &desired_id, expected_checksum.as_deref(), Some(&progress_cb));

    state.finish_download();
    *state.mod_install_report.lock().unwrap() = Some(report);
    state.mod_installing.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Downloads the asset at `url` fresh and returns its SHA-256 hex digest,
/// without extracting or installing anything. Called at approve/accept-update
/// time to stamp a catalog mod's `checksum` from the exact bytes an admin/dev
/// reviewed, so [`install_from_url`] can later detect a release asset that
/// was swapped out from under an already-approved mod.
pub fn compute_url_checksum(url: &str) -> Result<String, String> {
    let zip_path = download_to_temp_zip(url, None)?;
    crate::download::sha256_file(&zip_path.to_string_lossy())
        .ok_or_else(|| "failed to hash downloaded file".to_string())
}

/// Metadata read from a mod's `mod.toml`/`icon.png` without installing it —
/// used to auto-fill a catalog submission's display fields from the actual
/// release asset. Mirrors the fields of [`ModInfo`] that come purely from the
/// manifest (no id/enabled/state, which don't exist yet for an uninstalled
/// mod).
#[derive(Debug, Serialize)]
pub struct ModMetadata {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub platform: Vec<String>,
    pub requires: Vec<String>,
    /// `data:image/png;base64,...` icon, or empty if the archive has no `icon.png`.
    pub icon: String,
    /// Minimum host application version this mod requires (e.g. `"1.2.0"`),
    /// or empty if the manifest declares no `game_version`. See
    /// [`parse_game_version_constraint`].
    pub game_version: String,
}

/// Download the zip at `url` to a temp file, extract it to a temp dir, and
/// read its `mod.toml`/`icon.png` without installing anything permanently —
/// used at mod-submission time to auto-fill a catalog entry's metadata from
/// the actual release asset. Locates the manifest the same way
/// [`install_one_archive`] locates the mod's content root (a single
/// top-level directory, or the extracted tree itself if flat), so metadata
/// matches what would actually get installed.
pub fn fetch_metadata(url: &str) -> Result<ModMetadata, String> {
    let zip_path = download_to_temp_zip(url, None)?;

    let extract_dir = tempfile::Builder::new()
        .prefix("goopie-mod-meta-")
        .tempdir()
        .map_err(|e| format!("failed to create temp dir: {e}"))?;

    crate::archive::extract_zip(&zip_path.to_string_lossy(), &extract_dir.path().to_string_lossy())
        .map_err(|e| format!("failed to extract zip: {e}"))?;

    let top_level: Vec<std::fs::DirEntry> = std::fs::read_dir(extract_dir.path())
        .map_err(|e| format!("failed to read extracted zip: {e}"))?
        .filter_map(|e| e.ok())
        .collect();

    let content_root: PathBuf = if top_level.len() == 1 && top_level[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
        top_level[0].path()
    } else {
        extract_dir.path().to_path_buf()
    };

    let manifest = read_manifest_at(&content_root.join(MANIFEST_NAME));
    let icon_path = content_root.join(ICON_NAME);
    let icon = std::fs::read(&icon_path)
        .map(|bytes| format!("data:image/png;base64,{}", B64.encode(bytes)))
        .unwrap_or_default();

    Ok(ModMetadata {
        name: manifest.name.unwrap_or_default(),
        version: manifest.version.unwrap_or_default(),
        author: manifest.author.unwrap_or_default(),
        description: manifest.description.unwrap_or_default(),
        platform: manifest.platform,
        requires: manifest.requires.iter().map(format_requirement).collect(),
        icon,
        game_version: parse_game_version_constraint(manifest.game_version.as_deref()).unwrap_or_default(),
    })
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

        let installed = install_one_archive(&mods_dir, &zip_path.to_string_lossy(), None).unwrap();
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

        let installed = install_one_archive(&mods_dir, &zip_path.to_string_lossy(), None).unwrap();
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

        let err = install_one_archive(&mods_dir, &zip_path.to_string_lossy(), None).unwrap_err();
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

        let installed = install_one_archive(&mods_dir, &zip_path.to_string_lossy(), None).unwrap();
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
    fn cmp_versions_strips_a_leading_v_prefix() {
        use std::cmp::Ordering;
        assert_eq!(cmp_versions("v1.2.0", "1.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("V1.2.0", "1.1.0"), Ordering::Greater);
        assert_eq!(cmp_versions("1.0", "1.0.0"), Ordering::Equal, "missing trailing components count as 0");
    }

    #[test]
    fn is_parseable_version_accepts_dotted_numerics_and_rejects_everything_else() {
        assert!(is_parseable_version("1.0.0"));
        assert!(is_parseable_version("v1.2"));
        assert!(!is_parseable_version(""));
        assert!(!is_parseable_version("   "));
        assert!(!is_parseable_version("not-a-version"));
        assert!(!is_parseable_version("1.0.0-beta"), "a dash-qualified version isn't a plain dotted numeric");
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

    #[test]
    fn de_comma_list_parses_a_comma_separated_string_field() {
        let m: Manifest = toml::from_str("requires = \"game_symbols, other_mod\"\nconflicts = \"\"\n").unwrap();
        assert_eq!(m.requires, vec![
            ModRequirement { name: "game_symbols".to_string(), min_version: None },
            ModRequirement { name: "other_mod".to_string(), min_version: None },
        ]);
        assert_eq!(m.conflicts, Vec::<String>::new(), "an empty string parses to an empty list, not [\"\"]");
    }

    #[test]
    fn de_requires_parses_a_version_constrained_entry() {
        let m: Manifest = toml::from_str("requires = \"game_symbols >= 1.0.0, other_mod\"\n").unwrap();
        assert_eq!(m.requires, vec![
            ModRequirement { name: "game_symbols".to_string(), min_version: Some("1.0.0".to_string()) },
            ModRequirement { name: "other_mod".to_string(), min_version: None },
        ]);
    }

    #[test]
    fn parse_game_version_constraint_accepts_both_forms() {
        assert_eq!(parse_game_version_constraint(Some("1.2.0")), Some("1.2.0".to_string()));
        assert_eq!(parse_game_version_constraint(Some(">= 1.2.0")), Some("1.2.0".to_string()));
        assert_eq!(parse_game_version_constraint(Some("  ")), None);
        assert_eq!(parse_game_version_constraint(None), None);
    }

    #[test]
    fn de_comma_list_defaults_to_empty_when_the_key_is_absent() {
        let m: Manifest = toml::from_str("name = \"Sample\"\n").unwrap();
        assert!(m.requires.is_empty());
        assert!(m.conflicts.is_empty());
        assert!(m.load_after.is_empty());
        assert!(m.platform.is_empty());
    }

    /// Builds a test `Manifest`. Each `requires` entry may be a bare id
    /// (`"game_symbols"`) or carry a version pin (`"game_symbols >= 1.0.0"`),
    /// parsed the same way [`de_requires`] parses the real `mod.toml` field.
    fn manifest(code: Option<&str>, requires: &[&str], conflicts: &[&str], load_after: &[&str], platform: &[&str]) -> Manifest {
        manifest_versioned(code, requires, conflicts, load_after, platform, None, None)
    }

    /// Full-control variant of [`manifest`] that also sets `version` and
    /// `game_version`, for the version-constraint tests.
    fn manifest_versioned(
        code: Option<&str>,
        requires: &[&str],
        conflicts: &[&str],
        load_after: &[&str],
        platform: &[&str],
        version: Option<&str>,
        game_version: Option<&str>,
    ) -> Manifest {
        Manifest {
            code: code.map(str::to_string),
            version: version.map(str::to_string),
            requires: requires.iter().map(|s| parse_requirement(s)).collect(),
            conflicts: conflicts.iter().map(|s| s.to_string()).collect(),
            load_after: load_after.iter().map(|s| s.to_string()).collect(),
            platform: platform.iter().map(|s| s.to_string()).collect(),
            game_version: game_version.map(str::to_string),
            ..Manifest::default()
        }
    }

    fn has_error(v: &Validation, id: &str) -> bool {
        v.issues.iter().any(|i| i.kind == "error" && i.id == id)
    }

    fn has_warning(v: &Validation, id: &str) -> bool {
        v.issues.iter().any(|i| i.kind == "warning" && i.id == id)
    }

    #[test]
    fn validate_enabled_passes_a_clean_asset_only_layout() {
        let enabled = vec![
            ("badapple".to_string(), manifest(None, &[], &[], &[], &[])),
            ("hdost".to_string(), manifest(None, &[], &[], &[], &[])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok);
        assert!(v.issues.is_empty());
    }

    #[test]
    fn validate_enabled_flags_a_missing_requires_target() {
        // ui_color requires game_symbols, but only ui_color is enabled.
        let enabled = vec![("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols"], &[], &[], &[host_os_platform()]))];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "ui_color"));
    }

    #[test]
    fn validate_enabled_flags_a_requires_ordered_after_its_dependency() {
        // ui_color (index 0) requires game_symbols (index 1) -- wrong order.
        let enabled = vec![
            ("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols"], &[], &[], &[host_os_platform()])),
            ("game_symbols".to_string(), manifest(Some("game_symbols"), &[], &[], &[], &[host_os_platform()])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "ui_color"));
    }

    #[test]
    fn validate_enabled_passes_requires_in_correct_order() {
        // game_symbols (index 0) loads before ui_color (index 1) -- correct.
        let enabled = vec![
            ("game_symbols".to_string(), manifest(Some("game_symbols"), &[], &[], &[], &[host_os_platform()])),
            ("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols"], &[], &[], &[host_os_platform()])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "issues: {:?}", v.issues);
    }

    #[test]
    fn validate_enabled_flags_mutual_conflicts_on_both_mods() {
        let enabled = vec![
            ("mod_a".to_string(), manifest(None, &[], &["mod_b"], &[], &[])),
            ("mod_b".to_string(), manifest(None, &[], &[], &[], &[])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "mod_a"));
        assert!(has_error(&v, "mod_b"), "both sides of a conflict should be highlighted, even if only one declares it");
    }

    #[test]
    fn validate_enabled_treats_load_after_violations_as_warnings_only() {
        // mod_a wants to load after mod_b, but currently loads first -- a warning, not a block.
        let enabled = vec![
            ("mod_a".to_string(), manifest(None, &[], &[], &["mod_b"], &[])),
            ("mod_b".to_string(), manifest(None, &[], &[], &[], &[])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "load_after must never block launch");
        assert!(has_warning(&v, "mod_a"));
    }

    #[test]
    fn validate_enabled_flags_self_reference_in_requires_and_conflicts() {
        let enabled = vec![("mod_a".to_string(), manifest(None, &["mod_a"], &["mod_a"], &[], &[]))];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert_eq!(v.issues.iter().filter(|i| i.kind == "error").count(), 2);
    }

    #[test]
    fn validate_enabled_errors_when_installed_game_is_older_than_game_version() {
        let enabled = vec![("mod_a".to_string(), manifest_versioned(None, &[], &[], &[], &[], None, Some("1.2.0")))];
        let v = validate_enabled(&enabled, "1.0.0");
        assert!(!v.ok);
        assert!(has_error(&v, "mod_a"));
    }

    #[test]
    fn validate_enabled_warns_when_installed_game_is_newer_than_game_version() {
        let enabled = vec![("mod_a".to_string(), manifest_versioned(None, &[], &[], &[], &[], None, Some("1.0.0")))];
        let v = validate_enabled(&enabled, "1.2.0");
        assert!(v.ok, "a newer game than a mod targets must never block launch");
        assert!(has_warning(&v, "mod_a"));
    }

    #[test]
    fn validate_enabled_passes_when_installed_game_matches_game_version_exactly() {
        let enabled = vec![("mod_a".to_string(), manifest_versioned(None, &[], &[], &[], &[], None, Some("1.0.0")))];
        let v = validate_enabled(&enabled, "1.0.0");
        assert!(v.ok);
        assert!(v.issues.is_empty());
    }

    #[test]
    fn validate_enabled_treats_bare_game_version_the_same_as_explicit_gte() {
        let a = manifest_versioned(None, &[], &[], &[], &[], None, Some("1.2.0"));
        let b = manifest_versioned(None, &[], &[], &[], &[], None, Some(">= 1.2.0"));
        let va = validate_enabled(&[("mod_a".to_string(), a)], "1.0.0");
        let vb = validate_enabled(&[("mod_a".to_string(), b)], "1.0.0");
        assert!(!va.ok && !vb.ok, "\"1.2.0\" and \">= 1.2.0\" must behave identically");
    }

    #[test]
    fn validate_enabled_warns_instead_of_erroring_when_installed_game_version_is_unknown() {
        let enabled = vec![("mod_a".to_string(), manifest_versioned(None, &[], &[], &[], &[], None, Some("1.2.0")))];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "an unknown installed version must never block launch");
        assert!(has_warning(&v, "mod_a"));
    }

    #[test]
    fn validate_enabled_warns_instead_of_erroring_on_an_unparseable_game_version_constraint() {
        let enabled = vec![("mod_a".to_string(), manifest_versioned(None, &[], &[], &[], &[], None, Some("not-a-version")))];
        let v = validate_enabled(&enabled, "1.0.0");
        assert!(v.ok);
        assert!(has_warning(&v, "mod_a"));
    }

    #[test]
    fn validate_enabled_errors_when_a_required_mods_version_is_too_old() {
        // ui_color requires game_symbols >= 2.0.0, but the enabled game_symbols is only 1.0.0.
        let enabled = vec![
            ("game_symbols".to_string(), manifest_versioned(Some("game_symbols"), &[], &[], &[], &[host_os_platform()], Some("1.0.0"), None)),
            ("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols >= 2.0.0"], &[], &[], &[host_os_platform()])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "ui_color"));
    }

    #[test]
    fn validate_enabled_passes_when_a_required_mods_version_satisfies_the_constraint() {
        let enabled = vec![
            ("game_symbols".to_string(), manifest_versioned(Some("game_symbols"), &[], &[], &[], &[host_os_platform()], Some("1.0.0"), None)),
            ("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols >= 1.0.0"], &[], &[], &[host_os_platform()])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "issues: {:?}", v.issues);
    }

    #[test]
    fn validate_enabled_warns_instead_of_erroring_when_a_required_mod_has_no_version() {
        // game_symbols has no `version` key at all -- can't verify the >= 1.0.0 pin.
        let enabled = vec![
            ("game_symbols".to_string(), manifest(Some("game_symbols"), &[], &[], &[], &[host_os_platform()])),
            ("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols >= 1.0.0"], &[], &[], &[host_os_platform()])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "a mod predating versioned requires must not block launch");
        assert!(has_warning(&v, "ui_color"));
    }

    #[test]
    fn validate_enabled_leaves_a_bare_requires_entry_unconstrained() {
        // No `>=` at all -- any enabled, correctly-ordered version satisfies it (unchanged behavior).
        let enabled = vec![
            ("game_symbols".to_string(), manifest(Some("game_symbols"), &[], &[], &[], &[host_os_platform()])),
            ("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols"], &[], &[], &[host_os_platform()])),
        ];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "issues: {:?}", v.issues);
    }

    #[test]
    fn validate_enabled_flags_a_code_mod_with_no_platform_binaries() {
        let enabled = vec![("some_mod".to_string(), manifest(Some("some_mod"), &[], &[], &[], &[]))];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "some_mod"));
    }

    #[test]
    fn validate_enabled_flags_a_code_mod_missing_this_hosts_binary() {
        // Ships a binary, but never for the host this test runs on.
        let other_os = if host_os() == "windows" { "linux-x64" } else { "windows-x64" };
        let enabled = vec![("some_mod".to_string(), manifest(Some("some_mod"), &[], &[], &[], &[other_os]))];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "some_mod"));
    }

    #[test]
    fn validate_enabled_flags_a_code_mod_with_only_the_wrong_arch_for_this_os() {
        // Same OS, but the only binary shipped is for the other architecture.
        let wrong_arch = if host_arch() == "x64" { "arm64" } else { "x64" };
        let other_arch_platform = format!("{}-{}", host_os(), wrong_arch);
        let enabled = vec![("some_mod".to_string(), manifest(Some("some_mod"), &[], &[], &[], &[&other_arch_platform]))];
        let v = validate_enabled(&enabled, "");
        assert!(!v.ok);
        assert!(has_error(&v, "some_mod"));
    }

    #[test]
    fn validate_enabled_passes_a_code_mod_shipping_both_arches_for_this_os() {
        let wrong_arch = if host_arch() == "x64" { "arm64" } else { "x64" };
        let other_arch_platform = format!("{}-{}", host_os(), wrong_arch);
        let platform = host_platform();
        let enabled = vec![("some_mod".to_string(), manifest(Some("some_mod"), &[], &[], &[], &[&platform, &other_arch_platform]))];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok, "issues: {:?}", v.issues);
    }

    #[test]
    fn validate_enabled_passes_an_asset_only_mod_with_no_platform_declared() {
        // No `code` key at all -- platform is simply irrelevant, not an error.
        let enabled = vec![("badapple".to_string(), manifest(None, &[], &[], &[], &[]))];
        let v = validate_enabled(&enabled, "");
        assert!(v.ok);
    }

    fn host_os_platform() -> &'static str {
        match (host_os(), host_arch()) {
            ("windows", "arm64") => "windows-arm64",
            ("windows", _) => "windows-x64",
            ("macos", "arm64") => "macos-arm64",
            ("macos", _) => "macos-x64",
            (_, "arm64") => "linux-arm64",
            _ => "linux-x64",
        }
    }

    fn entries(pairs: &[(&str, bool)]) -> Vec<SidecarEntry> {
        pairs.iter().map(|(id, enabled)| SidecarEntry { id: id.to_string(), enabled: *enabled }).collect()
    }

    #[test]
    fn sort_entries_with_moves_a_dependency_before_its_dependent() {
        // ui_color (requires game_symbols) is listed first -- wrong order.
        let e = entries(&[("ui_color", true), ("game_symbols", true)]);
        let mut manifests = std::collections::HashMap::new();
        manifests.insert("ui_color".to_string(), manifest(Some("ui_color"), &["game_symbols"], &[], &[], &[host_os_platform()]));
        manifests.insert("game_symbols".to_string(), manifest(Some("game_symbols"), &[], &[], &[], &[host_os_platform()]));

        let sorted = sort_entries_with(e, &manifests);
        let ids: Vec<&str> = sorted.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["game_symbols", "ui_color"]);
    }

    #[test]
    fn sort_entries_with_is_stable_when_already_valid() {
        let e = entries(&[("a", true), ("b", true), ("c", true)]);
        let manifests = std::collections::HashMap::new();
        let sorted = sort_entries_with(e, &manifests);
        let ids: Vec<&str> = sorted.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "an already-valid order (no dependencies) must be left untouched");
    }

    #[test]
    fn sort_entries_with_leaves_disabled_entries_in_their_original_slot() {
        // "disabled_lib" is disabled, so it must NOT be pulled forward even
        // though "consumer" requires it -- only the enabled subset is sorted.
        let e = entries(&[("consumer", true), ("disabled_lib", false), ("other", true)]);
        let mut manifests = std::collections::HashMap::new();
        manifests.insert("consumer".to_string(), manifest(Some("consumer"), &["disabled_lib"], &[], &[], &[host_os_platform()]));
        manifests.insert("other".to_string(), manifest(None, &[], &[], &[], &[]));

        let sorted = sort_entries_with(e, &manifests);
        assert_eq!(sorted[1], SidecarEntry { id: "disabled_lib".into(), enabled: false }, "disabled entries keep their absolute slot");
    }
}
