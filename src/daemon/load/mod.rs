//! Off-thread loading. Everything here runs on the capped pool, never the GTK thread;
//! results go back to the UI as `AppMsg`s and are dropped there if the session moved on.

mod archive;
mod audio;
mod image;
mod pdf;
mod pdfdoc;
mod text;

pub use archive::{Listing, Node, Tree, group_digits};
pub use audio::AudioInfo;
pub use pdf::{Links, PagePoint, RectPt, Regions, Target, rotate_rect, rotate_size};
pub use pdfdoc::{Dest, OutlineItem};
pub use text::TEXT_CAP;

use crate::app::AppMsg;
use crate::kind::{self, Kind};
use crate::pool::pool;
use crate::session::Session;
use gdk_pixbuf::Pixbuf;
use gtk::gdk;
use gtk::prelude::*;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Decoded pixels, ready to become a `gdk::MemoryTexture` on the GTK thread without any
/// further decoding or copying (ZAP §4: no PNG round trip on the display path).
pub struct Pixels {
    bytes: glib::Bytes,
    width: i32,
    height: i32,
    stride: usize,
    format: gdk::MemoryFormat,
}

impl Pixels {
    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn texture(&self) -> gdk::Texture {
        gdk::MemoryTexture::new(
            self.width,
            self.height,
            self.format,
            &self.bytes,
            self.stride,
        )
        .upcast()
    }

    fn from_pixbuf(pixbuf: &Pixbuf) -> Self {
        Self {
            bytes: pixbuf.read_pixel_bytes(),
            width: pixbuf.width(),
            height: pixbuf.height(),
            stride: pixbuf.rowstride() as usize,
            // gdk-pixbuf is straight (non-premultiplied) RGB(A).
            format: if pixbuf.has_alpha() {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            },
        }
    }
}

pub struct Meta {
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mime: String,
}

pub enum Payload {
    Image {
        pixels: Pixels,
        /// Original dimensions; `pixels` is scaled to the window.
        width: u32,
        height: u32,
    },
    Pdf {
        page: Pixels,
        /// 0-based.
        index: i32,
        pages: i32,
        /// In PDF points.
        size: (f64, f64),
    },
    Text {
        text: String,
        truncated: bool,
    },
    /// Nothing to decode: the GTK thread hands the path to GtkMediaFile / WebKit.
    Video,
    Html,
    Audio(AudioInfo),
    Archive(Box<Listing>),
    /// No preview for this kind; show the info page.
    Info,
    /// The preview failed or was refused (caps, missing GStreamer, ...).
    Error(String),
}

pub struct Job {
    pub id: u64,
    pub path: PathBuf,
    /// The extension's verdict, if it had one.
    pub hint: Option<Kind>,
    /// Physical pixels to decode to: the window, not the full resolution.
    pub target: (i32, i32),
    /// PDFs: clockwise degrees.
    pub rotation: u16,
}

/// Classifies and decodes `job` on the pool, then posts `AppMsg::Loaded`.
pub fn spawn(job: Job, session: Session, sender: relm4::Sender<AppMsg>) {
    pool().spawn(move || {
        if !session.is_current(job.id) {
            return;
        }
        crate::hit!("job");
        let Job {
            id,
            path,
            hint,
            target,
            rotation,
        } = job;

        let (kind, mime) = kind::resolve(&path, hint);
        let stat = std::fs::metadata(&path);
        let meta = Meta {
            size: stat.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: stat.as_ref().ok().and_then(|m| m.modified().ok()),
            mime,
        };

        if !session.is_current(id) {
            return;
        }
        let result = match kind {
            Kind::Image => image::load(&path, target),
            Kind::Pdf => pdf::render(&path, 0, Target::Fit(target), rotation),
            Kind::Archive => {
                archive::load(&path, &session, id).map(|l| Payload::Archive(Box::new(l)))
            }
            Kind::Text => text::load(&path),
            Kind::Audio => Ok(Payload::Audio(audio::load(&path))),
            Kind::Video if !playback_available() => Err(NO_GSTREAMER.into()),
            Kind::Video => Ok(Payload::Video),
            Kind::Html => Ok(Payload::Html),
            Kind::Binary | Kind::Unknown => Ok(Payload::Info),
        };
        let payload = result.unwrap_or_else(Payload::Error);

        let is_pdf = matches!(payload, Payload::Pdf { .. });
        let more_pages = matches!(payload, Payload::Pdf { pages, .. } if pages > 1);
        if !session.is_current(id) {
            return;
        }
        sender.emit(AppMsg::Loaded {
            id,
            kind,
            meta,
            payload,
        });

        // Show first (page 1), then the layout the continuous view needs. Same thread, so
        // the document is still open.
        if more_pages {
            match pdf::layout(&path, rotation) {
                Ok(sizes) if session.is_current(id) => sender.emit(AppMsg::PdfLayout { id, sizes }),
                Ok(_) => {}
                Err(err) => eprintln!("[flux-preview] PDF layout failed: {err}"),
            }
        }
        // Then what navigating needs: the outline and every page's links.
        if is_pdf && session.is_current(id) {
            match pdf::navigation(&path) {
                Ok((outline, links)) if session.is_current(id) => {
                    sender.emit(AppMsg::PdfNavigation { id, outline, links })
                }
                Ok(_) => {}
                Err(err) => eprintln!("[flux-preview] PDF links failed: {err}"),
            }
        }
    });
}

pub struct PageJob {
    pub id: u64,
    pub path: PathBuf,
    pub index: u32,
    /// Device px wide.
    pub width: i32,
    /// The pages still worth rendering; scrolled-past ones are skipped.
    pub keep: Arc<Mutex<Range<u32>>>,
    pub rotation: u16,
}

/// Renders one page for the continuous view, then posts `AppMsg::PdfPage`.
pub fn spawn_page(job: PageJob, session: Session, sender: relm4::Sender<AppMsg>) {
    pool().spawn(move || {
        let PageJob {
            id,
            path,
            index,
            width,
            keep,
            rotation,
        } = job;
        let wanted =
            || session.is_current(id) && keep.lock().map(|k| k.contains(&index)).unwrap_or(false);
        if !wanted() {
            return;
        }
        crate::hit!("pdf_page");
        let pixels = match pdf::render(&path, index as i32, Target::Width(width), rotation) {
            Ok(Payload::Pdf { page, .. }) => Some(page),
            Ok(_) => None,
            Err(err) => {
                eprintln!("[flux-preview] page {} failed: {err}", index + 1);
                None
            }
        };
        if session.is_current(id) {
            sender.emit(AppMsg::PdfPage {
                id,
                index,
                width,
                pixels,
            });
        }
    });
}

/// Sharper or further pixels for the file on screen.
pub enum Detail {
    /// The image at full resolution, for zooming in.
    Full,
    Page {
        index: i32,
        target: Target,
        rotation: u16,
    },
}

pub struct DetailJob {
    pub id: u64,
    /// The view generation: each request bumps it, dropping older renders.
    pub generation: u64,
    pub path: PathBuf,
    pub detail: Detail,
}

/// Decodes `job` on the pool, then posts `AppMsg::Detail`.
pub fn spawn_detail(
    job: DetailJob,
    session: Session,
    generations: Session,
    sender: relm4::Sender<AppMsg>,
) {
    pool().spawn(move || {
        let DetailJob {
            id,
            generation,
            path,
            detail,
        } = job;
        let current = || session.is_current(id) && generations.is_current(generation);
        if !current() {
            return;
        }
        crate::hit!("detail");
        let result = match detail {
            Detail::Full => image::load_full(&path),
            Detail::Page {
                index,
                target,
                rotation,
            } => pdf::render(&path, index, target, rotation),
        };
        if current() {
            sender.emit(AppMsg::Detail {
                id,
                generation,
                payload: result.unwrap_or_else(Payload::Error),
            });
        }
    });
}

pub struct SearchJob {
    pub id: u64,
    /// Each new query bumps it, stopping the previous search.
    pub generation: u64,
    pub path: PathBuf,
    pub query: String,
}

/// Searches the PDF on the pool, posting `AppMsg::SearchHits` per page with matches and
/// `AppMsg::SearchDone` at the end.
pub fn spawn_search(
    job: SearchJob,
    session: Session,
    searches: Session,
    sender: relm4::Sender<AppMsg>,
) {
    pool().spawn(move || {
        let SearchJob {
            id,
            generation,
            path,
            query,
        } = job;
        let current = || session.is_current(id) && searches.is_current(generation);
        if !current() {
            return;
        }
        crate::hit!("pdf_search");
        let result = pdf::find(
            &path,
            &query,
            |page, size, rects| {
                sender.emit(AppMsg::SearchHits {
                    id,
                    generation,
                    page,
                    size,
                    rects,
                });
                current()
            },
            current,
        );
        if let Err(err) = result {
            eprintln!("[flux-preview] search failed: {err}");
        }
        if current() {
            sender.emit(AppMsg::SearchDone { id, generation });
        }
    });
}

pub struct SelectJob {
    pub id: u64,
    /// Each drag step bumps it, dropping the steps before.
    pub generation: u64,
    pub path: PathBuf,
    pub from: PagePoint,
    pub to: PagePoint,
    /// Also extract the text: the drag ended.
    pub text: bool,
}

/// Works out a text selection on the pool, then posts `AppMsg::Selection`.
pub fn spawn_select(
    job: SelectJob,
    session: Session,
    selections: Session,
    sender: relm4::Sender<AppMsg>,
) {
    pool().spawn(move || {
        let SelectJob {
            id,
            generation,
            path,
            from,
            to,
            text,
        } = job;
        let current = || session.is_current(id) && selections.is_current(generation);
        if !current() {
            return;
        }
        crate::hit!("pdf_select");
        match pdf::select(&path, from, to, text) {
            Ok((regions, text)) if current() => sender.emit(AppMsg::Selection {
                id,
                generation,
                regions,
                text,
            }),
            Ok(_) => {}
            Err(err) => eprintln!("[flux-preview] selection failed: {err}"),
        }
    });
}

/// Looks up the freedesktop thumbnail on the pool and posts `AppMsg::Thumb` if found.
pub fn spawn_thumb(id: u64, path: PathBuf, session: Session, sender: relm4::Sender<AppMsg>) {
    pool().spawn(move || {
        if !session.is_current(id) {
            return;
        }
        crate::hit!("thumb");
        let Some(thumb) = crate::thumbcache::lookup(&path) else {
            return;
        };
        let Ok(pixbuf) = Pixbuf::from_file(&thumb) else {
            return;
        };
        if session.is_current(id) {
            let pixels = Pixels::from_pixbuf(&pixbuf);
            sender.emit(AppMsg::Thumb { id, pixels });
        }
    });
}

const NO_GSTREAMER: &str = "Playback unavailable: GStreamer 'playbin3' not found \
                            (is GST_PLUGIN_SYSTEM_PATH_1_0 set?)";

/// Whether GStreamer can play media in this process. Blocking (registry scan on first
/// call), so only call it from the pool.
///
/// GtkMediaFile aborts the whole process when GStreamer has no `playbin3` (e.g. on NixOS
/// without GST_PLUGIN_SYSTEM_PATH_1_0), so check before creating one. From flux
/// `src/ui/ops/view/video_preview.rs`.
pub fn playback_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        crate::hit!("playback_available");
        let ok = gstreamer::init().is_ok() && gstreamer::ElementFactory::find("playbin3").is_some();
        if !ok {
            eprintln!("[flux-preview] {NO_GSTREAMER}");
        }
        ok
    })
}
