use super::Page;
use super::zoom::{FitMode, ZoomView};
use gtk::gdk;
use gtk::prelude::*;

/// 100% for a PDF: 96 logical px per inch.
const PX_PER_PT: f64 = 96.0 / 72.0;

/// Images, PDF pages and placeholders, zoomable.
pub struct ImagePage {
    view: ZoomView,
}

impl Page for ImagePage {
    const NAME: &'static str = "image";

    fn build() -> Self {
        let view = ZoomView::default();
        view.set_hexpand(true);
        view.set_vexpand(true);
        view.add_css_class("preview-image");
        Self { view }
    }

    fn widget(&self) -> gtk::Widget {
        self.view.clone().upcast()
    }
}

impl ImagePage {
    pub fn view(&self) -> &ZoomView {
        &self.view
    }

    /// A low-res cached thumbnail, stretched to fill until the real decode lands.
    pub fn set_placeholder(&self, texture: &gdk::Texture) {
        let size = (texture.width() as f64, texture.height() as f64);
        self.view.set(texture, size, FitMode::Contain, false, true);
    }

    /// `size`: the original pixels, of which `texture` may hold fewer. 100% is one image
    /// pixel per device pixel; fitting only ever scales down.
    pub fn set_image(&self, texture: &gdk::Texture, (w, h): (u32, u32)) {
        let d = self.view.device_scale();
        let size = (w as f64 / d, h as f64 / d);
        self.view
            .set(texture, size, FitMode::ScaleDown, false, false);
    }

    /// `flip`: the direction we paged in from, to keep the zoom across pages; `None` for a
    /// newly opened document.
    pub fn set_pdf_page(&self, texture: &gdk::Texture, (w, h): (f64, f64), flip: Option<i32>) {
        let size = (w * PX_PER_PT, h * PX_PER_PT);
        match flip {
            Some(dir) => self.view.set_page(texture, size, dir),
            None => self.view.set(texture, size, FitMode::Contain, true, false),
        }
    }

    pub fn set_detail(&self, texture: &gdk::Texture) {
        self.view.set_texture(texture);
    }
}
