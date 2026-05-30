//! Archive extraction: zip, tar.gz, and 7z, fully in-process (no shell-outs).

use std::io;
use std::path::Path;

/// Extract a `.zip` file to `dest_dir`.
pub fn extract_zip(zip_path: &str, dest_dir: &str) -> io::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let dest = Path::new(dest_dir);
    std::fs::create_dir_all(dest)?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Sanitise the entry path so it cannot escape dest_dir.
        let entry_path = entry
            .enclosed_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid zip entry path"))?
            .to_path_buf();

        let out_path = dest.join(&entry_path);

        if entry.name().ends_with('/') {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out_file = std::fs::File::create(&out_path)?;
            io::copy(&mut entry, &mut out_file)?;

            // Preserve Unix permissions if available.
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }

    Ok(())
}

/// Extract a `.tar.gz` file to `dest_dir`.
pub fn extract_tar_gz(archive_path: &str, dest_dir: &str) -> io::Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    std::fs::create_dir_all(dest_dir)?;
    archive.unpack(dest_dir)?;
    Ok(())
}

/// Extract a `.7z` file to `dest_dir`.
pub fn extract_7z(archive_path: &str, dest_dir: &str) -> io::Result<()> {
    std::fs::create_dir_all(dest_dir)?;
    sevenz_rust::decompress_file(archive_path, dest_dir)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Dispatch to the correct extractor based on file extension.
pub fn extract_archive(archive_path: &str, dest_dir: &str) -> io::Result<()> {
    let lower = archive_path.to_lowercase();
    if lower.ends_with(".zip") {
        extract_zip(archive_path, dest_dir)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        extract_tar_gz(archive_path, dest_dir)
    } else if lower.ends_with(".7z") {
        extract_7z(archive_path, dest_dir)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported archive format: {}", archive_path),
        ))
    }
}

pub fn is_zip(name: &str) -> bool {
    name.to_lowercase().ends_with(".zip")
}

pub fn is_tar_gz(name: &str) -> bool {
    let l = name.to_lowercase();
    l.ends_with(".tar.gz") || l.ends_with(".tgz")
}

pub fn is_7z(name: &str) -> bool {
    name.to_lowercase().ends_with(".7z")
}

pub fn is_archive(name: &str) -> bool {
    is_zip(name) || is_tar_gz(name) || is_7z(name)
}

/// Detect if the first two bytes of a file are the gzip magic number `\x1f\x8b`.
pub fn is_gzip_magic(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b]
}
