use super::Payload;
use std::io::Read;
use std::path::Path;

/// Read only what we show (ZAP §6).
pub const TEXT_CAP: u64 = 512 * 1024;

/// First 512 KiB as (lossy) UTF-8. Plain text only for now.
///
/// TODO: syntax highlighting of the visible part on a worker (needs gtksourceview5),
/// markdown rendering.
pub fn load(path: &Path) -> Result<Payload, String> {
    crate::hit!("text");
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::with_capacity(len.min(TEXT_CAP) as usize);
    file.take(TEXT_CAP)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;

    // The sniff only saw 4 KiB; NULs further in still mean binary.
    if buf.contains(&0) {
        return Ok(Payload::Info);
    }
    Ok(Payload::Text {
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated: len > TEXT_CAP,
    })
}
