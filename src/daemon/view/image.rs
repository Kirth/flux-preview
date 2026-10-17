use super::Page;
use super::zoom::{FitMode, PagePoint, Pages, Rect, ZoomView};
use crate::app::PageCmd;
use gtk::gdk;
use gtk::prelude::*;
use std::collections::HashMap;

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

    /// Page 1 of a PDF, fitted to the window; the rest follows via `set_pdf_layout`.
    pub fn set_pdf_page(&self, texture: &gdk::Texture, size_pt: (f64, f64)) {
        let size = (size_pt.0 * PX_PER_PT, size_pt.1 * PX_PER_PT);
        self.view.set(texture, size, FitMode::Contain, true, false);
    }

    /// Every page's size in points: what the continuous view needs.
    pub fn set_pdf_layout(&self, sizes_pt: &[(f64, f64)]) {
        let sizes = sizes_pt
            .iter()
            .map(|&(w, h)| (w * PX_PER_PT, h * PX_PER_PT))
            .collect();
        self.view.set_layout(Pages::new(sizes));
    }

    pub fn set_pdf_page_texture(&self, index: u32, texture: &gdk::Texture) {
        self.view.set_page_texture(index, texture);
    }

    /// Search matches in points, per page, already rotated.
    pub fn set_pdf_highlights(
        &self,
        pages: &HashMap<u32, Vec<Rect>>,
        current: Option<(u32, usize)>,
    ) {
        self.view.set_highlights(to_px(pages), current);
    }

    /// Selected text as boxes per page, in points, already rotated.
    pub fn set_pdf_selection(&self, pages: &HashMap<u32, Vec<Rect>>) {
        self.view.set_selection(to_px(pages));
    }

    /// Link areas per page, in points, already rotated.
    pub fn set_pdf_links(&self, pages: &HashMap<u32, Vec<Rect>>) {
        self.view.set_links(to_px(pages));
    }

    /// Page `page`, scrolled to `y` points from its top when given.
    pub fn show_pdf_at(&self, page: u32, y: Option<f64>) {
        let cmd = match y {
            Some(y) => PageCmd::At(page, y * PX_PER_PT),
            None => PageCmd::Go(page),
        };
        self.view.apply(super::ZoomCmd::Page(cmd));
    }

    /// Called with both ends of a dragged text selection, in points.
    pub fn connect_pdf_select(&self, f: impl Fn(PagePoint, PagePoint, bool) + 'static) {
        let pt = |(page, (x, y)): PagePoint| (page, (x / PX_PER_PT, y / PX_PER_PT));
        self.view
            .connect_select(move |from, to, done| f(pt(from), pt(to), done));
    }

    /// Scrolls to a search match, in points.
    pub fn reveal_pdf(&self, page: u32, (x, y, w, h): Rect) {
        self.view.reveal(
            page,
            (x * PX_PER_PT, y * PX_PER_PT, w * PX_PER_PT, h * PX_PER_PT),
        );
    }

    pub fn set_detail(&self, texture: &gdk::Texture) {
        self.view.set_texture(texture);
    }
}

fn to_px(pages: &HashMap<u32, Vec<Rect>>) -> HashMap<u32, Vec<Rect>> {
    let px = |&(x, y, w, h): &Rect| (x * PX_PER_PT, y * PX_PER_PT, w * PX_PER_PT, h * PX_PER_PT);
    pages
        .iter()
        .map(|(&page, rects)| (page, rects.iter().map(px).collect()))
        .collect()
}
