use super::Page;
use gtk::prelude::*;

pub struct TextPage {
    root: gtk::Box,
    banner: adw::Banner,
    view: gtk::TextView,
}

impl Page for TextPage {
    const NAME: &'static str = "text";

    fn build() -> Self {
        let banner = adw::Banner::new(&format!(
            "Showing the first {} KiB",
            crate::load::TEXT_CAP / 1024
        ));
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&banner);
        root.append(&scroller);
        Self { root, banner, view }
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}

impl TextPage {
    pub fn set(&self, text: &str, truncated: bool) {
        crate::hit!("set");
        self.banner.set_revealed(truncated);
        self.view.buffer().set_text(text);
    }
}
