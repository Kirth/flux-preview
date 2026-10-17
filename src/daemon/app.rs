//! The previewer window: one relm4 component, kept alive (hidden) for the daemon's life.

use crate::kind::{self, Kind};
use crate::load::{self, Detail, DetailJob, Job, Meta, PageJob, Payload, Pixels, Target};
use crate::session::Session;
use crate::view::{PAN_STEP, Views, ZoomCmd, format_size};
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
}

#[derive(Clone, Copy, Debug)]
pub enum PageCmd {
    Step(i32),
    First,
    Last,
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
        toolbar.set_content(Some(&views.stack));
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
            AppMsg::Request(Request::Quit) | AppMsg::Quit => relm4::main_application().quit(),
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
                    self.show_loaded(kind, &meta, payload);
                }
            }
            AppMsg::Warmup => {
                self.views.warm_web();
                // Pays GStreamer's registry scan off-thread, before the first video.
                crate::pool::pool().spawn(|| {
                    load::playback_available();
                });
            }
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
        if self.pages > 1 {
            parts.push(format!("Page {} / {}", self.page + 1, self.pages));
        }
        if let Some(zoom) = self.zoom {
            parts.push(format!("{:.0}%", zoom * 100.0));
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
        let id = self.session.bump();
        self.views.stop_media();
        self.reset_view_state();

        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.title.set_title(&name);
        self.refresh_subtitle();

        if kind::from_extension(&path).is_none_or(Kind::has_thumbnail) {
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
        };
        load::spawn(job, self.session.clone(), sender.input_sender().clone());
    }

    fn show_loaded(&mut self, kind: Kind, meta: &Meta, payload: Payload) {
        crate::hit!("show_loaded");
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        self.kind = Some(kind);
        self.summary = vec![kind.label().to_owned(), format_size(meta.size)];
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
    }

    fn hide(&mut self, root: &adw::Window) {
        // Bumping drops everything in flight; blanking avoids flashing this file next time.
        self.session.bump();
        self.views.stop_media();
        self.views.show_blank();
        self.reset_view_state();
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
    let bindings: [Binding; 17] = [
        ("space|Escape", false, || AppMsg::Hide),
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
        let action = gtk::CallbackAction::new(move |_, _| {
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
