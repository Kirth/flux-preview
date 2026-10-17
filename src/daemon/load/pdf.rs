use super::{Payload, Pixels};
use gtk::gdk;
use poppler::cairo;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Zoomed renders stop here; past it the page is upscaled on screen.
const MAX_SIDE: f64 = 8192.0;
const MAX_AREA: f64 = 48e6;

#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// Fitted into this many device px.
    Fit((i32, i32)),
    /// Device px per PDF point.
    Scale(f64),
    /// This many device px wide.
    Width(i32),
}

type Key = (PathBuf, Option<SystemTime>, u64);

thread_local! {
    /// The last document this pool thread opened: continuous scrolling renders page after
    /// page of the same file, and reparsing it for each is the slow part.
    static OPEN: RefCell<Option<(Key, poppler::PopplerDocument)>> = const { RefCell::new(None) };
}

fn with_doc<R>(
    path: &Path,
    f: impl FnOnce(&poppler::PopplerDocument) -> Result<R, String>,
) -> Result<R, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let key = (path.to_path_buf(), meta.modified().ok(), meta.len());
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        if open.as_ref().is_none_or(|(k, _)| *k != key) {
            *open = None;
            let doc =
                poppler::PopplerDocument::new_from_file(path, None).map_err(|e| e.to_string())?;
            *open = Some((key, doc));
        }
        f(&open.as_ref().expect("just opened").1)
    })
}

/// Every page's size in points, for the continuous view's layout.
pub fn layout(path: &Path) -> Result<Vec<(f64, f64)>, String> {
    crate::hit!("pdf_layout");
    with_doc(path, |doc| {
        Ok((0..doc.get_n_pages())
            .map(|i| {
                doc.get_page(i)
                    .map(|p| p.get_size())
                    .filter(|&(w, h)| w > 0.0 && h > 0.0)
                    .unwrap_or((612.0, 792.0))
            })
            .collect())
    })
}

/// One page straight to pixels. From flux `src/utils/media/pdf.rs`, minus the PNG round
/// trip.
pub fn render(path: &Path, index: i32, target: Target) -> Result<Payload, String> {
    crate::hit!("pdf");
    with_doc(path, |doc| render_page(doc, index, target))
}

fn render_page(
    doc: &poppler::PopplerDocument,
    index: i32,
    target: Target,
) -> Result<Payload, String> {
    let pages = doc.get_n_pages() as i32;
    let index = index.clamp(0, (pages - 1).max(0));
    let page = doc.get_page(index as usize).ok_or("PDF has no pages")?;
    let (page_w, page_h) = page.get_size();
    if page_w <= 0.0 || page_h <= 0.0 {
        return Err("PDF page has no size".into());
    }

    let scale = match target {
        Target::Fit((tw, th)) => (tw as f64 / page_w).min(th as f64 / page_h),
        Target::Scale(scale) => scale,
        Target::Width(w) => w as f64 / page_w,
    }
    .min(MAX_SIDE / page_w.max(page_h))
    .min((MAX_AREA / (page_w * page_h)).sqrt());
    let w = (page_w * scale).round().max(1.0) as i32;
    let h = (page_h * scale).round().max(1.0) as i32;

    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).map_err(|e| e.to_string())?;
    {
        let cx = cairo::Context::new(&surface).map_err(|e| e.to_string())?;
        // PDF pages are transparent by default.
        cx.set_source_rgb(1.0, 1.0, 1.0);
        cx.paint().map_err(|e| e.to_string())?;
        cx.scale(scale, scale);
        page.render(&cx);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().map_err(|e| e.to_string())?;

    let pixels = Pixels {
        bytes: glib::Bytes::from(&*data),
        width: w,
        height: h,
        stride,
        // cairo ARGB32 is native-endian premultiplied.
        format: if cfg!(target_endian = "little") {
            gdk::MemoryFormat::B8g8r8a8Premultiplied
        } else {
            gdk::MemoryFormat::A8r8g8b8Premultiplied
        },
    };
    Ok(Payload::Pdf {
        page: pixels,
        index,
        pages,
        size: (page_w, page_h),
    })
}
