//! Extract the title image (PNG) from an Xbox 360 XEX2 executable.
//!
//! Supports encryption type 0 (none) and 1 (AES), and compression type 0 (none)
//! and 1 (basic). The title image is found inside the XDBF/SPA resource section
//! embedded in the decompressed basefile.

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

/// Decrypt a single 16-byte block with AES-128-ECB.
fn aes128_ecb_decrypt_block(block: &mut [u8; 16], key: &[u8; 16]) {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut blk = *GenericArray::from_slice(block.as_slice());
    cipher.decrypt_block(&mut blk);
    block.copy_from_slice(&blk);
}

/// In-place AES-128-CBC decryption with a zero IV.
fn aes128_cbc_decrypt_inplace(data: &mut [u8], key: &[u8; 16]) {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut iv = [0u8; 16];

    for chunk in data.chunks_exact_mut(16) {
        // Save ciphertext before we overwrite it.
        let ct: [u8; 16] = chunk.try_into().unwrap();

        let mut block = *GenericArray::from_slice(chunk as &[u8]);
        cipher.decrypt_block(&mut block);

        for i in 0..16 {
            chunk[i] = block[i] ^ iv[i];
        }
        iv = ct;
    }
}

// -- Optional header data -----------------------------------------------------

/// Parsed info from the Resource Info optional header (key 0x000002FF).
struct ResourceInfo {
    resource_va: u32,
    resource_size: u32,
}

/// Parsed info from the File Format Info optional header (key 0x000003FF).
struct FileFormatInfo {
    encryption_type: u16,
    compression_type: u16,
    /// Basic-compression block descriptors: (data_size, zero_size).
    basic_blocks: Vec<(u32, u32)>,
    /// Normal (LZX) compression: window size in bytes.
    normal_window_size: u32,
    /// Normal compression: first block's compressed data size.
    normal_first_block_size: u32,
}

// -- Normal (LZX) decompression -----------------------------------------------

/// Decompress a basefile that uses XEX "normal" (LZX) compression.
///
/// The basefile is divided into blocks.  Each block has a 24-byte header:
///   - next_block_size (u32 BE) — size of the next block (0 on the last)
///   - sha1_hash (20 bytes) — integrity hash (ignored here)
///
/// After the header, the payload contains a series of inner LZX chunks, each
/// prefixed with a 2-byte BE compressed size.  Each chunk decompresses to
/// `window_size` bytes (or less for the final chunk).
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
            "unsupported LZX window size: {:#x}",
            window_size
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
        // Skip 20-byte SHA1 hash → payload starts at offset 24.
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

// -- Public API ---------------------------------------------------------------

/// Extract the title image PNG from an XEX2 executable.
///
/// Returns the raw PNG bytes on success.
pub fn extract_title_image(xex_path: &Path) -> io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(xex_path)?;

    // Read the entire file into memory — XEX files are typically a few MB.
    let mut xex_data = Vec::new();
    file.read_to_end(&mut xex_data)?;

    // -- Parse header ---------------------------------------------------------

    if xex_data.len() < 0x18 {
        return Err(invalid("file too small for XEX2 header"));
    }
    if &xex_data[0..4] != b"XEX2" {
        return Err(invalid("not a XEX2 file (bad magic)"));
    }

    let basefile_offset = read_u32_be(&xex_data, 0x08) as usize;
    let security_info_offset = read_u32_be(&xex_data, 0x10) as usize;
    let opt_header_count = read_u32_be(&xex_data, 0x14) as usize;

    // -- Scan optional headers ------------------------------------------------

    let mut resource_info: Option<ResourceInfo> = None;
    let mut format_info: Option<FileFormatInfo> = None;
    let mut image_base: u32 = 0x82000000; // default

    for i in 0..opt_header_count {
        let entry_off = 0x18 + i * 8;
        if entry_off + 8 > xex_data.len() {
            return Err(invalid("optional header entry out of bounds"));
        }

        let key = read_u32_be(&xex_data, entry_off);
        let value = read_u32_be(&xex_data, entry_off + 4);

        match key {
            // Resource Info — value is a file offset (low byte 0xFF).
            0x000002FF => {
                let off = value as usize;
                if off + 0x14 > xex_data.len() {
                    return Err(invalid("resource info struct out of bounds"));
                }
                resource_info = Some(ResourceInfo {
                    resource_va: read_u32_be(&xex_data, off + 0x0C),
                    resource_size: read_u32_be(&xex_data, off + 0x10),
                });
            }

            // File Format Info — value is a file offset (low byte 0xFF).
            0x000003FF => {
                let off = value as usize;
                if off + 8 > xex_data.len() {
                    return Err(invalid("file format info struct out of bounds"));
                }
                let struct_size = read_u32_be(&xex_data, off) as usize;
                let encryption_type = read_u16_be(&xex_data, off + 4);
                let compression_type = read_u16_be(&xex_data, off + 6);

                let mut basic_blocks = Vec::new();
                let mut normal_window_size = 0u32;
                let mut normal_first_block_size = 0u32;

                if compression_type == 1 {
                    // Basic compression: array of (data_size, zero_size) pairs.
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
                    // Normal (LZX) compression: window_size (u32), then block
                    // descriptors of 24 bytes each (data_size u32 + sha1 20 bytes).
                    if off + 16 <= xex_data.len() {
                        normal_window_size = read_u32_be(&xex_data, off + 8);
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

            // Image Base VA — inline u32 (low byte 0x01).
            0x00010201 => {
                image_base = value;
            }

            _ => {}
        }
    }

    let resource_info =
        resource_info.ok_or_else(|| invalid("no resource info header found in XEX"))?;
    let format_info =
        format_info.ok_or_else(|| invalid("no file format info header found in XEX"))?;

    // Validate encryption and compression types.
    if format_info.encryption_type > 1 {
        return Err(invalid(&format!(
            "unsupported encryption type: {}",
            format_info.encryption_type
        )));
    }
    if format_info.compression_type > 2 {
        return Err(invalid(&format!(
            "unsupported compression type: {} (only none, basic, and normal/LZX are supported)",
            format_info.compression_type
        )));
    }

    // -- Extract and decrypt the basefile -------------------------------------

    if basefile_offset >= xex_data.len() {
        return Err(invalid("basefile offset beyond end of file"));
    }

    let mut basefile = xex_data[basefile_offset..].to_vec();

    if format_info.encryption_type == 1 {
        // Read the 16-byte encrypted image key from the security info struct.
        let key_off = security_info_offset + 0x150;
        if key_off + 16 > xex_data.len() {
            return Err(invalid("security info image key out of bounds"));
        }

        let mut session_key: [u8; 16] = xex_data[key_off..key_off + 16].try_into().unwrap();

        // Decrypt the image key with the retail key (ECB) to get the session key.
        aes128_ecb_decrypt_block(&mut session_key, &RETAIL_KEY);

        // Decrypt the basefile in-place with CBC (IV = 0).
        // Truncate to a multiple of 16 for block-aligned decryption.
        let aligned_len = basefile.len() & !0xF;
        aes128_cbc_decrypt_inplace(&mut basefile[..aligned_len], &session_key);
    }

    // -- Decompress the basefile ----------------------------------------------

    let image = if format_info.compression_type == 1 {
        // Basic compression: series of (data_size, zero_size) blocks.
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
        // Normal (LZX) compression.
        decompress_normal_lzx(
            &basefile,
            format_info.normal_window_size,
            format_info.normal_first_block_size,
        )?
    } else {
        // No compression.
        basefile
    };

    // -- Locate the XDBF resource section in the decompressed image -----------

    if image_base == 0 {
        return Err(invalid("image base VA is zero"));
    }

    let rva = resource_info.resource_va.wrapping_sub(image_base) as usize;
    let rsize = resource_info.resource_size as usize;

    if rva + rsize > image.len() {
        return Err(invalid(&format!(
            "resource section out of bounds (offset {:#x}, size {:#x}, image len {:#x})",
            rva, rsize, image.len()
        )));
    }

    let xdbf = &image[rva..rva + rsize];

    // -- Parse XDBF -----------------------------------------------------------

    if xdbf.len() < 0x18 {
        return Err(invalid("XDBF section too small for header"));
    }
    if &xdbf[0..4] != b"XDBF" {
        return Err(invalid("bad XDBF magic"));
    }

    let version = read_u32_be(xdbf, 0x04);
    if version != 0x10000 {
        return Err(invalid(&format!("unexpected XDBF version: {:#x}", version)));
    }

    let entry_count = read_u32_be(xdbf, 0x08) as usize;
    let free_count = read_u32_be(xdbf, 0x10) as usize;

    // Data starts after the header, entry table, and free-entry table.
    let data_base = 0x18 + entry_count * 18 + free_count * 8;

    // Scan entries for image namespace (2) with PNG data.
    // Prefer the title thumbnail (id 0x8000).
    let mut best_png: Option<Vec<u8>> = None;

    for i in 0..entry_count {
        let e_off = 0x18 + i * 18;
        if e_off + 18 > xdbf.len() {
            break;
        }

        let namespace = read_u16_be(xdbf, e_off);
        if namespace != 2 {
            continue; // only interested in image namespace
        }

        let id = read_u64_be(xdbf, e_off + 2);
        let offset = read_u32_be(xdbf, e_off + 10) as usize;
        let length = read_u32_be(xdbf, e_off + 14) as usize;

        let abs_start = data_base + offset;
        let abs_end = abs_start + length;
        if abs_end > xdbf.len() || length < 8 {
            continue;
        }

        let entry_data = &xdbf[abs_start..abs_end];

        // Check for PNG signature.
        if entry_data[..8] != PNG_SIGNATURE {
            continue;
        }

        // Prefer the title thumbnail (id 0x8000).
        if id == 0x8000 {
            return Ok(entry_data.to_vec());
        }

        // Keep the first PNG we find as a fallback.
        if best_png.is_none() {
            best_png = Some(entry_data.to_vec());
        }
    }

    best_png.ok_or_else(|| invalid("no PNG image found in XDBF resource section"))
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
}
