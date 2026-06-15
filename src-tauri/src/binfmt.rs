//! Lightweight executable header sniffing.
//!
//! After a build is downloaded and extracted, we inspect its main executable to
//! determine which platform/architecture it was actually built for — far more
//! reliable than guessing from the release asset's filename. The detected
//! values are persisted in the `.installed.json` sidecar (see `games.rs`) and
//! used by the frontend to hide/grey-out builds that can't run on the current
//! system.
//!
//! Only the first few KB of the file are read; unrecognised formats yield
//! `ExeInfo { platform: None, arch: None }` rather than an error, so an
//! unidentifiable executable simply isn't gated.

use std::io::Read;
use std::path::Path;

/// Detected platform/architecture of an executable. Either field may be `None`
/// if it couldn't be determined — callers should treat `None` as "unknown" and
/// not block on it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExeInfo {
    /// "Windows" | "Linux" | "macOS"
    pub platform: Option<&'static str>,
    /// Matches `std::env::consts::ARCH` values: "x86_64" | "x86" | "aarch64" | "arm"
    pub arch: Option<&'static str>,
}

const HEADER_READ_LEN: usize = 4096;

/// Inspect `path`'s header and return its detected platform/arch. Returns
/// `ExeInfo::default()` (both `None`) if the file can't be read or the format
/// isn't recognised.
pub fn detect_executable(path: &Path) -> ExeInfo {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return ExeInfo::default(),
    };

    let mut buf = [0u8; HEADER_READ_LEN];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return ExeInfo::default(),
    };
    let data = &buf[..n];

    detect_from_bytes(data)
}

/// Pure header-parsing logic, split out from [`detect_executable`] for unit
/// testing without touching the filesystem.
fn detect_from_bytes(data: &[u8]) -> ExeInfo {
    if data.len() >= 4 && &data[0..4] == b"\x7FELF" {
        return detect_elf(data);
    }
    if data.len() >= 2 && &data[0..2] == b"MZ" {
        return detect_pe(data);
    }
    if data.len() >= 4 {
        let magic = &data[0..4];
        if magic == b"\xFE\xED\xFA\xCE" || magic == b"\xFE\xED\xFA\xCF"
            || magic == b"\xCE\xFA\xED\xFE" || magic == b"\xCF\xFA\xED\xFE"
            || magic == b"\xCA\xFE\xBA\xBE" || magic == b"\xBE\xBA\xFE\xCA"
        {
            return detect_macho(data);
        }
    }
    ExeInfo::default()
}

/// ELF header: https://en.wikipedia.org/wiki/Executable_and_Linkable_Format
/// - byte 5 (EI_DATA): 1 = little-endian, 2 = big-endian
/// - bytes 18-19 (e_machine): architecture, endianness-dependent
fn detect_elf(data: &[u8]) -> ExeInfo {
    if data.len() < 20 {
        return ExeInfo { platform: Some("Linux"), arch: None };
    }
    let little_endian = data[5] != 2;
    let machine = if little_endian {
        u16::from_le_bytes([data[18], data[19]])
    } else {
        u16::from_be_bytes([data[18], data[19]])
    };
    let arch = match machine {
        62 => Some("x86_64"),
        183 => Some("aarch64"),
        3 => Some("x86"),
        40 => Some("arm"),
        _ => None,
    };
    ExeInfo { platform: Some("Linux"), arch }
}

/// PE header: the COFF header offset is a little-endian u32 at 0x3C, prefixed
/// by the "PE\0\0" signature. The COFF `Machine` field is a little-endian u16
/// immediately after that signature.
fn detect_pe(data: &[u8]) -> ExeInfo {
    if data.len() < 0x40 {
        return ExeInfo { platform: Some("Windows"), arch: None };
    }
    let pe_offset = u32::from_le_bytes([data[0x3C], data[0x3D], data[0x3E], data[0x3F]]) as usize;
    if pe_offset + 6 > data.len() || &data[pe_offset..pe_offset + 4] != b"PE\0\0" {
        return ExeInfo { platform: Some("Windows"), arch: None };
    }
    let machine = u16::from_le_bytes([data[pe_offset + 4], data[pe_offset + 5]]);
    let arch = match machine {
        0x8664 => Some("x86_64"),
        0x014C => Some("x86"),
        0xAA64 => Some("aarch64"),
        0x01C0 | 0x01C4 => Some("arm"),
        _ => None,
    };
    ExeInfo { platform: Some("Windows"), arch }
}

/// Mach-O header: `cputype` is a 32-bit value at offset 4, endianness given by
/// the magic number. FAT binaries (multiple architectures bundled together)
/// don't have a single `cputype`, so only the platform is reported for those.
fn detect_macho(data: &[u8]) -> ExeInfo {
    let magic = &data[0..4];
    let is_fat = magic == b"\xCA\xFE\xBA\xBE" || magic == b"\xBE\xBA\xFE\xCA";
    if is_fat {
        return ExeInfo { platform: Some("macOS"), arch: None };
    }
    if data.len() < 8 {
        return ExeInfo { platform: Some("macOS"), arch: None };
    }
    let little_endian = magic == b"\xCE\xFA\xED\xFE" || magic == b"\xCF\xFA\xED\xFE";
    let cputype = if little_endian {
        u32::from_le_bytes([data[4], data[5], data[6], data[7]])
    } else {
        u32::from_be_bytes([data[4], data[5], data[6], data[7]])
    };
    let arch = match cputype {
        0x0100_000C => Some("aarch64"),
        0x0100_0007 => Some("x86_64"),
        7 => Some("x86"),
        12 => Some("arm"),
        _ => None,
    };
    ExeInfo { platform: Some("macOS"), arch }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf_header(ei_data: u8, e_machine: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 20];
        buf[0..4].copy_from_slice(b"\x7FELF");
        buf[5] = ei_data; // 1 = LE, 2 = BE
        if ei_data == 1 {
            buf[18..20].copy_from_slice(&e_machine.to_le_bytes());
        } else {
            buf[18..20].copy_from_slice(&e_machine.to_be_bytes());
        }
        buf
    }

    fn pe_header(machine: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 0x40 + 6];
        buf[0..2].copy_from_slice(b"MZ");
        let pe_offset: u32 = 0x40;
        buf[0x3C..0x40].copy_from_slice(&pe_offset.to_le_bytes());
        buf[0x40..0x44].copy_from_slice(b"PE\0\0");
        buf[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
        buf
    }

    fn macho_header(magic: [u8; 4], cputype: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 8];
        buf[0..4].copy_from_slice(&magic);
        let little_endian = magic == *b"\xCE\xFA\xED\xFE" || magic == *b"\xCF\xFA\xED\xFE";
        if little_endian {
            buf[4..8].copy_from_slice(&cputype.to_le_bytes());
        } else {
            buf[4..8].copy_from_slice(&cputype.to_be_bytes());
        }
        buf
    }

    #[test]
    fn elf_x86_64() {
        let info = detect_from_bytes(&elf_header(1, 62));
        assert_eq!(info, ExeInfo { platform: Some("Linux"), arch: Some("x86_64") });
    }

    #[test]
    fn elf_aarch64() {
        let info = detect_from_bytes(&elf_header(1, 183));
        assert_eq!(info, ExeInfo { platform: Some("Linux"), arch: Some("aarch64") });
    }

    #[test]
    fn pe_x86_64() {
        let info = detect_from_bytes(&pe_header(0x8664));
        assert_eq!(info, ExeInfo { platform: Some("Windows"), arch: Some("x86_64") });
    }

    #[test]
    fn pe_arm64() {
        let info = detect_from_bytes(&pe_header(0xAA64));
        assert_eq!(info, ExeInfo { platform: Some("Windows"), arch: Some("aarch64") });
    }

    #[test]
    fn pe_x86() {
        let info = detect_from_bytes(&pe_header(0x014C));
        assert_eq!(info, ExeInfo { platform: Some("Windows"), arch: Some("x86") });
    }

    #[test]
    fn macho_x86_64() {
        let info = detect_from_bytes(&macho_header(*b"\xCF\xFA\xED\xFE", 0x0100_0007));
        assert_eq!(info, ExeInfo { platform: Some("macOS"), arch: Some("x86_64") });
    }

    #[test]
    fn macho_arm64() {
        let info = detect_from_bytes(&macho_header(*b"\xCF\xFA\xED\xFE", 0x0100_000C));
        assert_eq!(info, ExeInfo { platform: Some("macOS"), arch: Some("aarch64") });
    }

    #[test]
    fn macho_fat_unknown_arch() {
        let info = detect_from_bytes(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 2]);
        assert_eq!(info, ExeInfo { platform: Some("macOS"), arch: None });
    }

    #[test]
    fn unknown_format() {
        let info = detect_from_bytes(b"not an executable");
        assert_eq!(info, ExeInfo::default());
    }

    #[test]
    fn empty_file() {
        let info = detect_from_bytes(&[]);
        assert_eq!(info, ExeInfo::default());
    }
}
