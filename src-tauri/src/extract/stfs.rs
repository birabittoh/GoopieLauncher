use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const STFS_BLOCK_SIZE: usize = 0x1000;
const FILE_TABLE_OFFSET: u64 = 0xC000;
const FILE_TABLE_ENTRY_SIZE: usize = 0x40;
const ROOT_PARENT: u16 = 0xFFFF;
const END_OF_CHAIN: u32 = 0xFFFFFF;

struct StfsEntry {
    index: usize,
    name: String,
    flags: u8,
    blocks: u32,
    start_block: u32,
    parent: u16,
    size: u32,
}

impl StfsEntry {
    fn is_directory(&self) -> bool {
        (self.flags & 0x80) != 0
    }
}

fn read_uint24_le(data: &[u8]) -> u32 {
    u32::from(data[0]) | (u32::from(data[1]) << 8) | (u32::from(data[2]) << 16)
}

fn read_uint24_be(data: &[u8]) -> u32 {
    (u32::from(data[0]) << 16) | (u32::from(data[1]) << 8) | u32::from(data[2])
}

fn physical_block(logical_block: u32) -> u32 {
    let group = logical_block / 0xAA;
    let level1_groups = group / 0xAA;
    let level1_overhead = if level1_groups > 0 { level1_groups + 1 } else { 0 };
    logical_block + 0x0C + group + (if group > 0 { 1 } else { 0 }) + level1_overhead
}

fn physical_offset(logical_block: u32) -> u64 {
    u64::from(physical_block(logical_block)) * STFS_BLOCK_SIZE as u64
}

fn hash_entry_offset(logical_block: u32) -> u64 {
    let group = logical_block / 0xAA;
    let index = logical_block % 0xAA;
    let level1_groups = group / 0xAA;
    let level1_overhead = if level1_groups > 0 { level1_groups + 1 } else { 0 };
    let table_block = 0x0B + (group * 0xAB) + (if group > 0 { 1 } else { 0 }) + level1_overhead;
    u64::from(table_block) * STFS_BLOCK_SIZE as u64 + u64::from(index) * 0x18
}

fn next_block(source: &mut std::fs::File, logical_block: u32) -> std::io::Result<u32> {
    source.seek(SeekFrom::Start(hash_entry_offset(logical_block) + 0x15))?;
    let mut buf = [0u8; 3];
    source.read_exact(&mut buf)?;
    Ok(read_uint24_be(&buf))
}

fn parse_entries(path: &str) -> std::io::Result<Vec<StfsEntry>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(FILE_TABLE_OFFSET))?;

    let mut entries = Vec::new();
    let mut index = 0usize;

    loop {
        let mut raw = [0u8; FILE_TABLE_ENTRY_SIZE];
        if f.read_exact(&mut raw).is_err() {
            break;
        }
        if raw.iter().all(|&b| b == 0) {
            break;
        }

        let name_flags = raw[0x28];
        let name_len = (name_flags & 0x3F) as usize;
        if name_len == 0 {
            break;
        }

        let name = String::from_utf8_lossy(&raw[..name_len]).into_owned();

        entries.push(StfsEntry {
            index,
            name,
            flags: name_flags,
            blocks: read_uint24_le(&raw[0x29..0x2C]),
            start_block: read_uint24_le(&raw[0x2F..0x32]),
            parent: u16::from_be_bytes([raw[0x32], raw[0x33]]),
            size: u32::from_be_bytes([raw[0x34], raw[0x35], raw[0x36], raw[0x37]]),
        });
        index += 1;
    }

    if !entries.iter().any(|e| e.name == "default.xex") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "could not find default.xex in the STFS file table",
        ));
    }

    Ok(entries)
}

fn entry_path(entry: &StfsEntry, entries: &[StfsEntry]) -> std::io::Result<std::path::PathBuf> {
    let mut parts = vec![entry.name.clone()];
    let mut parent = entry.parent;
    let mut seen = std::collections::HashSet::new();
    seen.insert(entry.index);

    while parent != ROOT_PARENT {
        let pi = parent as usize;
        if pi >= entries.len() || seen.contains(&pi) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid parent chain for {}", entry.name),
            ));
        }
        parts.push(entries[pi].name.clone());
        seen.insert(pi);
        parent = entries[pi].parent;
    }

    parts.reverse();
    Ok(parts.iter().collect())
}

fn extract_file(
    source: &mut std::fs::File,
    entry: &StfsEntry,
    dest_path: &Path,
) -> std::io::Result<()> {
    if let Some(parent_dir) = dest_path.parent() {
        std::fs::create_dir_all(parent_dir)?;
    }

    let mut remaining = entry.size as usize;
    let blocks_to_copy = std::cmp::max(
        entry.blocks as usize,
        (entry.size as usize + STFS_BLOCK_SIZE - 1) / STFS_BLOCK_SIZE,
    );
    let mut logical_block = entry.start_block;
    let mut out_file = std::fs::File::create(dest_path)?;

    for block_index in 0..blocks_to_copy {
        if remaining == 0 {
            break;
        }
        if logical_block == END_OF_CHAIN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected end of block chain while extracting {}", entry.name),
            ));
        }

        source.seek(SeekFrom::Start(physical_offset(logical_block)))?;
        let to_read = std::cmp::min(STFS_BLOCK_SIZE, remaining);
        let mut buf = vec![0u8; to_read];
        source.read_exact(&mut buf)?;
        out_file.write_all(&buf)?;
        remaining -= to_read;

        if block_index + 1 < blocks_to_copy {
            logical_block = next_block(source, logical_block)?;
        }
    }

    Ok(())
}

pub fn extract(path: &str, dest: &Path) -> std::io::Result<usize> {
    let entries = parse_entries(path)?;
    let mut source = std::fs::File::open(path)?;
    let mut count = 0usize;

    for i in 0..entries.len() {
        let rel_path = entry_path(&entries[i], &entries)?;
        let out_path = dest.join(&rel_path);

        if entries[i].is_directory() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            extract_file(&mut source, &entries[i], &out_path)?;
            count += 1;
        }
    }

    Ok(count)
}
