//! Off-thread loading. Everything here runs on the capped pool, never the GTK thread;
//! results go back to the UI as `AppMsg`s and are dropped there if the session moved on.

mod archive;
mod audio;
mod image;
mod pdf;
mod text;

pub use archive::{Listing, Node, Tree, group_digits};
pub use audio::AudioInfo;
pub use pdf::Target;
pub use text::TEXT_CAP;

use crate::app::AppMsg;
use crate::kind::{self, Kind};
use crate::pool::pool;
use crate::session::Session;
use gdk_pixbuf::Pixbuf;
use gtk::gdk;
use gtk::prelude::*;
use std::path::PathBuf;
use std::sync::OnceLock;
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
            Kind::Pdf => pdf::render(&path, 0, Target::Fit(target)),
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

        if session.is_current(id) {
            sender.emit(AppMsg::Loaded {
                id,
                kind,
                meta,
                payload,
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
    },
}

pub struct DetailJob {
    pub id: u64,
    /// The view generation: page flips and zooms bump it, dropping older renders.
    pub generation: u64,
    pub path: PathBuf,
    pub detail: Detail,
    /// Passed back untouched: the direction of the page flip this render is for.
    pub flip: Option<i32>,
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
            flip,
        } = job;
        let current = || session.is_current(id) && generations.is_current(generation);
        if !current() {
            return;
        }
        crate::hit!("detail");
        let result = match detail {
            Detail::Full => image::load_full(&path),
            Detail::Page { index, target } => pdf::render(&path, index, target),
        };
        if current() {
            sender.emit(AppMsg::Detail {
                id,
                generation,
                flip,
                payload: result.unwrap_or_else(Payload::Error),
            });
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
