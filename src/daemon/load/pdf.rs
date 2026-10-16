use super::{Payload, Pixels};
use gtk::gdk;
use poppler::cairo;
use std::path::Path;

/// Page 1 straight to pixels, fitted to `target`. From flux `src/utils/media/pdf.rs`,
/// minus the PNG round trip.
///
/// TODO: render further pages lazily on scroll / page keys.
pub fn load(path: &Path, (tw, th): (i32, i32)) -> Result<Payload, String> {
    crate::hit!("pdf");
    let doc = poppler::PopplerDocument::new_from_file(path, None).map_err(|e| e.to_string())?;
    let pages = doc.get_n_pages() as i32;
    let page = doc.get_page(0).ok_or("PDF has no pages")?;
    let (page_w, page_h) = page.get_size();
    if page_w <= 0.0 || page_h <= 0.0 {
        return Err("PDF page has no size".into());
    }

    let scale = (tw as f64 / page_w).min(th as f64 / page_h);
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

    let page = Pixels {
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
    Ok(Payload::Pdf { page, pages })
}
