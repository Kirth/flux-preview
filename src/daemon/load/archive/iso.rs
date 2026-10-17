//! ISO 9660 in Rust; UDF through `7z l -slt`. From flux `backends/iso.rs`.

use super::tool::ToolError;
use super::{Lister, Raw};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

struct Device(std::fs::File);

impl iso9660_simple::Read for Device {
    fn read(&mut self, position: usize, buffer: &mut [u8]) -> Option<()> {
        self.0.seek(SeekFrom::Start(position as u64)).ok()?;
        self.0.read_exact(buffer).ok()
    }
}

/// The UDF Anchor Volume Descriptor Pointer (tag 2) sits at sector 256.
pub fn has_udf(file: &mut std::fs::File) -> bool {
    let mut tag = [0u8; 2];
    file.seek(SeekFrom::Start(256 * 2048)).is_ok()
        && file.read_exact(&mut tag).is_ok()
        && u16::from_le_bytes(tag) == 2
}

pub fn list(path: &Path, udf: bool, lister: &mut Lister) -> Result<(), String> {
    if udf {
        return super::rar::sevenz_slt(path, lister).map_err(|e| match e {
            ToolError::Missing => "Listing UDF images needs 7-Zip (7z)".into(),
            ToolError::Failed(err) => err,
        });
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut iso = iso9660_simple::ISO9660::from_device(Device(file))
        .ok_or("Could not read the ISO 9660 volume")?;
    let root = match iso.root().lba.get() as usize {
        0 => 16,
        lba => lba,
    };
    let mut seen = HashSet::new();
    walk(&mut iso, root, "", lister, &mut seen);
    Ok(())
}

/// Depth-first; `seen` guards against directory records that loop back.
fn walk(
    iso: &mut iso9660_simple::ISO9660,
    lba: usize,
    dir: &str,
    lister: &mut Lister,
    seen: &mut HashSet<usize>,
) -> bool {
    if !seen.insert(lba) {
        return true;
    }
    let entries: Vec<_> = iso.read_directory(lba).collect();
    for entry in entries {
        let name = entry
            .name
            .split(';')
            .next()
            .unwrap_or("")
            .trim_end_matches('.')
            .to_owned();
        if name.is_empty() || name == "\x00" || name == "\x01" {
            continue;
        }
        let path = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        let is_dir = entry.is_folder();
        let child = entry.record.lba.get() as usize;
        let keep_going = lister.push(Raw {
            path: path.clone(),
            is_dir,
            size: Some(if is_dir { 0 } else { entry.file_size() as u64 }),
            mtime: None,
            encrypted: false,
        });
        if !keep_going || (is_dir && child != 0 && !walk(iso, child, &path, lister, seen)) {
            return false;
        }
    }
    true
}
