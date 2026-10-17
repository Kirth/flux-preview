//! The preview pages, in one `gtk::Stack`. Each page is built on first use and then kept
//! and reused for every later file of that kind (ZAP §1: build rarely used UI lazily).

mod image;
mod info;
mod media;
mod text;
mod web;
mod zoom;

use crate::app::{AppMsg, PageCmd};
use crate::load::{AudioInfo, Meta};
use gtk::gdk;
use std::path::Path;

use image::ImagePage;
use info::InfoPage;
pub use info::format_size;
use media::{AudioPage, VideoPage};
use text::TextPage;
use web::WebPage;
pub use zoom::{PAN_STEP, ZoomCmd};

trait Page {
    const NAME: &'static str;
    fn build() -> Self;
    fn widget(&self) -> gtk::Widget;
}

pub struct Views {
    pub stack: gtk::Stack,
    sender: relm4::Sender<AppMsg>,
    image: Option<ImagePage>,
    text: Option<TextPage>,
    video: Option<VideoPage>,
    audio: Option<AudioPage>,
    web: Option<WebPage>,
    info: Option<InfoPage>,
}

fn page<'a, P: Page>(stack: &gtk::Stack, slot: &'a mut Option<P>) -> &'a P {
    let page = slot.get_or_insert_with(|| {
        let page = P::build();
        stack.add_named(&page.widget(), Some(P::NAME));
        page
    });
    stack.set_visible_child_name(P::NAME);
    page
}

impl Views {
    pub fn new(sender: relm4::Sender<AppMsg>) -> Self {
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::None)
            .hexpand(true)
            .vexpand(true)
            .build();
        stack.add_named(&gtk::Box::default(), Some("blank"));
        let spinner = adw::Spinner::builder()
            .width_request(48)
            .height_request(48)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        stack.add_named(&spinner, Some("loading"));
        Self {
            stack,
            sender,
            image: None,
            text: None,
            video: None,
            audio: None,
            web: None,
            info: None,
        }
    }

    /// Builds the WebView ahead of the first HTML file: spinning up WebKit's processes is
    /// the single most expensive thing this app does.
    pub fn warm_web(&mut self) {
        if self.web.is_none() {
            let web = WebPage::build();
            self.stack.add_named(&web.widget(), Some(WebPage::NAME));
            self.web = Some(web);
        }
    }

    pub fn show_blank(&self) {
        self.stack.set_visible_child_name("blank");
    }

    pub fn show_loading(&self) {
        self.stack.set_visible_child_name("loading");
    }

    /// The image page, built (and wired to the app) on first use.
    fn image(&mut self) -> &ImagePage {
        let image = self.image.get_or_insert_with(|| {
            let page = ImagePage::build();
            let view = page.view();
            let s = self.sender.clone();
            view.connect_detail_wanted(move |w, h| s.emit(AppMsg::DetailWanted(w, h)));
            let s = self.sender.clone();
            view.connect_page_edge(move |dir| s.emit(AppMsg::Page(PageCmd::Step(dir))));
            let s = self.sender.clone();
            view.connect_zoom_changed(move |zoom| s.emit(AppMsg::ZoomChanged(zoom)));
            self.stack.add_named(&page.widget(), Some(ImagePage::NAME));
            page
        });
        self.stack.set_visible_child_name(ImagePage::NAME);
        image
    }

    /// Whether an image or PDF page is on screen (zoom and page keys apply).
    pub fn shows_image(stack: &gtk::Stack) -> bool {
        stack.visible_child_name().as_deref() == Some(ImagePage::NAME)
    }

    fn image_visible(&self) -> Option<&ImagePage> {
        self.image
            .as_ref()
            .filter(|_| Self::shows_image(&self.stack))
    }

    /// A low-res cached thumbnail, stretched to fill until the real decode lands.
    pub fn show_placeholder(&mut self, texture: &gdk::Texture) {
        self.image().set_placeholder(texture);
    }

    /// `size`: the original dimensions; `texture` is usually scaled to the window.
    pub fn show_image(&mut self, texture: &gdk::Texture, size: (u32, u32)) {
        self.image().set_image(texture, size);
    }

    pub fn show_pdf_page(
        &mut self,
        texture: &gdk::Texture,
        size_pt: (f64, f64),
        flip: Option<i32>,
    ) {
        self.image().set_pdf_page(texture, size_pt, flip);
    }

    /// Sharper pixels for the image or page on screen.
    pub fn set_detail(&self, texture: &gdk::Texture) {
        if let Some(image) = self.image_visible() {
            image.set_detail(texture);
        }
    }

    /// Keyboard zoom and pan; ignored unless an image or PDF is on screen.
    pub fn zoom(&self, cmd: ZoomCmd) {
        if let Some(image) = self.image_visible() {
            image.view().apply(cmd);
        }
    }

    /// The image page's zoom (`None`: fitted) and device scale.
    pub fn zoom_state(&self) -> (Option<f64>, f64) {
        self.image.as_ref().map_or((None, 1.0), |image| {
            (image.view().free_zoom(), image.view().device_scale())
        })
    }

    pub fn show_text(&mut self, text: &str, truncated: bool) {
        page(&self.stack, &mut self.text).set(text, truncated);
    }

    pub fn show_video(&mut self, path: &Path) {
        page(&self.stack, &mut self.video).play(path);
    }

    pub fn show_audio(&mut self, path: &Path, info: &AudioInfo) {
        page(&self.stack, &mut self.audio).play(path, info);
    }

    pub fn show_html(&mut self, path: &Path) {
        page(&self.stack, &mut self.web).load(path);
    }

    pub fn show_info(&mut self, path: &Path, meta: &Meta, error: Option<&str>) {
        page(&self.stack, &mut self.info).set(path, meta, error);
    }

    /// Stops playback and drops the pipelines; called on every file switch and on hide.
    pub fn stop_media(&self) {
        if let Some(video) = &self.video {
            video.stop();
        }
        if let Some(audio) = &self.audio {
            audio.stop();
        }
    }
}
