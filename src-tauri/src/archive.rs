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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// One regular file at the root and one nested under a subdirectory, so each
    /// test exercises both flat entries and directory creation on extract.
    const ENTRIES: &[(&str, &[u8])] = &[
        ("game-linux-x64", b"\x7fELF fake executable payload"),
        ("data/config.toml", b"name = \"goopie\"\n"),
    ];

    fn build_src_dir() -> tempfile::TempDir {
        let src = tempfile::tempdir().expect("src tempdir");
        for (name, data) in ENTRIES {
            let p = src.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, data).unwrap();
        }
        src
    }

    fn make_zip(path: &PathBuf) {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let f = std::fs::File::create(path).unwrap();
        let mut zw = ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        for (name, data) in ENTRIES {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    fn make_tar_gz(path: &PathBuf) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let f = std::fs::File::create(path).unwrap();
        let enc = GzEncoder::new(f, Compression::default());
        let mut builder = tar::Builder::new(enc);
        for (name, data) in ENTRIES {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        // into_inner() finalizes the tar stream and hands back the gz encoder,
        // which we then finish() to flush the gzip trailer.
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn make_7z(path: &PathBuf) {
        let src = build_src_dir();
        sevenz_rust::compress_to_path(src.path(), path).unwrap();
    }

    /// Extract `archive` and assert every fixture entry came back byte-for-byte,
    /// including the nested `data/config.toml`.
    fn assert_round_trip(archive: &PathBuf) {
        let dest = tempfile::tempdir().expect("dest tempdir");
        extract_archive(&archive.to_string_lossy(), &dest.path().to_string_lossy())
            .unwrap_or_else(|e| panic!("extract_archive({}) failed: {e}", archive.display()));
        for (name, data) in ENTRIES {
            let out = dest.path().join(name);
            assert!(out.exists(), "{name} missing after extracting {}", archive.display());
            assert_eq!(&std::fs::read(&out).unwrap(), data, "{name} contents mismatch");
        }
    }

    #[test]
    fn extract_archive_handles_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("game.zip");
        make_zip(&path);
        assert_round_trip(&path);
    }

    #[test]
    fn extract_archive_handles_tar_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("game.tar.gz");
        make_tar_gz(&path);
        assert_round_trip(&path);
    }

    #[test]
    fn extract_archive_handles_tgz_extension() {
        // `.tgz` is an accepted alias for `.tar.gz` in the dispatcher.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("game.tgz");
        make_tar_gz(&path);
        assert_round_trip(&path);
    }

    #[test]
    fn extract_archive_handles_7z() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("game.7z");
        make_7z(&path);
        assert_round_trip(&path);
    }

    #[test]
    fn extract_archive_rejects_unsupported_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("game.rar");
        std::fs::write(&path, b"not really a rar").unwrap();
        let err = extract_archive(&path.to_string_lossy(), &tmp.path().to_string_lossy())
            .expect_err("unsupported extension must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn is_archive_matches_supported_extensions_case_insensitively() {
        for name in ["game.zip", "GAME.ZIP", "game.tar.gz", "game.TGZ", "game.7z"] {
            assert!(is_archive(name), "{name} should be recognized as an archive");
        }
        for name in ["game-linux-x64", "game.exe", "game.AppImage", "game.rar"] {
            assert!(!is_archive(name), "{name} should not be recognized as an archive");
        }
    }
}
