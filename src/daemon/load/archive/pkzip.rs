//! ZIP and everything built on it (jar, apk, cbz, whl…). From flux `backends/zip.rs`,
//! reading the central directory only: encrypted entries are listed, never decrypted.

use super::{Lister, Raw, local_time};
use std::path::Path;

pub fn list(path: &Path, lister: &mut Lister) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| format!("ZIP: {e}"))?;
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(|e| format!("ZIP: {e}"))?;
        let raw = Raw {
            path: entry.name().to_owned(),
            is_dir: entry.is_dir(),
            size: Some(entry.size()),
            mtime: entry.last_modified().and_then(|t| {
                local_time(
                    t.year() as i32,
                    t.month() as i32,
                    t.day() as i32,
                    t.hour() as i32,
                    t.minute() as i32,
                    t.second() as f64,
                )
            }),
            encrypted: entry.encrypted(),
        };
        if !lister.push(raw) {
            break;
        }
    }
    Ok(())
}
