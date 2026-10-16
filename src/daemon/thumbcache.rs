//! Read-only lookup in the freedesktop thumbnail cache (~/.cache/thumbnails).
//!
//! Shared with GNOME, flux and friends, so it is often warm before we ever run. Shown as an
//! instant placeholder while the real preview decodes. Cribbed from flux
//! `src/utils/media/thumbnail.rs`.
//!
//! TODO: validate via the PNG's Thumb::MTime chunk as the spec says, not the file mtime.

use gio::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};

const TIERS: [&str; 4] = ["xx-large", "x-large", "large", "normal"];

/// Largest valid cached thumbnail for `path`, if any.
pub fn lookup(path: &Path) -> Option<PathBuf> {
    crate::hit!("lookup");
    let root = dirs::cache_dir()?.join("thumbnails");
    if path.starts_with(&root) {
        return None;
    }
    let source = fs::metadata(path).ok()?;
    let uri = gio::File::for_path(path).uri();
    let name = format!("{:x}.png", md5::compute(uri.as_bytes()));
    TIERS
        .iter()
        .map(|tier| root.join(tier).join(&name))
        .find(|thumb| is_valid(thumb, &source))
}

fn is_valid(thumb: &Path, source: &fs::Metadata) -> bool {
    let Ok(meta) = fs::metadata(thumb) else {
        return false;
    };
    let stale = matches!(
        (meta.modified(), source.modified()),
        (Ok(thumb_time), Ok(source_time)) if source_time > thumb_time
    );
    // Zero bytes = partial write; > 1 MiB = not a thumbnail we want to decode on a hunch.
    !stale && meta.len() > 0 && meta.len() <= 1024 * 1024
}
