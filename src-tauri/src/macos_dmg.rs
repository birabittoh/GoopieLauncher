//! Secure installation of a notarized macOS application from a release DMG.

use serde::Deserialize;
use std::{fs, path::{Path, PathBuf}, process::Command};

use crate::{binfmt, download};

pub const BUILD_KIND: &str = "macos-app";

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Vec<u8>, String>;
}

pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Vec<u8>, String> {
        let output = Command::new(program).args(args).output()
            .map_err(|e| format!("could not run {program}: {e}"))?;
        if output.status.success() { return Ok(output.stdout); }
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if diagnostic.is_empty() {
            format!("{program} failed with {}", output.status)
        } else {
            format!("{program}: {diagnostic}")
        })
    }
}

#[derive(Debug, Deserialize)]
struct AttachResult {
    #[serde(rename = "system-entities", default)]
    entities: Vec<AttachEntity>,
}

#[derive(Debug, Deserialize)]
struct AttachEntity {
    #[serde(rename = "mount-point")]
    mount_point: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BundleInfo {
    #[serde(rename = "CFBundleIdentifier")]
    bundle_identifier: String,
    #[serde(rename = "CFBundleExecutable")]
    executable: String,
    #[serde(rename = "CFBundleShortVersionString", default)]
    short_version: String,
    #[serde(rename = "CFBundleVersion", default)]
    bundle_version: String,
}

#[derive(Debug)]
pub struct InstalledApp {
    pub app_path: String,
    pub exe_path: String,
    pub arch: String,
}

fn parse_mount_point(bytes: &[u8]) -> Result<PathBuf, String> {
    let parsed: AttachResult = plist::from_bytes(bytes)
        .map_err(|e| format!("invalid hdiutil plist: {e}"))?;
    let mounts: Vec<_> = parsed.entities.into_iter().filter_map(|e| e.mount_point).collect();
    if mounts.len() != 1 { return Err(format!("DMG exposed {} mount points; expected one", mounts.len())); }
    Ok(PathBuf::from(&mounts[0]))
}

fn root_app(mount: &Path) -> Result<PathBuf, String> {
    let mut apps = Vec::new();
    for entry in fs::read_dir(mount).map_err(|e| format!("cannot read mounted DMG: {e}"))? {
        let entry = entry.map_err(|e| format!("cannot read mounted DMG entry: {e}"))?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir()
            && entry.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("app")) {
            apps.push(entry.path());
        }
    }
    if apps.len() != 1 { return Err(format!("DMG contains {} root-level apps; expected exactly one", apps.len())); }
    let canonical_mount = fs::canonicalize(mount).map_err(|e| e.to_string())?;
    let canonical_app = fs::canonicalize(&apps[0]).map_err(|e| e.to_string())?;
    if !canonical_app.starts_with(&canonical_mount) { return Err("app bundle escapes the DMG mount".into()); }
    Ok(apps.remove(0))
}

fn read_bundle_info(app: &Path) -> Result<BundleInfo, String> {
    let path = app.join("Contents/Info.plist");
    plist::from_file(&path).map_err(|e| format!("invalid {}: {e}", path.display()))
}

fn normalized_version(value: &str) -> &str { value.strip_prefix('v').unwrap_or(value) }

fn validate_app(app: &Path, expected_id: &str, version: &str, expected_arch: &str, runner: &dyn CommandRunner) -> Result<(String, String), String> {
    let info = read_bundle_info(app)?;
    if info.bundle_identifier != expected_id {
        return Err(format!("wrong bundle identifier: expected {expected_id}, found {}", info.bundle_identifier));
    }
    let actual_version = if info.short_version.is_empty() { &info.bundle_version } else { &info.short_version };
    if !version.is_empty() && normalized_version(actual_version) != normalized_version(version) {
        return Err(format!("wrong app version: expected {version}, found {actual_version}"));
    }
    if info.executable.contains('/') || info.executable.contains('\\') || info.executable == "." || info.executable == ".." {
        return Err("CFBundleExecutable is not a file name".into());
    }
    let exe = app.join("Contents/MacOS").join(&info.executable);
    let canonical_app = fs::canonicalize(app).map_err(|e| e.to_string())?;
    let canonical_exe = fs::canonicalize(&exe).map_err(|e| format!("bundle executable is missing: {e}"))?;
    if !canonical_exe.starts_with(&canonical_app) { return Err("bundle executable escapes the app".into()); }
    let detected = binfmt::detect_executable(&exe);
    if detected.platform != Some("macOS") { return Err("bundle executable is not Mach-O".into()); }
    let arch = detected.arch.unwrap_or("universal");
    if (arch == "universal" && !binfmt::macho_supports_arch(&exe, expected_arch))
        || (arch != "universal" && arch != expected_arch) {
        return Err(format!("wrong Mach-O architecture: expected {expected_arch}, found {arch}"));
    }
    let app_arg = app.to_string_lossy().into_owned();
    runner.run("/usr/bin/codesign", &["--verify".into(), "--strict".into(), "--verbose=2".into(), app_arg.clone()])?;
    runner.run("/usr/sbin/spctl", &["--assess".into(), "--type".into(), "execute".into(), app_arg])?;
    Ok((info.executable, arch.to_string()))
}

pub fn install(
    dmg: &Path,
    final_dir: &Path,
    expected_sha256: &str,
    expected_id: &str,
    version: &str,
    expected_arch: &str,
) -> Result<InstalledApp, String> {
    if !cfg!(target_os = "macos") { return Err("DMG installation is only supported on macOS".into()); }
    if expected_sha256.is_empty() { return Err("macOS release asset is missing its SHA-256 digest".into()); }
    if expected_id.is_empty() { return Err("game metadata is missing macBundleIdentifier".into()); }
    let actual = download::sha256_file(&dmg.to_string_lossy()).ok_or("could not hash downloaded DMG")?;
    let expected = expected_sha256.strip_prefix("sha256:").unwrap_or(expected_sha256);
    if !actual.eq_ignore_ascii_case(expected) { return Err(format!("DMG checksum mismatch: expected {expected}, found {actual}")); }

    let runner = SystemCommandRunner;
    let output = runner.run("/usr/bin/hdiutil", &[
        "attach".into(), "-readonly".into(), "-nobrowse".into(), "-plist".into(), dmg.to_string_lossy().into_owned(),
    ])?;
    let mount = parse_mount_point(&output)?;
    let result = (|| {
        let app = root_app(&mount)?;
        let (executable, _) = validate_app(&app, expected_id, version, expected_arch, &runner)?;
        fs::create_dir_all(final_dir).map_err(|e| format!("cannot create build directory: {e}"))?;
        let app_name = app.file_name().ok_or("app has no file name")?.to_string_lossy().into_owned();
        let temp_app = final_dir.join(format!(".{app_name}.installing"));
        if temp_app.exists() { fs::remove_dir_all(&temp_app).map_err(|e| e.to_string())?; }
        runner.run("/usr/bin/ditto", &[app.to_string_lossy().into_owned(), temp_app.to_string_lossy().into_owned()])?;
        let (_, arch) = validate_app(&temp_app, expected_id, version, expected_arch, &runner)?;
        let final_app = final_dir.join(&app_name);
        let backup_app = final_dir.join(format!(".{app_name}.previous"));
        if backup_app.exists() { fs::remove_dir_all(&backup_app).map_err(|e| e.to_string())?; }
        if final_app.exists() {
            fs::rename(&final_app, &backup_app).map_err(|e| format!("cannot stage existing app update: {e}"))?;
        }
        if let Err(e) = fs::rename(&temp_app, &final_app) {
            if backup_app.exists() { let _ = fs::rename(&backup_app, &final_app); }
            return Err(format!("cannot finalize app install: {e}"));
        }
        if backup_app.exists() { let _ = fs::remove_dir_all(&backup_app); }
        Ok(InstalledApp {
            app_path: app_name.clone(),
            exe_path: format!("{app_name}/Contents/MacOS/{executable}"),
            arch,
        })
    })();
    let detach = runner.run("/usr/bin/hdiutil", &["detach".into(), mount.to_string_lossy().into_owned()]);
    match (result, detach) {
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(format!("installed app but failed to detach DMG: {e}")),
        (Ok(app), Ok(_)) => Ok(app),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_mount_from_hdiutil_plist() {
        let mut entity = plist::Dictionary::new();
        entity.insert("mount-point".to_string(), plist::Value::String("/Volumes/Test".into()));
        let mut root = plist::Dictionary::new();
        root.insert("system-entities".to_string(), plist::Value::Array(vec![plist::Value::Dictionary(entity)]));
        let value = plist::Value::Dictionary(root);
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &value).unwrap();
        assert_eq!(parse_mount_point(&bytes).unwrap(), PathBuf::from("/Volumes/Test"));
    }

    #[test]
    fn rejects_ambiguous_mount_output() {
        let bytes = br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>system-entities</key><array/></dict></plist>"#;
        assert!(parse_mount_point(bytes).unwrap_err().contains("0 mount points"));
    }
}
