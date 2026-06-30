//! Parse XEX2 executables: decrypt, decompress, and read the embedded XDBF/SPA resource.
//!
//! Supports encryption type 0 (none) and 1 (AES), and compression type 0 (none),
//! 1 (basic), and 2 (normal/LZX).  The XDBF resource section contains the title
//! image, achievement metadata, string tables, and more.

use std::{
    io::{self, Read},
    path::Path,
};

use aes::Aes128;
use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};

/// The retail XEX AES key used to decrypt the per-file image key.
const RETAIL_KEY: [u8; 16] = [
    0x20, 0xB1, 0x85, 0xA5, 0x9D, 0x28, 0xFD, 0xC3,
    0x40, 0x58, 0x3F, 0xBB, 0x08, 0x96, 0xBF, 0x91,
];

/// PNG file signature.
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

// XDBF section namespaces
const XDBF_NS_METADATA: u16 = 1;
const XDBF_NS_IMAGE: u16 = 2;
const XDBF_NS_STRING: u16 = 3;

// Well-known XDBF entry IDs
const XDBF_ID_TITLE: u64 = 0x8000;
const XDBF_ID_XSTC: u64 = 0x58535443; // "XSTC" — default language
const XDBF_ID_XACH: u64 = 0x58414348; // "XACH" — achievement table

// XLanguage values (matches ReXGlue SDK enum)
const XLANGUAGE_ENGLISH: u32 = 1;

// -- Big-endian read helpers --------------------------------------------------

fn read_u16_be(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}

fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_u64_be(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

// -- AES helpers --------------------------------------------------------------

fn aes128_ecb_decrypt_block(block: &mut [u8; 16], key: &[u8; 16]) {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut blk = *GenericArray::from_slice(block.as_slice());
    cipher.decrypt_block(&mut blk);
    block.copy_from_slice(&blk);
}

fn aes128_cbc_decrypt_inplace(data: &mut [u8], key: &[u8; 16]) {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut iv = [0u8; 16];

    for chunk in data.chunks_exact_mut(16) {
        let ct: [u8; 16] = chunk.try_into().unwrap();
        let mut block = *GenericArray::from_slice(chunk as &[u8]);
        cipher.decrypt_block(&mut block);
        for i in 0..16 {
            chunk[i] = block[i] ^ iv[i];
        }
        iv = ct;
    }
}

// -- Normal (LZX) decompression -----------------------------------------------

fn decompress_normal_lzx(
    basefile: &[u8],
    window_size: u32,
    first_block_size: u32,
) -> io::Result<Vec<u8>> {
    let ws = match window_size {
        0x8000   => lzxd::WindowSize::KB32,
        0x10000  => lzxd::WindowSize::KB64,
        0x20000  => lzxd::WindowSize::KB128,
        0x40000  => lzxd::WindowSize::KB256,
        0x80000  => lzxd::WindowSize::KB512,
        0x100000 => lzxd::WindowSize::MB1,
        0x200000 => lzxd::WindowSize::MB2,
        _ => return Err(invalid(&format!(
            "unsupported LZX window size: {:#x}", window_size
        ))),
    };

    let mut lzx = lzxd::Lzxd::new(ws);
    let mut decompressed = Vec::new();
    let mut block_offset = 0usize;
    let mut block_size = first_block_size as usize;

    while block_size > 0 && block_offset < basefile.len() {
        if block_offset + block_size > basefile.len() {
            return Err(invalid("LZX block extends beyond basefile"));
        }
        let block_data = &basefile[block_offset..block_offset + block_size];
        if block_data.len() < 24 {
            return Err(invalid("LZX block too small for header"));
        }
        let next_block_size = read_u32_be(block_data, 0) as usize;
        let payload = &block_data[24..];
        let mut payload_offset = 0usize;

        while payload_offset < payload.len() {
            if payload_offset + 2 > payload.len() {
                break;
            }
            let chunk_compressed_size = read_u16_be(payload, payload_offset) as usize;
            payload_offset += 2;
            if chunk_compressed_size == 0 {
                break;
            }
            if payload_offset + chunk_compressed_size > payload.len() {
                return Err(invalid("LZX chunk extends beyond block payload"));
            }
            let chunk_data = &payload[payload_offset..payload_offset + chunk_compressed_size];
            let out_size = window_size as usize;
            match lzx.decompress_next(chunk_data, out_size) {
                Ok(chunk) => decompressed.extend_from_slice(chunk),
                Err(e) => return Err(invalid(&format!("LZX decompression error: {:?}", e))),
            }
            payload_offset += chunk_compressed_size;
        }

        block_offset += block_size;
        block_size = next_block_size;
    }

    Ok(decompressed)
}

// -- Optional header data -----------------------------------------------------

struct ResourceInfo {
    resource_va: u32,
    resource_size: u32,
}

struct FileFormatInfo {
    encryption_type: u16,
    compression_type: u16,
    basic_blocks: Vec<(u32, u32)>,
    normal_window_size: u32,
    normal_first_block_size: u32,
}

// -- Achievement record -------------------------------------------------------

/// A raw achievement entry parsed from the XACH table with strings resolved.
#[derive(Debug, Clone, serde::Serialize)]
pub struct XdbfAchievement {
    pub id: u32,
    pub label: String,
    pub description: String,
    pub unachieved_description: String,
    /// The XDBF image-resource id (used to fetch the icon PNG).
    pub image_id: u32,
    pub gamerscore: u32,
    pub flags: u32,
}

// -- Parsed XDBF --------------------------------------------------------------

/// A parsed and queryable XDBF/SPA resource extracted from an XEX2 basefile.
pub struct Xdbf {
    /// Raw XDBF bytes (owned).
    bytes: Vec<u8>,
    /// Offset of the first byte of XDBF data after the header+entry+free tables.
    data_base: usize,
    /// Number of entries in the entry table.
    entry_count: usize,
    /// The XEX title id string, e.g. "4D5307D2".
    pub title_id: String,
}

impl Xdbf {
    /// Find an entry by (section, id) and return a slice into the content.
    fn get_entry(&self, section: u16, id: u64) -> Option<&[u8]> {
        for i in 0..self.entry_count {
            let e_off = 0x18 + i * 18;
            if e_off + 18 > self.bytes.len() {
                break;
            }
            let ns = read_u16_be(&self.bytes, e_off);
            let eid = read_u64_be(&self.bytes, e_off + 2);
            let offset = read_u32_be(&self.bytes, e_off + 10) as usize;
            let length = read_u32_be(&self.bytes, e_off + 14) as usize;
            if ns != section || eid != id {
                continue;
            }
            let start = self.data_base + offset;
            let end = start + length;
            if end > self.bytes.len() {
                return None;
            }
            return Some(&self.bytes[start..end]);
        }
        None
    }

    /// Resolve a string from the XSTR table for the given language id.
    /// Falls back to empty string if not found.
    fn get_string(&self, language: u32, string_id: u16) -> String {
        let block = match self.get_entry(XDBF_NS_STRING, language as u64) {
            Some(b) => b,
            None => return String::new(),
        };
        // XSTR section header: magic(4) version(4) size(4) count(2) = 14 bytes
        if block.len() < 14 {
            return String::new();
        }
        // magic "XSTR" = 0x58535452
        let count = read_u16_be(block, 12) as usize;
        let mut ptr = 14usize;
        for _ in 0..count {
            if ptr + 4 > block.len() {
                break;
            }
            let sid = read_u16_be(block, ptr);
            let slen = read_u16_be(block, ptr + 2) as usize;
            ptr += 4;
            if ptr + slen > block.len() {
                break;
            }
            if sid == string_id {
                return String::from_utf8_lossy(&block[ptr..ptr + slen]).into_owned();
            }
            ptr += slen;
        }
        String::new()
    }

    /// Get the default language id from the XSTC entry (falls back to English = 1).
    pub fn default_language(&self) -> u32 {
        let block = match self.get_entry(XDBF_NS_METADATA, XDBF_ID_XSTC) {
            Some(b) => b,
            None => return XLANGUAGE_ENGLISH,
        };
        // XSTC header: magic(4) version(4) size(4) default_language(4) = 16 bytes
        if block.len() < 16 {
            return XLANGUAGE_ENGLISH;
        }
        read_u32_be(block, 12)
    }

    /// Returns true if the XSTR string table has an entry for the given language id.
    /// Use to decide whether a requested UI language has translations in this game,
    /// falling back to [`default_language`](Self::default_language) when it doesn't.
    pub fn has_language(&self, language: u32) -> bool {
        self.get_entry(XDBF_NS_STRING, language as u64).is_some()
    }

    /// Return the PNG bytes for an image resource by id, or `None` if absent.
    pub fn get_image(&self, id: u64) -> Option<&[u8]> {
        let data = self.get_entry(XDBF_NS_IMAGE, id)?;
        if data.len() >= 8 && data[..8] == PNG_SIGNATURE {
            Some(data)
        } else {
            None
        }
    }

    /// Parse the XACH achievement table and resolve strings for the given language.
    /// If no XACH entry exists, returns an empty vec.
    pub fn get_achievements(&self, language: u32) -> Vec<XdbfAchievement> {
        let block = match self.get_entry(XDBF_NS_METADATA, XDBF_ID_XACH) {
            Some(b) => b,
            None => return vec![],
        };
        // XACH section header: magic(4) version(4) size(4) count(2) = 14 bytes
        if block.len() < 14 {
            return vec![];
        }
        let count = read_u16_be(block, 12) as usize;
        let mut ptr = 14usize;
        let mut out = Vec::with_capacity(count);

        for _ in 0..count {
            // XdbfAchievementTableEntry = 0x24 bytes, all big-endian:
            // id(u16) label_id(u16) description_id(u16) unachieved_id(u16)
            // image_id(u32) gamerscore(u16) unkE(u16) flags(u32)
            // unk14(u32) unk18(u32) unk1C(u32) unk20(u32)
            if ptr + 0x24 > block.len() {
                break;
            }
            let id       = read_u16_be(block, ptr)       as u32;
            let label_id = read_u16_be(block, ptr + 2);
            let desc_id  = read_u16_be(block, ptr + 4);
            let uach_id  = read_u16_be(block, ptr + 6);
            let image_id = read_u32_be(block, ptr + 8);
            let gscore   = read_u16_be(block, ptr + 12) as u32;
            let flags    = read_u32_be(block, ptr + 16);
            ptr += 0x24;

            out.push(XdbfAchievement {
                id,
                label: self.get_string(language, label_id),
                description: self.get_string(language, desc_id),
                unachieved_description: self.get_string(language, uach_id),
                image_id,
                gamerscore: gscore,
                flags,
            });
        }
        out
    }
}

// -- Public API ---------------------------------------------------------------

/// Parse an XEX2 file: decrypt + decompress the basefile, locate and validate
/// the XDBF/SPA resource, and return a queryable [`Xdbf`] wrapper.
///
/// Returns an `io::Error` for unreadable, encrypted-with-unknown-key, or
/// unsupported files.  Callers that need graceful degradation should match on
/// the error rather than propagating it.
pub fn load_xdbf(xex_path: &Path) -> io::Result<Xdbf> {
    let mut file = std::fs::File::open(xex_path)?;
    let mut xex_data = Vec::new();
    file.read_to_end(&mut xex_data)?;

    if xex_data.len() < 0x18 {
        return Err(invalid("file too small for XEX2 header"));
    }
    if &xex_data[0..4] != b"XEX2" {
        return Err(invalid("not a XEX2 file (bad magic)"));
    }

    let basefile_offset    = read_u32_be(&xex_data, 0x08) as usize;
    let security_info_offset = read_u32_be(&xex_data, 0x10) as usize;
    let opt_header_count   = read_u32_be(&xex_data, 0x14) as usize;

    let mut resource_info: Option<ResourceInfo> = None;
    let mut format_info: Option<FileFormatInfo> = None;
    let mut image_base: u32 = 0x82000000;
    let mut title_id: Option<u32> = None;

    for i in 0..opt_header_count {
        let entry_off = 0x18 + i * 8;
        if entry_off + 8 > xex_data.len() {
            return Err(invalid("optional header entry out of bounds"));
        }
        let key   = read_u32_be(&xex_data, entry_off);
        let value = read_u32_be(&xex_data, entry_off + 4);

        match key {
            0x000002FF => {
                // Resource Info: file offset, struct is 0x14 bytes.
                // +0x00: name (8 bytes, resource directory name, NOT the title id)
                // +0x08: struct size (u32)
                // +0x0C: resource VA (u32)
                // +0x10: resource size (u32)
                let off = value as usize;
                if off + 0x14 > xex_data.len() {
                    return Err(invalid("resource info struct out of bounds"));
                }
                resource_info = Some(ResourceInfo {
                    resource_va:   read_u32_be(&xex_data, off + 0x0C),
                    resource_size: read_u32_be(&xex_data, off + 0x10),
                });
            }
            0x00040006 => {
                // Execution Info: file offset, xex2_opt_execution_info struct.
                // +0x0C: title_id (u32)
                let off = value as usize;
                if off + 0x10 > xex_data.len() {
                    return Err(invalid("execution info struct out of bounds"));
                }
                title_id = Some(read_u32_be(&xex_data, off + 0x0C));
            }
            0x000003FF => {
                // File Format Info: file offset.
                let off = value as usize;
                if off + 8 > xex_data.len() {
                    return Err(invalid("file format info struct out of bounds"));
                }
                let struct_size = read_u32_be(&xex_data, off) as usize;
                let encryption_type  = read_u16_be(&xex_data, off + 4);
                let compression_type = read_u16_be(&xex_data, off + 6);

                let mut basic_blocks = Vec::new();
                let mut normal_window_size = 0u32;
                let mut normal_first_block_size = 0u32;

                if compression_type == 1 {
                    let num_blocks = (struct_size.saturating_sub(8)) / 8;
                    for j in 0..num_blocks {
                        let blk_off = off + 8 + j * 8;
                        if blk_off + 8 > xex_data.len() {
                            break;
                        }
                        let data_size = read_u32_be(&xex_data, blk_off);
                        let zero_size = read_u32_be(&xex_data, blk_off + 4);
                        basic_blocks.push((data_size, zero_size));
                    }
                } else if compression_type == 2 {
                    if off + 16 <= xex_data.len() {
                        normal_window_size      = read_u32_be(&xex_data, off + 8);
                        normal_first_block_size = read_u32_be(&xex_data, off + 12);
                    }
                }

                format_info = Some(FileFormatInfo {
                    encryption_type,
                    compression_type,
                    basic_blocks,
                    normal_window_size,
                    normal_first_block_size,
                });
            }
            0x00010201 => {
                image_base = value;
            }
            _ => {}
        }
    }

    let resource_info = resource_info
        .ok_or_else(|| invalid("no resource info header found in XEX"))?;
    let format_info = format_info
        .ok_or_else(|| invalid("no file format info header found in XEX"))?;

    if format_info.encryption_type > 1 {
        return Err(invalid(&format!(
            "unsupported encryption type: {}", format_info.encryption_type
        )));
    }
    if format_info.compression_type > 2 {
        return Err(invalid(&format!(
            "unsupported compression type: {} (only none, basic, and normal/LZX are supported)",
            format_info.compression_type
        )));
    }

    if basefile_offset >= xex_data.len() {
        return Err(invalid("basefile offset beyond end of file"));
    }

    let mut basefile = xex_data[basefile_offset..].to_vec();

    if format_info.encryption_type == 1 {
        let key_off = security_info_offset + 0x150;
        if key_off + 16 > xex_data.len() {
            return Err(invalid("security info image key out of bounds"));
        }
        let mut session_key: [u8; 16] = xex_data[key_off..key_off + 16].try_into().unwrap();
        aes128_ecb_decrypt_block(&mut session_key, &RETAIL_KEY);
        let aligned_len = basefile.len() & !0xF;
        aes128_cbc_decrypt_inplace(&mut basefile[..aligned_len], &session_key);
    }

    let image = if format_info.compression_type == 1 {
        let mut decompressed = Vec::new();
        let mut src_offset = 0usize;
        for (data_size, zero_size) in &format_info.basic_blocks {
            let ds = *data_size as usize;
            let zs = *zero_size as usize;
            if src_offset + ds > basefile.len() {
                return Err(invalid("basic compression data block exceeds basefile"));
            }
            decompressed.extend_from_slice(&basefile[src_offset..src_offset + ds]);
            decompressed.resize(decompressed.len() + zs, 0);
            src_offset += ds;
        }
        decompressed
    } else if format_info.compression_type == 2 {
        decompress_normal_lzx(
            &basefile,
            format_info.normal_window_size,
            format_info.normal_first_block_size,
        )?
    } else {
        basefile
    };

    if image_base == 0 {
        return Err(invalid("image base VA is zero"));
    }

    let rva   = resource_info.resource_va.wrapping_sub(image_base) as usize;
    let rsize = resource_info.resource_size as usize;

    if rva + rsize > image.len() {
        return Err(invalid(&format!(
            "resource section out of bounds (offset {:#x}, size {:#x}, image len {:#x})",
            rva, rsize, image.len()
        )));
    }

    let xdbf_bytes = image[rva..rva + rsize].to_vec();

    if xdbf_bytes.len() < 0x18 {
        return Err(invalid("XDBF section too small for header"));
    }
    if &xdbf_bytes[0..4] != b"XDBF" {
        return Err(invalid("bad XDBF magic"));
    }
    let version = read_u32_be(&xdbf_bytes, 0x04);
    if version != 0x10000 {
        return Err(invalid(&format!("unexpected XDBF version: {:#x}", version)));
    }
    let entry_count = read_u32_be(&xdbf_bytes, 0x08) as usize;
    let free_count  = read_u32_be(&xdbf_bytes, 0x10) as usize;
    let data_base   = 0x18 + entry_count * 18 + free_count * 8;

    Ok(Xdbf {
        bytes: xdbf_bytes,
        data_base,
        entry_count,
        title_id: format!("{:08X}", title_id.unwrap_or(0)),
    })
}

/// Extract the title image PNG from an XEX2 executable.
///
/// Returns the raw PNG bytes on success.  Prefers the title thumbnail
/// (XDBF image id `0x8000`); falls back to the first PNG image found.
pub fn extract_title_image(xex_path: &Path) -> io::Result<Vec<u8>> {
    let xdbf = load_xdbf(xex_path)?;

    // Prefer the title thumbnail (id 0x8000).
    if let Some(png) = xdbf.get_image(XDBF_ID_TITLE) {
        return Ok(png.to_vec());
    }

    // Scan all image-namespace entries for any PNG as a fallback.
    for i in 0..xdbf.entry_count {
        let e_off = 0x18 + i * 18;
        if e_off + 18 > xdbf.bytes.len() {
            break;
        }
        let ns = read_u16_be(&xdbf.bytes, e_off);
        if ns != XDBF_NS_IMAGE {
            continue;
        }
        let id = read_u64_be(&xdbf.bytes, e_off + 2);
        if id == XDBF_ID_TITLE {
            continue; // already tried above
        }
        if let Some(png) = xdbf.get_image(id) {
            return Ok(png.to_vec());
        }
    }

    Err(invalid("no PNG image found in XDBF resource section"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-test: extract a title image from every installed game's `default.xex`.
    /// Skips gracefully when the games directory or specific games are absent (CI).
    #[test]
    fn test_extract_installed_games() {
        let config_dir = crate::config::get_games_folder();
        let games_dir = std::path::PathBuf::from(config_dir);
        if !games_dir.exists() {
            eprintln!("SKIP: games dir not found ({})", games_dir.display());
            return;
        }

        let mut found_any = false;
        if let Ok(entries) = std::fs::read_dir(&games_dir) {
            for entry in entries.flatten() {
                let xex = entry.path().join("assets").join("default.xex");
                if !xex.exists() {
                    continue;
                }
                found_any = true;
                let name = entry.file_name().to_string_lossy().into_owned();
                match extract_title_image(&xex) {
                    Ok(png) => {
                        eprintln!("OK {}: {} bytes", name, png.len());
                        assert!(png.len() > 8);
                        assert_eq!(&png[..8], &PNG_SIGNATURE);
                    }
                    Err(e) => panic!("FAIL {}: {}", name, e),
                }
            }
        }
        if !found_any {
            eprintln!("SKIP: no games with default.xex found");
        }
    }

    /// Smoke-test: load XDBF and parse achievements from installed games.
    #[test]
    fn test_load_xdbf_achievements() {
        let config_dir = crate::config::get_games_folder();
        let games_dir = std::path::PathBuf::from(config_dir);
        if !games_dir.exists() {
            eprintln!("SKIP: games dir not found");
            return;
        }

        let mut found_any = false;
        if let Ok(entries) = std::fs::read_dir(&games_dir) {
            for entry in entries.flatten() {
                let xex = entry.path().join("assets").join("default.xex");
                if !xex.exists() {
                    continue;
                }
                found_any = true;
                let name = entry.file_name().to_string_lossy().into_owned();
                match load_xdbf(&xex) {
                    Ok(xdbf) => {
                        let lang = xdbf.default_language();
                        let achievements = xdbf.get_achievements(lang);
                        eprintln!(
                            "OK {} (title_id={}, lang={}, achievements={})",
                            name, xdbf.title_id, lang, achievements.len()
                        );
                        for a in &achievements {
                            assert_ne!(a.id, 0, "achievement id 0 is invalid");
                            assert!(!a.label.is_empty(), "achievement {} has empty label", a.id);
                        }
                    }
                    Err(e) => panic!("FAIL {}: {}", name, e),
                }
            }
        }
        if !found_any {
            eprintln!("SKIP: no games with default.xex found");
        }
    }
}
