//! The preview pages, in one `gtk::Stack`. Each page is built on first use and then kept
//! and reused for every later file of that kind (ZAP §1: build rarely used UI lazily).

mod image;
mod info;
mod media;
mod text;
mod web;

use crate::load::{AudioInfo, Meta};
use gtk::gdk;
use std::path::Path;

use image::ImagePage;
use info::InfoPage;
pub use info::format_size;
use media::{AudioPage, VideoPage};
use text::TextPage;
use web::WebPage;

trait Page {
    const NAME: &'static str;
    fn build() -> Self;
    fn widget(&self) -> gtk::Widget;
}

pub struct Views {
    pub stack: gtk::Stack,
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
    pub fn new() -> Self {
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

    /// `placeholder`: a low-res cached thumbnail, stretched to fill until the real decode
    /// lands. Real images are only ever scaled down.
    pub fn show_image(&mut self, texture: &gdk::Texture, placeholder: bool) {
        page(&self.stack, &mut self.image).set(texture, placeholder);
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
