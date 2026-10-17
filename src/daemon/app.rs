//! The previewer window: one relm4 component, kept alive (hidden) for the daemon's life.

use crate::kind::{self, Kind};
use crate::load::{
    self, Dest, Detail, DetailJob, Job, Links, Meta, OutlineItem, PageJob, PagePoint, Payload,
    Pixels, RectPt, Regions, SearchJob, SelectJob, Target,
};
use crate::places::{self, Place, Places};
use crate::session::Session;
use crate::view::{Outline, PAN_STEP, PdfBar, Views, ZoomCmd, format_size};
use adw::prelude::*;
use flux_preview::ipc::{self, Request};
use relm4::prelude::*;
use std::collections::HashMap;
use std::ops::Range;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_SIZE: (i32, i32) = (960, 680);
/// Holding an arrow key: only decode once the user pauses this long (ZAP §3).
const STEP_DEBOUNCE: Duration = Duration::from_millis(40);
/// Fast loads never flash a spinner (ZAP §2).
const SPINNER_DELAY: Duration = Duration::from_millis(150);
/// Expensive one-time setup waits until after the first frame (ZAP §1).
const WARMUP_DELAY: Duration = Duration::from_millis(75);
const SLOW_MSG: Duration = Duration::from_millis(32);

pub struct Init {
    pub listener: UnixListener,
    pub initial: Option<Request>,
}

pub enum AppMsg {
    Request(Request),
    Step(i64),
    Hide,
    OpenExternal,
    ToggleFullscreen,
    Quit,
    /// Debounced: start decoding for session `id`.
    Load(u64),
    SpinnerDue(u64),
    Thumb {
        id: u64,
        pixels: Pixels,
    },
    Loaded {
        id: u64,
        kind: Kind,
        meta: Meta,
        payload: Payload,
    },
    Warmup,
    Zoom(ZoomCmd),
    Page(PageCmd),
    /// The image page shows more device px than its texture has.
    DetailWanted(i32, i32),
    Detail {
        id: u64,
        generation: u64,
        payload: Payload,
    },
    /// `None`: fitted to the window.
    ZoomChanged(Option<f64>),
    /// Every page's size in points; arrives right after page 1.
    PdfLayout {
        id: u64,
        sizes: Vec<(f64, f64)>,
    },
    /// The continuous view wants these pages at these widths; only `keep` is worth
    /// rendering any more.
    PagesWanted {
        pages: Vec<(u32, i32)>,
        keep: Range<u32>,
    },
    PdfPage {
        id: u64,
        index: u32,
        width: i32,
        pixels: Option<Pixels>,
    },
    /// The page in the middle of the continuous view.
    PageShown(u32),
    /// PDFs: clockwise degrees, ±90.
    Rotate(i32),
    /// Closes the search if open, else hides.
    Escape,
    SearchOpen,
    SearchChanged(String),
    /// Next (+1) or previous (-1) match.
    SearchStep(i32),
    SearchClose,
    /// Matches on one page, in unrotated points on a page of `size`.
    SearchHits {
        id: u64,
        generation: u64,
        page: u32,
        size: (f64, f64),
        rects: Vec<RectPt>,
    },
    SearchDone {
        id: u64,
        generation: u64,
    },
    /// The outline and every page's links, in unrotated points.
    PdfNavigation {
        id: u64,
        outline: Vec<OutlineItem>,
        links: Vec<Links>,
    },
    /// A click on the page: on a link (page, index), or elsewhere.
    LinkClicked(Option<(u32, usize)>),
    GoTo(Dest),
    /// A text selection being dragged, in rotated points; `done` once let go.
    SelectDrag {
        from: PagePoint,
        to: PagePoint,
        done: bool,
    },
    /// Its boxes per page, in unrotated points, and its text once the drag ended.
    Selection {
        id: u64,
        generation: u64,
        regions: Regions,
        text: Option<String>,
    },
    /// Plain drags select text (on) or pan (off).
    SelectMode(bool),
    Copy,
    /// Where PDFs were left, read from disk after startup.
    Places(Box<Places>),
}

#[derive(Clone, Copy, Debug)]
pub enum PageCmd {
    Step(i32),
    First,
    Last,
    /// 0-based; clamped to the last page.
    Go(u32),
    /// That page, scrolled to this many content px from its top.
    At(u32, f64),
}

// relm4 wants Debug; payloads are pixel buffers, so print the variant only.
impl std::fmt::Debug for AppMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl AppMsg {
    fn name(&self) -> &'static str {
        match self {
            AppMsg::Request(_) => "Request",
            AppMsg::Step(_) => "Step",
            AppMsg::Hide => "Hide",
            AppMsg::OpenExternal => "OpenExternal",
            AppMsg::ToggleFullscreen => "ToggleFullscreen",
            AppMsg::Quit => "Quit",
            AppMsg::Load(_) => "Load",
            AppMsg::SpinnerDue(_) => "SpinnerDue",
            AppMsg::Thumb { .. } => "Thumb",
            AppMsg::Loaded { .. } => "Loaded",
            AppMsg::Warmup => "Warmup",
            AppMsg::Zoom(_) => "Zoom",
            AppMsg::Page(_) => "Page",
            AppMsg::DetailWanted(..) => "DetailWanted",
            AppMsg::Detail { .. } => "Detail",
            AppMsg::ZoomChanged(_) => "ZoomChanged",
            AppMsg::PdfLayout { .. } => "PdfLayout",
            AppMsg::PagesWanted { .. } => "PagesWanted",
            AppMsg::PdfPage { .. } => "PdfPage",
            AppMsg::PageShown(_) => "PageShown",
            AppMsg::Rotate(_) => "Rotate",
            AppMsg::Escape => "Escape",
            AppMsg::SearchOpen => "SearchOpen",
            AppMsg::SearchChanged(_) => "SearchChanged",
            AppMsg::SearchStep(_) => "SearchStep",
            AppMsg::SearchClose => "SearchClose",
            AppMsg::SearchHits { .. } => "SearchHits",
            AppMsg::SearchDone { .. } => "SearchDone",
            AppMsg::PdfNavigation { .. } => "PdfNavigation",
            AppMsg::LinkClicked(_) => "LinkClicked",
            AppMsg::GoTo(_) => "GoTo",
            AppMsg::SelectDrag { .. } => "SelectDrag",
            AppMsg::Selection { .. } => "Selection",
            AppMsg::SelectMode(_) => "SelectMode",
            AppMsg::Copy => "Copy",
            AppMsg::Places(_) => "Places",
        }
    }
}

pub struct App {
    title: adw::WindowTitle,
    views: Views,
    session: Session,
    paths: Vec<PathBuf>,
    index: usize,
    /// Something (thumbnail or full preview) is on screen for the current session.
    has_content: bool,
    /// The full preview is on screen; late thumbnails must not cover it.
    has_full: bool,
    /// Zoom renders within the current file; bumped by each new one and by every file
    /// change, so only the latest lands.
    generations: Session,
    kind: Option<Kind>,
    /// Kind, size and dimensions, for the subtitle.
    summary: Vec<String>,
    zoom: Option<f64>,
    /// PDF: the page on screen (0-based) of `pages`, and page 1's size in points.
    page: i32,
    pages: i32,
    page_size: (f64, f64),
    /// Continuous view: renders queued per page (at this width), and the pages still
    /// worth rendering, shared with the queued jobs.
    in_flight: HashMap<u32, i32>,
    keep: Arc<Mutex<Range<u32>>>,
    /// Widest texture (device px) on screen or asked for; `i32::MAX` once nothing
    /// sharper exists.
    detail: i32,
    toolbar: adw::ToolbarView,
    input: relm4::Sender<AppMsg>,
    /// The PDF controls, built on the first PDF.
    bar: Option<PdfBar>,
    /// PDF: clockwise degrees; kept across the reload a rotation causes.
    rotation: u16,
    /// Set by `Rotate`: the next `show_current` reloads the same file.
    reloading: bool,
    /// After a rotation: the page to return to once the layout is back.
    restore_page: Option<u32>,
    search: Option<Search>,
    /// Bumped by every new query, stopping the search before it.
    searches: Session,
    split: adw::OverlaySplitView,
    /// The outline sidebar, built for the first PDF that has one.
    outline: Option<Outline>,
    /// PDF: every page's size in points, rotated; empty until the layout arrives.
    sizes: Vec<(f64, f64)>,
    /// PDF: every page's links, in unrotated points.
    links: Vec<Links>,
    /// The selected text, once a selection drag ended.
    selected: Option<String>,
    /// Bumped by every selection step, dropping the ones before.
    selections: Session,
    /// `None` until read from disk: nothing is saved before, or it would be overwritten.
    places: Option<Places>,
    /// The zoom to return to, with `restore_page`.
    restore_zoom: Option<f64>,
}

struct Search {
    query: String,
    /// In page order: page, its unrotated size and the match, in points.
    hits: Vec<(u32, (f64, f64), RectPt)>,
    current: Option<usize>,
    done: bool,
}

impl Search {
    fn count(&self) -> String {
        match (self.hits.len(), self.current) {
            (0, _) if self.done => "No matches".into(),
            (0, _) => "Searching…".into(),
            (n, current) => format!(
                "{} of {n}{}",
                current.map_or(0, |c| c + 1),
                if self.done { "" } else { "+" }
            ),
        }
    }
}

impl Component for App {
    type Init = Init;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = ();
    type Root = adw::Window;
    type Widgets = ();

    fn init_root() -> Self::Root {
        relm4::set_global_css(include_str!("style.css"));
        // Sized here, before it is ever mapped: a later resize would override a tiling
        // compositor's tile size (ZAP §1).
        adw::Window::builder()
            .title("Preview")
            .default_width(DEFAULT_SIZE.0)
            .default_height(DEFAULT_SIZE.1)
            .width_request(320)
            .height_request(240)
            .build()
    }

    fn init(init: Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        let views = Views::new(sender.input_sender().clone());
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        // The outline slides in beside the page.
        let split = adw::OverlaySplitView::builder()
            .content(&views.stack)
            .show_sidebar(false)
            .min_sidebar_width(200.0)
            .max_sidebar_width(320.0)
            .build();
        toolbar.set_content(Some(&split));
        toolbar.set_reveal_bottom_bars(false);
        root.set_content(Some(&toolbar));

        // Never destroyed: closing hides, the next request shows it again instantly.
        let s = sender.input_sender().clone();
        root.connect_close_request(move |_| {
            s.emit(AppMsg::Hide);
            glib::Propagation::Stop
        });
        install_shortcuts(&root, &sender, &views.stack);

        let s = sender.input_sender().clone();
        std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || ipc::serve(init.listener, |req| s.emit(AppMsg::Request(req))))
            .expect("failed to spawn ipc thread");

        if let Some(req) = init.initial {
            sender.input(AppMsg::Request(req));
        }
        let s = sender.input_sender().clone();
        glib::timeout_add_local_once(WARMUP_DELAY, move || s.emit(AppMsg::Warmup));

        let model = App {
            title,
            views,
            session: Session::default(),
            paths: Vec::new(),
            index: 0,
            has_content: false,
            has_full: false,
            generations: Session::default(),
            kind: None,
            summary: Vec::new(),
            zoom: None,
            page: 0,
            pages: 0,
            page_size: (0.0, 0.0),
            in_flight: HashMap::new(),
            keep: Arc::new(Mutex::new(0..0)),
            detail: 0,
            toolbar,
            input: sender.input_sender().clone(),
            bar: None,
            rotation: 0,
            reloading: false,
            restore_page: None,
            search: None,
            searches: Session::default(),
            split,
            outline: None,
            sizes: Vec::new(),
            links: Vec::new(),
            selected: None,
            selections: Session::default(),
            places: None,
            restore_zoom: None,
        };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: AppMsg, sender: ComponentSender<Self>, root: &Self::Root) {
        crate::hit!("update");
        let started = Instant::now();
        let name = msg.name();

        match msg {
            AppMsg::Request(Request::Toggle { index, paths }) => {
                let index = index.min(paths.len().saturating_sub(1));
                if root.is_visible() && self.current() == paths.get(index).map(PathBuf::as_path) {
                    self.hide(root);
                } else if !paths.is_empty() {
                    self.paths = paths;
                    self.index = index;
                    self.show_current(&sender, root, true);
                    root.present();
                }
            }
            AppMsg::Request(Request::Hide) | AppMsg::Hide => self.hide(root),
            AppMsg::ToggleFullscreen => root.set_fullscreened(!root.is_fullscreen()),
            AppMsg::Request(Request::Quit) | AppMsg::Quit => {
                if let Some(contents) = self.save_place() {
                    places::write(&contents);
                }
                relm4::main_application().quit()
            }
            AppMsg::Step(delta) => {
                let len = self.paths.len() as i64;
                if len > 1 && root.is_visible() {
                    self.index = (self.index as i64 + delta).rem_euclid(len) as usize;
                    self.show_current(&sender, root, false);
                }
            }
            AppMsg::OpenExternal => {
                if let Some(path) = self.current() {
                    let uri = gio::File::for_path(path).uri();
                    gio::AppInfo::launch_default_for_uri_async(
                        &uri,
                        None::<&gio::AppLaunchContext>,
                        None::<&gio::Cancellable>,
                        |res| {
                            if let Err(err) = res {
                                eprintln!("[flux-preview] open failed: {err}");
                            }
                        },
                    );
                    self.hide(root);
                }
            }
            AppMsg::Load(id) => {
                if self.session.is_current(id) {
                    self.start_load(id, &sender, root);
                }
            }
            AppMsg::SpinnerDue(id) => {
                if self.session.is_current(id) && !self.has_content {
                    self.views.show_loading();
                }
            }
            AppMsg::Thumb { id, pixels } => {
                if self.session.is_current(id) && !self.has_full {
                    self.views.show_placeholder(&pixels.texture());
                    self.has_content = true;
                }
            }
            AppMsg::Loaded {
                id,
                kind,
                meta,
                payload,
            } => {
                if self.session.is_current(id) {
                    self.has_content = true;
                    self.has_full = true;
                    self.show_loaded(kind, &meta, payload, &sender);
                }
            }
            AppMsg::Warmup => {
                self.views.warm_web();
                // Pays GStreamer's registry scan off-thread, before the first video.
                crate::pool::pool().spawn(|| {
                    load::playback_available();
                });
                let s = sender.input_sender().clone();
                crate::pool::pool().spawn(move || s.emit(AppMsg::Places(Box::new(Places::load()))));
            }
            AppMsg::Places(places) => self.places = Some(*places),
            AppMsg::Zoom(cmd) => {
                if root.is_visible() {
                    self.views.zoom(cmd);
                }
            }
            AppMsg::Page(cmd) => {
                if root.is_visible() && self.kind == Some(Kind::Pdf) {
                    self.views.zoom(ZoomCmd::Page(cmd));
                }
            }
            AppMsg::Rotate(delta) => {
                if root.is_visible() && self.kind == Some(Kind::Pdf) {
                    self.rotation = (self.rotation as i32 + delta).rem_euclid(360) as u16;
                    self.restore_page = Some(self.page.max(0) as u32);
                    self.clear_selection();
                    self.reloading = true;
                    self.show_current(&sender, root, true);
                }
            }
            AppMsg::Escape => {
                if self.selected.is_some() {
                    self.clear_selection();
                } else if self.bar.as_ref().is_some_and(PdfBar::is_searching) {
                    self.end_search();
                } else {
                    self.hide(root);
                }
            }
            AppMsg::SearchOpen => {
                if let Some(bar) = self.bar.as_ref().filter(|_| self.kind == Some(Kind::Pdf)) {
                    bar.open_search();
                }
            }
            AppMsg::SearchChanged(query) => self.start_search(query, &sender),
            AppMsg::SearchStep(delta) => self.step_search(delta),
            AppMsg::SearchClose => self.end_search(),
            AppMsg::SearchHits {
                id,
                generation,
                page,
                size,
                rects,
            } => {
                if self.session.is_current(id) && self.searches.is_current(generation) {
                    self.add_hits(page, size, rects);
                }
            }
            AppMsg::SearchDone { id, generation } => {
                if self.session.is_current(id) && self.searches.is_current(generation) {
                    self.finish_search();
                }
            }
            AppMsg::PdfNavigation { id, outline, links } => {
                if self.session.is_current(id) {
                    self.links = links;
                    self.show_links();
                    self.show_outline(&outline);
                }
            }
            AppMsg::LinkClicked(Some((page, i))) => {
                let dest = self
                    .links
                    .get(page as usize)
                    .and_then(|links| links.get(i))
                    .map(|(_, dest)| dest.clone());
                if let Some(dest) = dest {
                    self.go_to(dest);
                }
            }
            AppMsg::LinkClicked(None) => self.clear_selection(),
            AppMsg::GoTo(dest) => self.go_to(dest),
            AppMsg::SelectDrag { from, to, done } => self.select(from, to, done, &sender),
            AppMsg::Selection {
                id,
                generation,
                regions,
                text,
            } => {
                if self.session.is_current(id) && self.selections.is_current(generation) {
                    self.show_selection(regions, text, root);
                }
            }
            AppMsg::SelectMode(on) => self.views.set_select_mode(on),
            AppMsg::Copy => {
                if let Some(text) = &self.selected {
                    root.clipboard().set_text(text);
                }
            }
            AppMsg::DetailWanted(w, h) => self.want_detail(w, h, &sender),
            AppMsg::Detail {
                id,
                generation,
                payload,
            } => {
                if self.session.is_current(id) && self.generations.is_current(generation) {
                    self.show_detail(payload);
                }
            }
            AppMsg::ZoomChanged(zoom) => {
                self.zoom = zoom;
                self.refresh_subtitle();
            }
            AppMsg::PdfLayout { id, sizes } => {
                if self.session.is_current(id) {
                    self.views.set_pdf_layout(&sizes);
                    self.sizes = sizes;
                    self.show_links();
                    let restore = self.restore_page.take().filter(|&p| p > 0);
                    // A match past page 1 could not be scrolled to before the layout was
                    // known; it wins over the page a rotation left.
                    if let Some(i) = self.search.as_ref().and_then(|s| s.current) {
                        self.reveal_hit(i);
                    } else if let Some(page) = restore {
                        self.views.zoom(ZoomCmd::Page(PageCmd::Go(page)));
                    }
                }
            }
            AppMsg::PagesWanted { pages, keep } => self.want_pages(pages, keep, &sender),
            AppMsg::PdfPage {
                id,
                index,
                width,
                pixels,
            } => {
                if self.session.is_current(id) {
                    if self.in_flight.get(&index) == Some(&width) {
                        self.in_flight.remove(&index);
                    }
                    if let Some(pixels) = pixels {
                        self.views.set_pdf_page_texture(index, &pixels.texture());
                    }
                }
            }
            AppMsg::PageShown(page) => {
                self.page = page as i32;
                self.refresh_subtitle();
            }
        }

        let elapsed = started.elapsed();
        if elapsed > SLOW_MSG {
            eprintln!("[slow AppMsg] {name} took {elapsed:?}");
        }
    }
}

impl App {
    fn current(&self) -> Option<&Path> {
        self.paths.get(self.index).map(PathBuf::as_path)
    }

    fn position(&self) -> Option<String> {
        (self.paths.len() > 1).then(|| format!("{} / {}", self.index + 1, self.paths.len()))
    }

    fn refresh_subtitle(&self) {
        let mut parts = self.summary.clone();
        match self.bar.as_ref().filter(|_| self.kind == Some(Kind::Pdf)) {
            // The bar shows page and zoom.
            Some(bar) => {
                bar.set_page(self.page, self.pages);
                bar.set_zoom(self.zoom);
            }
            None => {
                if let Some(zoom) = self.zoom {
                    parts.push(format!("{:.0}%", zoom * 100.0));
                }
            }
        }
        parts.extend(self.position());
        self.title.set_subtitle(&parts.join("  ·  "));
    }

    /// Show first, work after (ZAP rule 1): name and cached thumbnail now, decode later.
    fn show_current(
        &mut self,
        sender: &ComponentSender<Self>,
        root: &adw::Window,
        immediate: bool,
    ) {
        crate::hit!("show_current");
        // A rotation reloads the same file: keep its rotation and search, skip the
        // (unrotated) thumbnail.
        let reloading = std::mem::take(&mut self.reloading);
        if !reloading {
            self.save_place_async();
        }
        let id = self.session.bump();
        self.views.stop_media();
        self.reset_view_state();
        if !reloading {
            self.forget_file();
        }

        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        if !reloading && let Some(place) = self.places.as_ref().and_then(|p| p.get(&path)) {
            self.rotation = place.rotation;
            self.restore_page = Some(place.page);
            self.restore_zoom = place.zoom;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.title.set_title(&name);
        self.refresh_subtitle();

        // A thumbnail shows the page unrotated.
        if !reloading
            && self.rotation == 0
            && kind::from_extension(&path).is_none_or(Kind::has_thumbnail)
        {
            load::spawn_thumb(
                id,
                path,
                self.session.clone(),
                sender.input_sender().clone(),
            );
        }

        let s = sender.input_sender().clone();
        glib::timeout_add_local_once(SPINNER_DELAY, move || s.emit(AppMsg::SpinnerDue(id)));

        if immediate {
            self.start_load(id, sender, root);
        } else {
            // Stale timers fire too; their id no longer matches and `Load` ignores them.
            let s = sender.input_sender().clone();
            glib::timeout_add_local_once(STEP_DEBOUNCE, move || s.emit(AppMsg::Load(id)));
        }
    }

    fn start_load(&self, id: u64, sender: &ComponentSender<Self>, root: &adw::Window) {
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        let job = Job {
            id,
            hint: kind::from_extension(&path),
            path,
            target: window_target(root),
            rotation: self.rotation,
        };
        load::spawn(job, self.session.clone(), sender.input_sender().clone());
    }

    fn show_loaded(
        &mut self,
        kind: Kind,
        meta: &Meta,
        payload: Payload,
        sender: &ComponentSender<Self>,
    ) {
        crate::hit!("show_loaded");
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        self.kind = Some(kind);
        self.summary = vec![kind.label().to_owned(), format_size(meta.size)];
        let pdf = matches!(payload, Payload::Pdf { .. });
        if pdf && self.bar.is_none() {
            let bar = PdfBar::new(&self.input);
            bar.outline
                .bind_property("active", &self.split, "show-sidebar")
                .bidirectional()
                .sync_create()
                .build();
            self.toolbar.add_top_bar(&bar.search);
            self.toolbar.add_bottom_bar(&bar.bottom);
            self.bar = Some(bar);
        }
        self.toolbar.set_reveal_bottom_bars(pdf);
        if !pdf {
            self.split.set_show_sidebar(false);
        }
        match payload {
            Payload::Image {
                pixels,
                width,
                height,
            } => {
                self.summary.push(format!("{width} × {height}"));
                // Decoded whole already: zooming has nothing sharper to load.
                self.detail = if pixels.width() as u32 >= width {
                    i32::MAX
                } else {
                    pixels.width()
                };
                self.views.show_image(&pixels.texture(), (width, height));
            }
            Payload::Pdf {
                page,
                index,
                pages,
                size,
            } => {
                self.summary
                    .push(format!("{pages} page{}", if pages == 1 { "" } else { "s" }));
                self.page = index;
                self.pages = pages;
                self.page_size = size;
                self.detail = page.width();
                self.views.show_pdf_page(&page.texture(), size);
                // Where it was left; the page follows with the layout.
                if let Some(zoom) = self.restore_zoom.take() {
                    self.views.zoom(ZoomCmd::Set(zoom));
                }
                // After a rotation: the view was reset, the matches are not.
                if self.search.as_ref().is_some_and(|s| s.done) {
                    self.show_search(false);
                } else if let Some(query) = self.search.as_ref().map(|s| s.query.clone()) {
                    self.start_search(query, sender);
                }
            }
            Payload::Text { text, truncated } => self.views.show_text(&text, truncated),
            Payload::Video => self.views.show_video(&path),
            Payload::Audio(info) => self.views.show_audio(&path, &info),
            Payload::Html => self.views.show_html(&path),
            Payload::Archive(listing) => {
                self.summary.push(listing.format.clone());
                let files = listing.tree.root().files;
                self.summary.push(format!(
                    "{} file{}",
                    load::group_digits(files),
                    if files == 1 { "" } else { "s" }
                ));
                self.views.show_archive(*listing);
            }
            Payload::Info => self.views.show_info(&path, meta, None),
            Payload::Error(err) => self.views.show_info(&path, meta, Some(&err)),
        }
        self.refresh_subtitle();
    }

    /// Queues renders for the continuous view, skipping pages already rendering at that
    /// width or wider.
    fn want_pages(
        &mut self,
        pages: Vec<(u32, i32)>,
        keep: Range<u32>,
        sender: &ComponentSender<Self>,
    ) {
        if self.kind != Some(Kind::Pdf) {
            return;
        }
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        if let Ok(mut shared) = self.keep.lock() {
            *shared = keep.clone();
        }
        // Jobs for pages outside `keep` skip themselves; forget them here too.
        self.in_flight.retain(|i, _| keep.contains(i));
        for (index, width) in pages {
            if self.in_flight.get(&index).is_some_and(|&w| w >= width) {
                continue;
            }
            self.in_flight.insert(index, width);
            let job = PageJob {
                id: self.session.current(),
                path: path.clone(),
                index,
                width,
                keep: self.keep.clone(),
                rotation: self.rotation,
            };
            load::spawn_page(job, self.session.clone(), sender.input_sender().clone());
        }
    }

    fn want_detail(&mut self, w: i32, _h: i32, sender: &ComponentSender<Self>) {
        if !self.has_full || w <= self.detail {
            return;
        }
        let detail = match self.kind {
            Some(Kind::Image) => {
                self.detail = i32::MAX;
                Detail::Full
            }
            // Only the single-page view asks this way, so it is always page 1.
            Some(Kind::Pdf) if self.page_size.0 > 0.0 => {
                self.detail = w;
                Detail::Page {
                    index: 0,
                    target: Target::Scale(w as f64 / self.page_size.0),
                    rotation: self.rotation,
                }
            }
            _ => return,
        };
        self.spawn_detail(detail, sender);
    }

    fn spawn_detail(&self, detail: Detail, sender: &ComponentSender<Self>) {
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        let job = DetailJob {
            id: self.session.current(),
            generation: self.generations.bump(),
            path,
            detail,
        };
        load::spawn_detail(
            job,
            self.session.clone(),
            self.generations.clone(),
            sender.input_sender().clone(),
        );
    }

    fn show_detail(&mut self, payload: Payload) {
        match payload {
            Payload::Image { pixels, .. } => self.views.set_detail(&pixels.texture()),
            Payload::Pdf { page, .. } => self.views.set_detail(&page.texture()),
            Payload::Error(err) => eprintln!("[flux-preview] render failed: {err}"),
            _ => {}
        }
    }

    /// What survives a rotation's reload but not a change of file.
    fn forget_file(&mut self) {
        self.rotation = 0;
        self.restore_page = None;
        self.restore_zoom = None;
        self.end_search();
        self.clear_selection();
        if let Some(bar) = &self.bar {
            bar.reset_select();
            bar.outline.set_visible(false);
        }
        self.split.set_show_sidebar(false);
    }

    /// Remembers where the PDF on screen was left; returns the file to write, if changed.
    fn save_place(&mut self) -> Option<String> {
        if self.kind != Some(Kind::Pdf) || !self.has_full {
            return None;
        }
        let path = self.current()?.to_path_buf();
        let place = Place {
            page: self.page.max(0) as u32,
            zoom: self.zoom,
            rotation: self.rotation,
        };
        // As it opens anyway: nothing to remember.
        let place =
            (place.page > 0 || place.zoom.is_some() || place.rotation != 0).then_some(place);
        self.places.as_mut()?.set(&path, place)
    }

    fn save_place_async(&mut self) {
        if let Some(contents) = self.save_place() {
            crate::pool::pool().spawn(move || places::write(&contents));
        }
    }

    /// Page `page`'s size in points, rotated.
    fn rotated_size(&self, page: u32) -> (f64, f64) {
        match self.sizes.get(page as usize) {
            Some(&size) => size,
            None => self.page_size,
        }
    }

    /// An unrotated rect on page `page`, rotated for the screen.
    fn rotated(&self, page: u32, rect: RectPt) -> RectPt {
        let unrotated = load::rotate_size(self.rotated_size(page), self.rotation);
        load::rotate_rect(rect, unrotated, self.rotation)
    }

    /// A point on the screen's (rotated) page, on the unrotated page.
    fn unrotated(&self, (page, (x, y)): PagePoint) -> PagePoint {
        let back = (360 - self.rotation) % 360;
        let (x, y, _, _) = load::rotate_rect((x, y, 0.0, 0.0), self.rotated_size(page), back);
        (page, (x, y))
    }

    /// Pushes the links (rotated) to the view, once both they and the page sizes are known.
    fn show_links(&self) {
        if self.links.is_empty() || (self.pages > 1 && self.sizes.is_empty()) {
            return;
        }
        let pages = self
            .links
            .iter()
            .enumerate()
            .filter(|(_, links)| !links.is_empty())
            .map(|(page, links)| {
                let page = page as u32;
                (
                    page,
                    links
                        .iter()
                        .map(|&(rect, _)| self.rotated(page, rect))
                        .collect(),
                )
            })
            .collect();
        self.views.set_pdf_links(&pages);
    }

    fn show_outline(&mut self, items: &[OutlineItem]) {
        let Some(bar) = &self.bar else { return };
        bar.outline.set_visible(!items.is_empty());
        if items.is_empty() {
            self.split.set_show_sidebar(false);
            return;
        }
        let outline = self.outline.get_or_insert_with(|| {
            let outline = Outline::new(&self.input);
            self.split.set_sidebar(Some(&outline.root));
            outline
        });
        outline.set(items);
    }

    /// Follows a link or outline entry.
    fn go_to(&self, dest: Dest) {
        match dest {
            Dest::Page { page, y } => {
                let y = y.map(|y| self.rotated(page, (0.0, y, 0.0, 0.0)).1);
                self.views.show_pdf_at(page, y);
            }
            Dest::Uri(uri) => gio::AppInfo::launch_default_for_uri_async(
                &uri,
                None::<&gio::AppLaunchContext>,
                None::<&gio::Cancellable>,
                |res| {
                    if let Err(err) = res {
                        eprintln!("[flux-preview] open link failed: {err}");
                    }
                },
            ),
        }
    }

    fn select(
        &mut self,
        from: PagePoint,
        to: PagePoint,
        done: bool,
        sender: &ComponentSender<Self>,
    ) {
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        if self.kind != Some(Kind::Pdf) {
            return;
        }
        let job = SelectJob {
            id: self.session.current(),
            generation: self.selections.bump(),
            path,
            from: self.unrotated(from),
            to: self.unrotated(to),
            text: done,
        };
        load::spawn_select(
            job,
            self.session.clone(),
            self.selections.clone(),
            sender.input_sender().clone(),
        );
    }

    fn show_selection(&mut self, regions: Regions, text: Option<String>, root: &adw::Window) {
        let pages = regions
            .into_iter()
            .map(|(page, rects)| {
                (
                    page,
                    rects.into_iter().map(|r| self.rotated(page, r)).collect(),
                )
            })
            .collect();
        self.views.set_pdf_selection(&pages);
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            // Middle-click paste, as everywhere on Linux; Ctrl+C for the clipboard.
            root.primary_clipboard().set_text(&text);
            self.selected = Some(text);
        }
    }

    fn clear_selection(&mut self) {
        self.selections.bump();
        self.selected = None;
        self.views.set_pdf_selection(&HashMap::new());
    }

    /// A new query (debounced by the entry); empty clears the results.
    fn start_search(&mut self, query: String, sender: &ComponentSender<Self>) {
        let generation = self.searches.bump();
        let path = self.current().map(Path::to_path_buf);
        let (Some(path), Some(Kind::Pdf), false) = (path, self.kind, query.is_empty()) else {
            self.search = None;
            self.show_search(false);
            return;
        };
        self.search = Some(Search {
            query: query.clone(),
            hits: Vec::new(),
            current: None,
            done: false,
        });
        self.show_search(false);
        let job = SearchJob {
            id: self.session.current(),
            generation,
            path,
            query,
        };
        load::spawn_search(
            job,
            self.session.clone(),
            self.searches.clone(),
            sender.input_sender().clone(),
        );
    }

    fn add_hits(&mut self, page: u32, size: (f64, f64), rects: Vec<RectPt>) {
        let shown = self.page.max(0) as u32;
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let first = search.hits.len();
        search
            .hits
            .extend(rects.into_iter().map(|rect| (page, size, rect)));
        // The first match from the page on screen on.
        let reveal = search.current.is_none() && page >= shown;
        if reveal {
            search.current = Some(first);
        }
        self.show_search(reveal);
    }

    fn finish_search(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        search.done = true;
        // Nothing from the page on screen on: wrap to the first match.
        let reveal = search.current.is_none() && !search.hits.is_empty();
        if reveal {
            search.current = Some(0);
        }
        self.show_search(reveal);
    }

    fn step_search(&mut self, delta: i32) {
        let page = self.page.max(0) as u32;
        let Some(search) = self.search.as_mut().filter(|s| !s.hits.is_empty()) else {
            return;
        };
        let len = search.hits.len() as i64;
        search.current = Some(match search.current {
            Some(c) => (c as i64 + delta as i64).rem_euclid(len) as usize,
            None => search
                .hits
                .iter()
                .position(|&(p, ..)| p >= page)
                .unwrap_or(0),
        });
        self.show_search(true);
    }

    /// Pushes the matches (rotated) and the count to the screen; `reveal` also scrolls to
    /// the current one.
    fn show_search(&self, reveal: bool) {
        let mut pages: HashMap<u32, Vec<RectPt>> = HashMap::new();
        let mut current = None;
        if let Some(search) = &self.search {
            for (i, &(page, size, rect)) in search.hits.iter().enumerate() {
                let rects = pages.entry(page).or_default();
                if search.current == Some(i) {
                    current = Some((page, rects.len()));
                }
                rects.push(load::rotate_rect(rect, size, self.rotation));
            }
        }
        self.views.set_pdf_highlights(&pages, current);
        if let Some(bar) = &self.bar {
            bar.set_count(&self.search.as_ref().map(Search::count).unwrap_or_default());
        }
        if let Some(i) = self
            .search
            .as_ref()
            .and_then(|s| s.current)
            .filter(|_| reveal)
        {
            self.reveal_hit(i);
        }
    }

    fn reveal_hit(&self, i: usize) {
        if let Some(&(page, size, rect)) = self.search.as_ref().and_then(|s| s.hits.get(i)) {
            self.views
                .reveal_pdf(page, load::rotate_rect(rect, size, self.rotation));
        }
    }

    /// Drops the query, its results and the search bar.
    fn end_search(&mut self) {
        self.searches.bump();
        if self.search.take().is_some() {
            self.views.set_pdf_highlights(&HashMap::new(), None);
        }
        if let Some(bar) = &self.bar {
            bar.close_search();
        }
    }

    /// Everything that belongs to the file on screen, not the next one.
    fn reset_view_state(&mut self) {
        self.generations.bump();
        self.has_content = false;
        self.has_full = false;
        self.kind = None;
        self.summary.clear();
        self.zoom = None;
        self.page = 0;
        self.pages = 0;
        self.page_size = (0.0, 0.0);
        self.in_flight.clear();
        // A fresh range: jobs still queued for the old file see an empty one.
        self.keep = Arc::new(Mutex::new(0..0));
        self.detail = 0;
        self.sizes.clear();
        self.links.clear();
    }

    fn hide(&mut self, root: &adw::Window) {
        self.save_place_async();
        // Bumping drops everything in flight; blanking avoids flashing this file next time.
        self.session.bump();
        self.views.stop_media();
        self.views.show_blank();
        self.reset_view_state();
        self.forget_file();
        // Otherwise the next preview would open fullscreen.
        root.set_fullscreened(false);
        root.set_visible(false);
    }
}

/// Capture phase, so the text view, video controls and WebView never see these keys first.
/// The zoom and page keys only apply to images and PDFs; everywhere else they pass through,
/// so the text view still scrolls. Handlers only post messages (ZAP §5).
fn install_shortcuts(root: &adw::Window, sender: &ComponentSender<App>, stack: &gtk::Stack) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    type Binding = (&'static str, bool, fn() -> AppMsg);
    let bindings: [Binding; 22] = [
        ("space", false, || AppMsg::Hide),
        ("Escape", false, || AppMsg::Escape),
        ("<Control>f", true, || AppMsg::SearchOpen),
        ("<Control>c", true, || AppMsg::Copy),
        ("r", true, || AppMsg::Rotate(90)),
        ("<Shift>r", true, || AppMsg::Rotate(-90)),
        ("Left|h", false, || AppMsg::Step(-1)),
        ("Right|l", false, || AppMsg::Step(1)),
        ("Return", false, || AppMsg::OpenExternal),
        ("<Control>q", false, || AppMsg::Quit),
        ("plus|equal|KP_Add", true, || AppMsg::Zoom(ZoomCmd::In)),
        ("minus|KP_Subtract", true, || AppMsg::Zoom(ZoomCmd::Out)),
        ("0|KP_0", true, || AppMsg::Zoom(ZoomCmd::Fit)),
        ("1|KP_1", true, || AppMsg::Zoom(ZoomCmd::Actual)),
        ("Up|k", true, || AppMsg::Zoom(ZoomCmd::Pan(0.0, -PAN_STEP))),
        ("Down|j", true, || AppMsg::Zoom(ZoomCmd::Pan(0.0, PAN_STEP))),
        ("<Shift>Left", true, || {
            AppMsg::Zoom(ZoomCmd::Pan(-PAN_STEP, 0.0))
        }),
        ("<Shift>Right", true, || {
            AppMsg::Zoom(ZoomCmd::Pan(PAN_STEP, 0.0))
        }),
        ("Page_Up", true, || AppMsg::Page(PageCmd::Step(-1))),
        ("Page_Down", true, || AppMsg::Page(PageCmd::Step(1))),
        ("Home", true, || AppMsg::Page(PageCmd::First)),
        ("End", true, || AppMsg::Page(PageCmd::Last)),
    ];
    for (keys, image_only, msg) in bindings {
        let s = sender.input_sender().clone();
        let stack = stack.downgrade();
        // These still work while typing in the search or page entry; the rest type.
        let while_typing = matches!(keys, "Escape" | "<Control>q" | "<Control>f");
        let action = gtk::CallbackAction::new(move |widget, _| {
            let typing = widget
                .root()
                .and_then(|root| root.focus())
                .is_some_and(|focus| focus.is::<gtk::Text>());
            if typing && !while_typing {
                return glib::Propagation::Proceed;
            }
            if image_only
                && !stack
                    .upgrade()
                    .is_some_and(|stack| Views::shows_image(&stack))
            {
                return glib::Propagation::Proceed;
            }
            s.emit(msg());
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(keys),
            Some(action),
        ));
    }
    root.add_controller(controller);
}

/// The window in device px: what a fitted decode is sized to. Unmapped windows report 0.
fn window_target(root: &adw::Window) -> (i32, i32) {
    let scale = root.scale_factor().max(1);
    let (w, h) = match (root.width(), root.height()) {
        (w, h) if w > 0 && h > 0 => (w, h),
        _ => DEFAULT_SIZE,
    };
    (w * scale, h * scale)
}

pub fn run(listener: UnixListener, initial: Option<Request>) {
    // No application_id and no flags: either one triggers a synchronous D-Bus handshake and
    // Wayland compositor lookup that blocks the main thread for ~200 ms (ZAP §1). Our own
    // unix socket provides single-instance instead.
    let app = adw::Application::builder().build();
    assert!(
        app.application_id().is_none(),
        "\n\n[flux-preview] STARTUP REGRESSION: application_id is set on the adw::Application.\n\
         This adds a synchronous ~200ms D-Bus name acquisition to every launch.\n"
    );
    assert!(
        !app.flags().contains(gio::ApplicationFlags::NON_UNIQUE),
        "\n\n[flux-preview] STARTUP REGRESSION: NON_UNIQUE is set on the adw::Application.\n\
         This adds a synchronous ~200ms D-Bus handshake to every launch.\n"
    );

    let app = RelmApp::from_app(app).visible_on_activate(false);
    app.allow_multiple_instances(true);
    app.with_args(vec![]).run::<App>(Init { listener, initial });
}
