use super::pdfdoc::{Dest, Document, OutlineItem};
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
    static OPEN: RefCell<Option<(Key, Document)>> = const { RefCell::new(None) };
}

fn with_doc<R>(path: &Path, f: impl FnOnce(&Document) -> Result<R, String>) -> Result<R, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let key = (path.to_path_buf(), meta.modified().ok(), meta.len());
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        if open.as_ref().is_none_or(|(k, _)| *k != key) {
            *open = None;
            let doc = Document::open(path)?;
            *open = Some((key, doc));
        }
        f(&open.as_ref().expect("just opened").1)
    })
}

pub use super::pdfdoc::RectPt;

/// Clockwise degrees, one of 0, 90, 180, 270.
fn quarter_turns(rotation: u16) -> u16 {
    (rotation / 90) % 4
}

/// A page's size once rotated.
pub fn rotate_size((w, h): (f64, f64), rotation: u16) -> (f64, f64) {
    if quarter_turns(rotation) % 2 == 1 {
        (h, w)
    } else {
        (w, h)
    }
}

/// Where `rect` on an unrotated page of `size` lands once the page is rotated.
pub fn rotate_rect((x, y, w, h): RectPt, (pw, ph): (f64, f64), rotation: u16) -> RectPt {
    match quarter_turns(rotation) {
        1 => (ph - y - h, x, h, w),
        2 => (pw - x - w, ph - y - h, w, h),
        3 => (y, pw - x - w, h, w),
        _ => (x, y, w, h),
    }
}

/// Every page's size in points, rotated, for the continuous view's layout.
pub fn layout(path: &Path, rotation: u16) -> Result<Vec<(f64, f64)>, String> {
    crate::hit!("pdf_layout");
    with_doc(path, |doc| {
        Ok((0..doc.pages())
            .map(|i| {
                let size = doc
                    .page(i)
                    .map(|p| p.size())
                    .filter(|&(w, h)| w > 0.0 && h > 0.0)
                    .unwrap_or((612.0, 792.0));
                rotate_size(size, rotation)
            })
            .collect())
    })
}

/// One page straight to pixels, rotated clockwise by `rotation` degrees. From flux
/// `src/utils/media/pdf.rs`, minus the PNG round trip.
pub fn render(path: &Path, index: i32, target: Target, rotation: u16) -> Result<Payload, String> {
    crate::hit!("pdf");
    with_doc(path, |doc| render_page(doc, index, target, rotation))
}

fn render_page(
    doc: &Document,
    index: i32,
    target: Target,
    rotation: u16,
) -> Result<Payload, String> {
    let pages = doc.pages() as i32;
    let index = index.clamp(0, (pages - 1).max(0));
    let page = doc.page(index as u32).ok_or("PDF has no pages")?;
    let unrotated = page.size();
    if unrotated.0 <= 0.0 || unrotated.1 <= 0.0 {
        return Err("PDF page has no size".into());
    }
    let (page_w, page_h) = rotate_size(unrotated, rotation);

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
        let (uw, uh) = unrotated;
        match quarter_turns(rotation) {
            1 => cx.translate(uh, 0.0),
            2 => cx.translate(uw, uh),
            3 => cx.translate(0.0, uw),
            _ => {}
        }
        cx.rotate(quarter_turns(rotation) as f64 * std::f64::consts::FRAC_PI_2);
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

/// Case-insensitive text search, page by page. `on_page` gets each page with matches
/// (index, unrotated size in points, the matches) and returns `false` to stop.
pub fn find(
    path: &Path,
    query: &str,
    mut on_page: impl FnMut(u32, (f64, f64), Vec<RectPt>) -> bool,
    mut keep_going: impl FnMut() -> bool,
) -> Result<(), String> {
    let query = std::ffi::CString::new(query).map_err(|e| e.to_string())?;
    with_doc(path, |doc| {
        for index in 0..doc.pages() {
            if !keep_going() {
                break;
            }
            let Some(page) = doc.page(index) else {
                continue;
            };
            let rects = page.find(&query);
            if !rects.is_empty() && !on_page(index, page.size(), rects) {
                break;
            }
        }
        Ok(())
    })
}

/// A page's links, in unrotated points.
pub type Links = Vec<(RectPt, Dest)>;

/// The outline and every page's links: what navigation needs, after the first page shows.
pub fn navigation(path: &Path) -> Result<(Vec<OutlineItem>, Vec<Links>), String> {
    crate::hit!("pdf_navigation");
    with_doc(path, |doc| {
        let links = (0..doc.pages()).map(|i| doc.links(i)).collect();
        Ok((doc.outline(), links))
    })
}

/// A point on a page, in unrotated points from its top-left corner.
pub type PagePoint = (u32, (f64, f64));

/// The boxes a text selection covers, per page.
pub type Regions = Vec<(u32, Vec<RectPt>)>;

/// A text selection dragged from `from` to `to`: the boxes it covers per page, and its
/// text when `text`.
pub fn select(
    path: &Path,
    from: PagePoint,
    to: PagePoint,
    text: bool,
) -> Result<(Regions, Option<String>), String> {
    with_doc(path, |doc| {
        let (from, to) = if (to.0, to.1.1) < (from.0, from.1.1) {
            (to, from)
        } else {
            (from, to)
        };
        let mut regions = Vec::new();
        let mut parts = Vec::new();
        for index in from.0..=to.0.min(doc.pages().saturating_sub(1)) {
            let Some(page) = doc.page(index) else {
                continue;
            };
            let size = page.size();
            let start = if index == from.0 { from.1 } else { (0.0, 0.0) };
            let end = if index == to.0 { to.1 } else { size };
            regions.push((index, page.selected_region(start, end)));
            if text {
                parts.push(page.selected_text(start, end));
            }
        }
        let text = text.then(|| {
            parts
                .iter()
                .map(|p| p.trim_end())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        });
        Ok((regions, text))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: (f64, f64) = (100.0, 200.0);
    const RECT: RectPt = (10.0, 20.0, 30.0, 5.0);

    #[test]
    fn rects_follow_the_page_around() {
        assert_eq!(rotate_rect(RECT, PAGE, 0), RECT);
        // Clockwise: the left edge becomes the top.
        assert_eq!(rotate_rect(RECT, PAGE, 90), (175.0, 10.0, 5.0, 30.0));
        assert_eq!(rotate_rect(RECT, PAGE, 180), (60.0, 175.0, 30.0, 5.0));
        assert_eq!(rotate_rect(RECT, PAGE, 270), (20.0, 60.0, 5.0, 30.0));
    }

    /// A PDF from its objects (numbered from 1; the catalog first), written to a temp file.
    fn write_pdf(name: &str, objects: &[String]) -> PathBuf {
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf += &format!("{} 0 obj\n{body}\nendobj\n", i + 1);
        }
        let xref = pdf.len();
        pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            pdf += &format!("{offset:010} 00000 n \n");
        }
        pdf += &format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        );
        let path =
            std::env::temp_dir().join(format!("flux-preview-{name}-{}.pdf", std::process::id()));
        std::fs::write(&path, pdf).unwrap();
        path
    }

    fn stream(content: &str) -> String {
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        )
    }

    const FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

    /// One 200×300 pt page with "Hello" set 50 pt from the top (PDF y = 250).
    fn hello_pdf() -> PathBuf {
        write_pdf(
            "find",
            &[
                "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] \
                 /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                    .to_owned(),
                stream("BT /F1 20 Tf 30 250 Td (Hello) Tj ET"),
                FONT.to_owned(),
            ],
        )
    }

    /// Two 200×300 pt pages. Page 1: "Hello world", a link to page 2 at PDF y = 200
    /// (100 pt from the top) and one to a web page; page 2: "Second page". The outline has
    /// one entry, "Chapter", leading to page 2.
    fn linked_pdf() -> PathBuf {
        let page = |contents: u32, annots: &str| {
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] \
                 /Resources << /Font << /F1 7 0 R >> >> /Contents {contents} 0 R {annots} >>"
            )
        };
        write_pdf(
            "links",
            &[
                "<< /Type /Catalog /Pages 2 0 R /Outlines 8 0 R >>".to_owned(),
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
                page(5, "/Annots [10 0 R 11 0 R]"),
                page(6, ""),
                stream("BT /F1 20 Tf 30 250 Td (Hello world) Tj ET"),
                stream("BT /F1 20 Tf 30 250 Td (Second page) Tj ET"),
                FONT.to_owned(),
                "<< /Type /Outlines /First 9 0 R /Last 9 0 R /Count 1 >>".to_owned(),
                "<< /Title (Chapter) /Parent 8 0 R /Dest [4 0 R /XYZ 0 200 0] >>".to_owned(),
                "<< /Type /Annot /Subtype /Link /Rect [10 100 60 120] /Border [0 0 0] \
                 /Dest [4 0 R /XYZ 0 200 0] >>"
                    .to_owned(),
                "<< /Type /Annot /Subtype /Link /Rect [100 100 150 120] /Border [0 0 0] \
                 /A << /S /URI /URI (https://example.org/) >> >>"
                    .to_owned(),
            ],
        )
    }

    #[test]
    fn links_and_outline_resolve() {
        let path = linked_pdf();
        let (outline, links) = navigation(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let to_page_2 = Dest::Page {
            page: 1,
            y: Some(100.0),
        };
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].title, "Chapter");
        assert_eq!(outline[0].dest, Some(to_page_2.clone()));
        assert_eq!(links.len(), 2);
        assert!(links[1].is_empty());
        let mut page_1 = links[0].clone();
        page_1.sort_by(|a, b| a.0.0.total_cmp(&b.0.0));
        // PDF y 100..120 is 180..200 pt from the top.
        assert_eq!(
            page_1,
            vec![
                ((10.0, 180.0, 50.0, 20.0), to_page_2),
                (
                    (100.0, 180.0, 50.0, 20.0),
                    Dest::Uri("https://example.org/".into())
                ),
            ]
        );
    }

    #[test]
    fn selection_spans_pages_in_reading_order() {
        let path = linked_pdf();
        // From the right half of the "H" on page 1 (it spans x 30..44) to just past
        // "Second" on page 2, dragged backwards.
        let (regions, text) = select(&path, (1, (95.0, 45.0)), (0, (40.0, 45.0)), true).unwrap();
        std::fs::remove_file(&path).ok();
        let text = text.unwrap();
        assert_eq!(text, "ello world\nSecond");
        assert_eq!(regions.iter().map(|r| r.0).collect::<Vec<_>>(), vec![0, 1]);
        assert!(regions.iter().all(|(_, rects)| !rects.is_empty()));
        // The boxes sit on the text line, 30..50 pt from the top.
        let (_, y, _, h) = regions[0].1[0];
        assert!(y > 25.0 && y + h < 60.0, "y = {y}, h = {h}");
    }

    #[test]
    fn find_reports_top_left_rects() {
        let path = hello_pdf();
        let mut hits = Vec::new();
        find(
            &path,
            "hello",
            |page, size, rects| {
                hits.push((page, size, rects));
                true
            },
            || true,
        )
        .unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(hits.len(), 1);
        let (page, size, rects) = &hits[0];
        assert_eq!((*page, *size, rects.len()), (0, (200.0, 300.0), 1));
        let (x, y, w, h) = rects[0];
        // The baseline sits 50 pt from the top: the word spans roughly 30..50 pt down.
        assert!((x - 30.0).abs() < 2.0, "x = {x}");
        assert!(
            y > 25.0 && y < 40.0 && y + h > 45.0 && y + h < 60.0,
            "y = {y}, h = {h}"
        );
        assert!(w > 30.0 && w < 70.0, "w = {w}");
    }

    #[test]
    fn four_quarter_turns_are_a_full_turn() {
        let mut rect = RECT;
        let mut size = PAGE;
        for _ in 0..4 {
            rect = rotate_rect(rect, size, 90);
            size = rotate_size(size, 90);
        }
        assert_eq!((rect, size), (RECT, PAGE));
    }
}
