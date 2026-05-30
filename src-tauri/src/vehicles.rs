//! Nuts & Bolts vehicle save parser.
//!
//! Ported from `VehicleParser.h` in the C++ launcher.
//! Reads binary `.header` files and corresponding data files from the game's save
//! directory and returns them as JSON values consumable by the website's vehicle browser.

use std::io::{Read, Seek, SeekFrom};

use serde_json::{json, Value};

use crate::paths;

// ── Structures ────────────────────────────────────────────────────────────────

struct VehiclePart {
    id: i32,
    px: f32, py: f32, pz: f32,
    rx: f32, ry: f32, rz: f32,
    color: u32,
    is_painted: bool,
}

struct Vehicle {
    name: String,
    parts: Vec<VehiclePart>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Scan the Nuts & Bolts save directory and return a JSON array of vehicle objects.
pub fn reload_vehicles() -> Vec<Value> {
    let Some(base) = paths::vehicle_save_base() else {
        return Vec::new();
    };

    let headers_path = base.join("Headers").join("00000001");
    if !headers_path.is_dir() {
        return Vec::new();
    }

    static VEHICLE_MAGIC: [u8; 24] = [
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x56, 0x00, 0x45, 0x00, 0x48, 0x00, 0x49,
        0x00, 0x43, 0x00, 0x4C, 0x00, 0x45, 0x00, 0x3A,
    ];

    let mut vehicles: Vec<Value> = Vec::new();

    let Ok(entries) = std::fs::read_dir(&headers_path) else { return vehicles };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("header") {
            continue;
        }

        let Ok(mut header_file) = std::fs::File::open(&path) else { continue };

        // Check magic bytes.
        let mut magic = [0u8; 24];
        if header_file.read_exact(&mut magic).is_err() || magic != VEHICLE_MAGIC {
            continue;
        }

        // Read vehicle name from header (offset 26, big-endian UTF-16).
        let _ = header_file.seek(SeekFrom::Start(26));
        let header_name = read_be_utf16_string(&mut header_file, 0x20);

        // Build path to the data file.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let file_id = stem
            .strip_prefix("0x")
            .or_else(|| stem.strip_prefix("0X"))
            .unwrap_or(&stem)
            .to_string();
        let data_path = base.join("00000001").join(&stem).join(&file_id);

        let Ok(mut data_file) = std::fs::File::open(&data_path) else { continue };

        // num_of_parts at 0x08 (big-endian u16).
        let _ = data_file.seek(SeekFrom::Start(0x08));
        let num_of_parts = be_read_u16(&mut data_file) as usize;

        // Vehicle name at 0x28 (big-endian UTF-16).
        let _ = data_file.seek(SeekFrom::Start(0x28));
        let name = read_be_utf16_string(&mut data_file, 0x20);
        let display_name = if name.is_empty() { header_name } else { name };

        // Parts start at 0x84.
        let _ = data_file.seek(SeekFrom::Start(0x84));
        let mut parts: Vec<VehiclePart> = Vec::with_capacity(num_of_parts);

        for _ in 0..num_of_parts {
            let x_pos = read_i8(&mut data_file) as f32;
            let y_pos = read_i8(&mut data_file) as f32;
            let z_pos = read_i8(&mut data_file) as f32;
            let _is_challenge = read_i8(&mut data_file);
            let is_painted = read_i8(&mut data_file) != 0;
            let _unk1 = read_i8(&mut data_file);
            let _unk2 = read_i8(&mut data_file);
            let _unk3 = read_i8(&mut data_file);
            let part_idx = be_read_u32(&mut data_file);
            let yaw   = be_read_f32(&mut data_file);
            let pitch = be_read_f32(&mut data_file);
            let roll  = be_read_f32(&mut data_file);
            let color = be_read_u32(&mut data_file);
            let _unk4 = be_read_i32(&mut data_file);
            let _unk5 = be_read_i32(&mut data_file);

            parts.push(VehiclePart {
                id: part_idx as i32,
                px: x_pos, py: y_pos, pz: z_pos,
                rx: yaw, ry: pitch, rz: roll,
                color,
                is_painted,
            });
        }

        vehicles.push(serialize_vehicle(&Vehicle { name: display_name, parts }));
    }

    vehicles
}

// ── Serialisation ─────────────────────────────────────────────────────────────

fn serialize_vehicle(v: &Vehicle) -> Value {
    let parts: Vec<Value> = v.parts.iter().map(|p| {
        json!({
            "shapeId": p.id,
            "px": p.px, "py": p.py, "pz": p.pz,
            "rx": p.rx, "ry": p.ry, "rz": p.rz,
            "color": p.color,
            "isPainted": p.is_painted,
        })
    }).collect();

    json!({
        "name": v.name,
        "parts": parts,
    })
}

// ── Binary reading helpers (big-endian) ───────────────────────────────────────

fn be_read_u16(f: &mut std::fs::File) -> u16 {
    let mut buf = [0u8; 2];
    let _ = f.read_exact(&mut buf);
    u16::from_be_bytes(buf)
}

fn be_read_u32(f: &mut std::fs::File) -> u32 {
    let mut buf = [0u8; 4];
    let _ = f.read_exact(&mut buf);
    u32::from_be_bytes(buf)
}

fn be_read_i32(f: &mut std::fs::File) -> i32 {
    be_read_u32(f) as i32
}

fn be_read_f32(f: &mut std::fs::File) -> f32 {
    f32::from_bits(be_read_u32(f))
}

fn read_i8(f: &mut std::fs::File) -> i8 {
    let mut buf = [0u8; 1];
    let _ = f.read_exact(&mut buf);
    buf[0] as i8
}

/// Read up to `max_chars` big-endian UTF-16 characters into a String, stopping at NUL.
fn read_be_utf16_string(f: &mut std::fs::File, max_chars: usize) -> String {
    let mut chars: Vec<u16> = Vec::with_capacity(max_chars);
    for _ in 0..max_chars {
        let mut buf = [0u8; 2];
        if f.read_exact(&mut buf).is_err() {
            break;
        }
        let ch = u16::from_be_bytes(buf);
        if ch == 0 {
            break;
        }
        chars.push(ch);
    }
    String::from_utf16_lossy(&chars).to_string()
}
