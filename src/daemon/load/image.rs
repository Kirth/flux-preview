use super::{Payload, Pixels};
use gdk_pixbuf::Pixbuf;
use std::path::Path;

const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_DIM: u32 = 65_536;

/// Header probe + caps, then decode scaled to `target` (never the full resolution).
///
/// TODO: full resolution on zoom; animated GIF/WebP (first frame only for now).
pub fn load(path: &Path, (tw, th): (i32, i32)) -> Result<Payload, String> {
    crate::hit!("image");
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size == 0 || size > MAX_FILE {
        return Err(format!("Not previewing a {size} byte image"));
    }
    let (width, height) = probe(path).ok_or("Unrecognised image header")?;
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(format!("Not previewing a {width}×{height} image"));
    }

    let pixbuf = if width as i32 <= tw && height as i32 <= th {
        Pixbuf::from_file(path)
    } else {
        Pixbuf::from_file_at_scale(path, tw, th, true)
    }
    .map_err(|e| e.to_string())?;
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);

    Ok(Payload::Image {
        pixels: Pixels::from_pixbuf(&pixbuf),
        width,
        height,
    })
}

/// Dimensions from the header only, before any decoder allocates.
///
/// `imagesize` is pure Rust and header-only. The gdk-pixbuf fallback is limited to SVG:
/// "glycin can `realloc` gigabytes on a malformed header before the RSS watchdog
/// notices" (flux `src/utils/media/image.rs`).
fn probe(path: &Path) -> Option<(u32, u32)> {
    if let Ok(dim) = imagesize::size(path) {
        return Some((
            u32::try_from(dim.width).ok()?,
            u32::try_from(dim.height).ok()?,
        ));
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "svg" | "svgz") {
        return None;
    }
    let (_, w, h) = Pixbuf::file_info(path)?;
    Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
}
