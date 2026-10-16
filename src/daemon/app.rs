//! The previewer window: one relm4 component, kept alive (hidden) for the daemon's life.

use crate::kind::{self, Kind};
use crate::load::{self, Job, Meta, Payload, Pixels};
use crate::session::Session;
use crate::view::{Views, format_size};
use adw::prelude::*;
use flux_preview::ipc::{self, Request};
use relm4::prelude::*;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
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
        let views = Views::new();
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
        install_shortcuts(&root, &sender);

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
                    self.views.show_image(&pixels.texture(), true);
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

    fn set_subtitle(&self, parts: &[&str]) {
        let position = self.position();
        let parts: Vec<&str> = parts.iter().copied().chain(position.as_deref()).collect();
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
        self.has_content = false;
        self.has_full = false;

        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.title.set_title(&name);
        self.set_subtitle(&[]);

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
        // Decode to the window, in physical pixels. Unmapped windows report 0.
        let scale = root.scale_factor().max(1);
        let (w, h) = match (root.width(), root.height()) {
            (w, h) if w > 0 && h > 0 => (w, h),
            _ => DEFAULT_SIZE,
        };
        let job = Job {
            id,
            hint: kind::from_extension(&path),
            path,
            target: (w * scale, h * scale),
        };
        load::spawn(job, self.session.clone(), sender.input_sender().clone());
    }

    fn show_loaded(&mut self, kind: Kind, meta: &Meta, payload: Payload) {
        crate::hit!("show_loaded");
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        let size = format_size(meta.size);
        let mut detail = None;
        match payload {
            Payload::Image {
                pixels,
                width,
                height,
            } => {
                detail = Some(format!("{width} × {height}"));
                self.views.show_image(&pixels.texture(), false);
            }
            Payload::Pdf { page, pages } => {
                detail = Some(format!("{pages} page{}", if pages == 1 { "" } else { "s" }));
                self.views.show_image(&page.texture(), false);
            }
            Payload::Text { text, truncated } => self.views.show_text(&text, truncated),
            Payload::Video => self.views.show_video(&path),
            Payload::Audio(info) => self.views.show_audio(&path, &info),
            Payload::Html => self.views.show_html(&path),
            Payload::Info => self.views.show_info(&path, meta, None),
            Payload::Error(err) => self.views.show_info(&path, meta, Some(&err)),
        }
        let mut parts = vec![kind.label(), size.as_str()];
        parts.extend(detail.as_deref());
        self.set_subtitle(&parts);
    }

    fn hide(&mut self, root: &adw::Window) {
        // Bumping drops everything in flight; blanking avoids flashing this file next time.
        self.session.bump();
        self.views.stop_media();
        self.views.show_blank();
        self.has_content = false;
        self.has_full = false;
        root.set_visible(false);
    }
}

/// Capture phase, so the text view, video controls and WebView never see these keys first.
/// Handlers only post messages (ZAP §5).
fn install_shortcuts(root: &adw::Window, sender: &ComponentSender<App>) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    type Binding = (&'static str, fn() -> AppMsg);
    let bindings: [Binding; 5] = [
        ("space|Escape", || AppMsg::Hide),
        ("Left|h", || AppMsg::Step(-1)),
        ("Right|l", || AppMsg::Step(1)),
        ("Return", || AppMsg::OpenExternal),
        ("<Control>q", || AppMsg::Quit),
    ];
    for (keys, msg) in bindings {
        let s = sender.input_sender().clone();
        let action = gtk::CallbackAction::new(move |_, _| {
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
