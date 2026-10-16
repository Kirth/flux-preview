use super::Page;
use gtk::gdk;
use gtk::prelude::*;

/// Images, PDF pages and placeholders.
pub struct ImagePage {
    picture: gtk::Picture,
}

impl Page for ImagePage {
    const NAME: &'static str = "image";

    fn build() -> Self {
        let picture = gtk::Picture::builder()
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        picture.add_css_class("preview-image");
        Self { picture }
    }

    fn widget(&self) -> gtk::Widget {
        self.picture.clone().upcast()
    }
}

impl ImagePage {
    pub fn set(&self, texture: &gdk::Texture, placeholder: bool) {
        self.picture.set_content_fit(if placeholder {
            gtk::ContentFit::Contain
        } else {
            gtk::ContentFit::ScaleDown
        });
        self.picture.set_paintable(Some(texture));
    }
}
