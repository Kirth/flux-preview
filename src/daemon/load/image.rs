use super::{Payload, Pixels};
use gdk_pixbuf::Pixbuf;
use std::path::Path;

const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_DIM: u32 = 65_536;
/// Zoomed-in decodes stop here (~256 MiB of RGBA); bigger images stay a little soft.
const MAX_FULL_PIXELS: f64 = 64e6;
const MAX_FULL_SIDE: f64 = 16_384.0;

/// Header probe + caps, then decode scaled to `target` (never the full resolution).
///
/// TODO: animated GIF/WebP (first frame only for now).
pub fn load(path: &Path, target: (i32, i32)) -> Result<Payload, String> {
    crate::hit!("image");
    let dim = guard(path)?;
    decode(path, dim, target)
}

/// The full resolution, for zooming in, within `MAX_FULL_PIXELS`.
pub fn load_full(path: &Path) -> Result<Payload, String> {
    crate::hit!("image_full");
    let (w, h) = guard(path)?;
    let (wf, hf) = (w as f64, h as f64);
    let k = (MAX_FULL_PIXELS / (wf * hf))
        .sqrt()
        .min(MAX_FULL_SIDE / wf.max(hf))
        .min(1.0);
    decode(path, (w, h), ((wf * k) as i32, (hf * k) as i32))
}

/// The file and header caps, before any decoder allocates. Returns the dimensions.
fn guard(path: &Path) -> Result<(u32, u32), String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size == 0 || size > MAX_FILE {
        return Err(format!("Not previewing a {size} byte image"));
    }
    let (width, height) = probe(path).ok_or("Unrecognised image header")?;
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(format!("Not previewing a {width}×{height} image"));
    }
    Ok((width, height))
}

fn decode(
    path: &Path,
    (width, height): (u32, u32),
    (tw, th): (i32, i32),
) -> Result<Payload, String> {
    let pixbuf = if width as i32 <= tw && height as i32 <= th {
        Pixbuf::from_file(path)
    } else {
        Pixbuf::from_file_at_scale(path, tw.max(1), th.max(1), true)
    }
    .map_err(|e| e.to_string())?;
    // EXIF orientations 5-8 turn the image on its side.
    let turned = matches!(
        pixbuf.option("orientation").as_deref(),
        Some("5" | "6" | "7" | "8")
    );
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    let (width, height) = if turned {
        (height, width)
    } else {
        (width, height)
    };

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
