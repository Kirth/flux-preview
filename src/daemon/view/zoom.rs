//! A texture you can zoom and pan: the canvas of the image page.
//!
//! Drawn in `snapshot()` with a single transform instead of resizing a child inside a
//! ScrolledWindow, so zooming never waits for a layout pass and the point under the
//! cursor stays exactly put. The texture is always stretched over the content rect, so a
//! sharper decode can be swapped in without the view moving.
//!
//! PDFs open as a single page (fast); the first scroll down turns the same canvas into a
//! continuous document: the pages stacked vertically, each rendered only near the view.

use crate::app::PageCmd;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

const MAX_ZOOM: f64 = 32.0;
/// One wheel notch or key press.
const STEP: f64 = 1.25;
/// Keyboard pan, in screen px.
pub const PAN_STEP: f64 = 64.0;
const WHEEL_PAN: f64 = 48.0;
/// Between pages of a document, in content px.
const PAGE_GAP: f64 = 16.0;
/// Document mode: how often the visible pages are checked while scrolling.
const PAGES_THROTTLE: Duration = Duration::from_millis(50);
/// Pages kept rendered on either side of what is visible.
const KEEP_AROUND: u32 = 3;
/// Only ask for sharper pixels once zooming pauses this long (ZAP §3).
const DETAIL_DELAY: Duration = Duration::from_millis(150);
/// Ask for sharper pixels once the screen shows this many device px per texel.
const DETAIL_SLACK: f64 = 1.25;
/// Zoomed this far in, show hard pixel edges instead of a blur.
const NEAREST_FROM: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zoom {
    Fit,
    Free(f64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FitMode {
    /// Images: never upscaled to fit.
    ScaleDown,
    /// PDF pages and placeholders: always fill the window.
    Contain,
    /// Documents: one page of this size fills the window, not the whole stack.
    Page(f64, f64),
}

#[derive(Clone, Copy, Debug)]
pub enum ZoomCmd {
    In,
    Out,
    Fit,
    /// The widest page (or the image) spans the view.
    FitWidth,
    Actual,
    /// This zoom level; 1.0 is 100%.
    Set(f64),
    /// Move the view by screen px.
    Pan(f64, f64),
    /// PDFs: jump between pages (switching to the continuous view).
    Page(PageCmd),
}

/// The zoom/pan math, free of GTK so it can be tested.
///
/// Content coordinates are logical px at 100%; `center` is the content point shown in
/// the middle of the view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub view: (f64, f64),
    pub content: (f64, f64),
    pub fit: FitMode,
    pub zoom: Zoom,
    pub center: (f64, f64),
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new((1.0, 1.0), FitMode::ScaleDown)
    }
}

impl Viewport {
    pub fn new(content: (f64, f64), fit: FitMode) -> Self {
        let content = (content.0.max(1.0), content.1.max(1.0));
        Self {
            view: (1.0, 1.0),
            content,
            fit,
            zoom: Zoom::Fit,
            center: (content.0 / 2.0, content.1 / 2.0),
        }
    }

    pub fn fit_scale(&self) -> f64 {
        let fit = |(w, h): (f64, f64)| (self.view.0 / w).min(self.view.1 / h);
        match self.fit {
            FitMode::ScaleDown => fit(self.content).min(1.0),
            FitMode::Contain => fit(self.content),
            FitMode::Page(w, h) => fit((w.max(1.0), h.max(1.0))),
        }
    }

    /// Documents may zoom out to several pages a screen; everything else stops at fit.
    fn min_scale(&self) -> f64 {
        match self.fit {
            FitMode::Page(..) => self.fit_scale() / 4.0,
            _ => self.fit_scale().min(1.0),
        }
    }

    pub fn scale(&self) -> f64 {
        match self.zoom {
            Zoom::Fit => self.fit_scale(),
            Zoom::Free(z) => z,
        }
    }

    /// Top-left corner of the content, in view coordinates.
    pub fn origin(&self) -> (f64, f64) {
        let s = self.scale();
        (
            self.view.0 / 2.0 - self.center.0 * s,
            self.view.1 / 2.0 - self.center.1 * s,
        )
    }

    pub fn set_view(&mut self, view: (f64, f64)) {
        self.view = (view.0.max(1.0), view.1.max(1.0));
        self.clamp();
    }

    /// Zooms to `scale`, keeping the content point under `anchor` (view coordinates) put.
    /// Landing on the fit scale switches back to `Fit`.
    pub fn zoom_to(&mut self, scale: f64, anchor: (f64, f64)) {
        let old = self.scale();
        let fit = self.fit_scale();
        let new = scale.clamp(self.min_scale(), MAX_ZOOM);
        let a = (anchor.0 - self.view.0 / 2.0, anchor.1 - self.view.1 / 2.0);
        let p = (self.center.0 + a.0 / old, self.center.1 + a.1 / old);
        self.center = (p.0 - a.0 / new, p.1 - a.1 / new);
        self.zoom = if (new - fit).abs() < 1e-3 {
            Zoom::Fit
        } else {
            Zoom::Free(new)
        };
        self.clamp();
    }

    pub fn zoom_by(&mut self, factor: f64, anchor: (f64, f64)) {
        self.zoom_to(self.scale() * factor, anchor);
    }

    pub fn fit_width(&mut self) {
        let middle = (self.view.0 / 2.0, self.view.1 / 2.0);
        self.zoom_to(self.view.0 / self.content.0, middle);
    }

    pub fn fit(&mut self) {
        self.zoom = Zoom::Fit;
        self.clamp();
    }

    /// Moves the view by `d` screen px; returns how far it actually moved.
    pub fn pan(&mut self, d: (f64, f64)) -> (f64, f64) {
        let s = self.scale();
        let before = self.center;
        self.center.0 += d.0 / s;
        self.center.1 += d.1 / s;
        self.clamp();
        (
            (self.center.0 - before.0) * s,
            (self.center.1 - before.1) * s,
        )
    }

    fn clamp(&mut self) {
        let s = self.scale();
        self.center.0 = clamp_axis(self.center.0, self.content.0, self.view.0 / s);
        self.center.1 = clamp_axis(self.center.1, self.content.1, self.view.1 / s);
    }
}

/// Content smaller than the view is centered; larger content may not leave a gap.
fn clamp_axis(center: f64, content: f64, visible: f64) -> f64 {
    if visible >= content {
        content / 2.0
    } else {
        center.clamp(visible / 2.0, content - visible / 2.0)
    }
}

/// A document's pages stacked top to bottom, centered horizontally, in content px.
#[derive(Clone, Debug, PartialEq)]
pub struct Pages {
    sizes: Vec<(f64, f64)>,
    tops: Vec<f64>,
    width: f64,
    height: f64,
    largest: (f64, f64),
}

impl Pages {
    pub fn new(sizes: Vec<(f64, f64)>) -> Self {
        let mut tops = Vec::with_capacity(sizes.len());
        let mut y = 0.0;
        let (mut width, mut largest_h) = (1.0f64, 1.0f64);
        for &(w, h) in &sizes {
            tops.push(y);
            y += h + PAGE_GAP;
            width = width.max(w);
            largest_h = largest_h.max(h);
        }
        Self {
            sizes,
            tops,
            width,
            height: (y - PAGE_GAP).max(1.0),
            largest: (width, largest_h),
        }
    }

    pub fn len(&self) -> u32 {
        self.sizes.len() as u32
    }

    pub fn size(&self) -> (f64, f64) {
        (self.width, self.height)
    }

    /// `(x, y, w, h)` of page `i`.
    pub fn rect(&self, i: u32) -> (f64, f64, f64, f64) {
        let (w, h) = self.sizes[i as usize];
        ((self.width - w) / 2.0, self.tops[i as usize], w, h)
    }

    /// The page at height `y`; a gap belongs to the page above it.
    pub fn page_at(&self, y: f64) -> u32 {
        let after = self.tops.partition_point(|&top| top <= y);
        after
            .saturating_sub(1)
            .min(self.sizes.len().saturating_sub(1)) as u32
    }

    /// The pages overlapping `y0..y1`.
    pub fn range(&self, y0: f64, y1: f64) -> Range<u32> {
        self.page_at(y0)..self.page_at(y1) + 1
    }
}

/// Continuous-view state: the layout plus whichever pages are rendered.
pub struct Doc {
    pages: Pages,
    textures: HashMap<u32, gdk::Texture>,
    /// Widest render asked for per page, so one isn't asked for twice.
    asked: HashMap<u32, i32>,
    current: u32,
    keep: Range<u32>,
}

type DetailFn = Rc<dyn Fn(i32, i32)>;
type PagesFn = Rc<dyn Fn(Vec<(u32, i32)>, Range<u32>)>;
type PageFn = Rc<dyn Fn(u32)>;
type ZoomFn = Rc<dyn Fn(Option<f64>)>;
type LinkFn = Rc<dyn Fn(Option<(u32, usize)>)>;
type SelectFn = Rc<dyn Fn(PagePoint, PagePoint, bool)>;

/// A point on a page: its index and content px from its top-left corner.
pub type PagePoint = (u32, (f64, f64));

/// `(x, y, w, h)` in content px, relative to its page's top-left corner.
pub type Rect = (f64, f64, f64, f64);

const HIGHLIGHT: gdk::RGBA = gdk::RGBA::new(1.0, 0.85, 0.0, 0.35);
const HIGHLIGHT_CURRENT: gdk::RGBA = gdk::RGBA::new(1.0, 0.45, 0.0, 0.55);
const SELECTION: gdk::RGBA = gdk::RGBA::new(0.21, 0.52, 0.89, 0.35);

/// Linear, mipmapped when shrinking a lot, hard pixel edges when zoomed far in.
fn filter_for(per_texel: f64, smooth: bool) -> gsk::ScalingFilter {
    if smooth || per_texel < NEAREST_FROM {
        if per_texel < 0.5 {
            // Mipmapped: big photos shrunk to the window stay smooth.
            gsk::ScalingFilter::Trilinear
        } else {
            gsk::ScalingFilter::Linear
        }
    } else {
        gsk::ScalingFilter::Nearest
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct State {
        pub vp: Viewport,
        pub texture: Option<gdk::Texture>,
        pub placeholder: bool,
        /// PDF: scrolling pans; scrolling down turns into the continuous view.
        pub paged: bool,
        /// The PDF's page layout, once known; needed to go continuous.
        pub layout: Option<Pages>,
        pub doc: Option<Doc>,
        pub pointer: Option<(f64, f64)>,
        /// `center` when the drag started.
        pub drag_from: Option<(f64, f64)>,
        /// Scale when the pinch started.
        pub pinch_from: Option<f64>,
        /// Search matches per page, and the selected one as (page, index).
        pub highlights: HashMap<u32, Vec<Rect>>,
        pub current_hit: Option<(u32, usize)>,
        /// Selected text, as boxes per page.
        pub selection: HashMap<u32, Vec<Rect>>,
        /// Clickable areas per page.
        pub links: HashMap<u32, Vec<Rect>>,
        /// Plain drags select text instead of panning.
        pub select_mode: bool,
        /// Where the text selection being dragged started.
        pub select_from: Option<PagePoint>,
    }

    #[derive(Default)]
    pub struct ZoomView {
        pub state: RefCell<State>,
        pub detail_timer: RefCell<Option<glib::SourceId>>,
        pub pages_timer: RefCell<Option<glib::SourceId>>,
        pub on_detail: RefCell<Option<DetailFn>>,
        pub on_pages: RefCell<Option<PagesFn>>,
        pub on_page: RefCell<Option<PageFn>>,
        pub on_zoom: RefCell<Option<ZoomFn>>,
        pub on_link: RefCell<Option<LinkFn>>,
        pub on_select: RefCell<Option<SelectFn>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZoomView {
        const NAME: &'static str = "FluxPreviewZoomView";
        type Type = super::ZoomView;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ZoomView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.set_focusable(false);
            obj.install_controllers();
        }
    }

    impl WidgetImpl for ZoomView {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            self.state
                .borrow_mut()
                .vp
                .set_view((width as f64, height as f64));
            // A bigger window can need a sharper texture too.
            self.obj().schedule_detail();
            self.obj().schedule_pages();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let st = self.state.borrow();
            let s = st.vp.scale();
            let (ox, oy) = st.vp.origin();
            let device = self.obj().device_scale();
            let rect = |x: f64, y: f64, w: f64, h: f64| {
                graphene::Rect::new(
                    (ox + x * s) as f32,
                    (oy + y * s) as f32,
                    (w * s) as f32,
                    (h * s) as f32,
                )
            };

            let highlights = |page: u32, px: f64, py: f64| {
                for &(x, y, w, h) in st.selection.get(&page).into_iter().flatten() {
                    snapshot.append_color(&SELECTION, &rect(px + x, py + y, w, h));
                }
                for (n, &(x, y, w, h)) in st.highlights.get(&page).into_iter().flatten().enumerate()
                {
                    let color = if st.current_hit == Some((page, n)) {
                        &HIGHLIGHT_CURRENT
                    } else {
                        &HIGHLIGHT
                    };
                    snapshot.append_color(color, &rect(px + x, py + y, w, h));
                }
            };

            if let Some(doc) = &st.doc {
                let visible = doc.pages.range(-oy / s, (st.vp.view.1 - oy) / s);
                for i in visible {
                    let (x, y, w, h) = doc.pages.rect(i);
                    match doc.textures.get(&i) {
                        Some(texture) => {
                            let per_texel = w * s * device / texture.width().max(1) as f64;
                            snapshot.append_scaled_texture(
                                texture,
                                filter_for(per_texel, false),
                                &rect(x, y, w, h),
                            );
                        }
                        // Not rendered yet: a blank page of the right size, so nothing moves.
                        None => snapshot.append_color(&gdk::RGBA::WHITE, &rect(x, y, w, h)),
                    }
                    highlights(i, x, y);
                }
                return;
            }

            let Some(texture) = &st.texture else {
                return;
            };
            let (w, h) = st.vp.content;
            let per_texel = w * s * device / texture.width().max(1) as f64;
            snapshot.append_scaled_texture(
                texture,
                filter_for(per_texel, st.placeholder),
                &rect(0.0, 0.0, w, h),
            );
            if st.paged {
                highlights(0, 0.0, 0.0);
            }
        }
    }
}

glib::wrapper! {
    pub struct ZoomView(ObjectSubclass<imp::ZoomView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ZoomView {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ZoomView {
    /// A new file: back to fit.
    pub fn set(
        &self,
        texture: &gdk::Texture,
        content: (f64, f64),
        fit: FitMode,
        paged: bool,
        placeholder: bool,
    ) {
        {
            let mut st = self.imp().state.borrow_mut();
            let view = st.vp.view;
            st.vp = Viewport::new(content, fit);
            st.vp.set_view(view);
            st.texture = Some(texture.clone());
            st.placeholder = placeholder;
            st.paged = paged;
            st.layout = None;
            st.doc = None;
            st.drag_from = None;
            st.pinch_from = None;
            st.highlights.clear();
            st.current_hit = None;
            st.selection.clear();
            st.links.clear();
            st.select_from = None;
        }
        self.queue_draw();
        if !placeholder {
            self.schedule_detail();
        }
    }

    /// The PDF's page sizes (content px), once known. Kept until the first scroll down
    /// or page key turns the single page into the continuous view.
    pub fn set_layout(&self, pages: Pages) {
        let mut st = self.imp().state.borrow_mut();
        if st.paged && st.doc.is_none() {
            st.layout = Some(pages);
        }
    }

    /// Sharper pixels for what is on screen; the view does not move.
    pub fn set_texture(&self, texture: &gdk::Texture) {
        let in_doc = self.imp().state.borrow().doc.is_some();
        if in_doc {
            // A sharper page 1 asked for before going continuous.
            self.set_page_texture(0, texture);
        } else {
            self.imp().state.borrow_mut().texture = Some(texture.clone());
            self.queue_draw();
        }
    }

    /// A rendered page for the continuous view; dropped if it scrolled out of reach or
    /// is no sharper than what is there.
    pub fn set_page_texture(&self, index: u32, texture: &gdk::Texture) {
        {
            let mut st = self.imp().state.borrow_mut();
            let Some(doc) = st.doc.as_mut().filter(|d| d.keep.contains(&index)) else {
                return;
            };
            if doc
                .textures
                .get(&index)
                .is_some_and(|t| t.width() >= texture.width())
            {
                return;
            }
            doc.textures.insert(index, texture.clone());
        }
        self.queue_draw();
    }

    pub fn apply(&self, cmd: ZoomCmd) {
        if !self.interactive() {
            return;
        }
        if let ZoomCmd::Page(cmd) = cmd {
            self.page(cmd);
            return;
        }
        let mut st = self.imp().state.borrow_mut();
        let middle = (st.vp.view.0 / 2.0, st.vp.view.1 / 2.0);
        match cmd {
            ZoomCmd::In => st.vp.zoom_by(STEP, middle),
            ZoomCmd::Out => st.vp.zoom_by(1.0 / STEP, middle),
            ZoomCmd::Fit => st.vp.fit(),
            ZoomCmd::FitWidth => st.vp.fit_width(),
            ZoomCmd::Actual => st.vp.zoom_to(1.0, middle),
            ZoomCmd::Set(zoom) => st.vp.zoom_to(zoom, middle),
            ZoomCmd::Pan(dx, dy) => {
                st.vp.pan((dx, dy));
                drop(st);
                self.moved();
                return;
            }
            ZoomCmd::Page(_) => unreachable!(),
        }
        drop(st);
        self.zoomed();
    }

    /// Page keys: go continuous if needed, then jump.
    fn page(&self, cmd: PageCmd) {
        if !self.enter_doc() {
            return;
        }
        {
            let mut st = self.imp().state.borrow_mut();
            let st = &mut *st;
            let Some(doc) = st.doc.as_ref() else { return };
            let last = doc.pages.len() - 1;
            let target = match cmd {
                PageCmd::Step(delta) => {
                    (doc.current as i64 + delta as i64).clamp(0, last as i64) as u32
                }
                PageCmd::First => 0,
                PageCmd::Last => last,
                PageCmd::Go(page) | PageCmd::At(page, _) => page.min(last),
            };
            // Centered if the page fits the window, else its top at the top.
            let (_, y, _, h) = doc.pages.rect(target);
            let visible = st.vp.view.1 / st.vp.scale();
            st.vp.center.1 = if let PageCmd::At(_, at) = cmd {
                // That spot at the top of the view, with a little room above.
                y + (at - PAGE_GAP).max(0.0) + visible / 2.0
            } else if h <= visible {
                y + h / 2.0
            } else {
                y + visible / 2.0
            };
            st.vp.clamp();
        }
        self.moved();
    }

    /// Search matches to draw over the pages, replacing the previous ones.
    pub fn set_highlights(
        &self,
        highlights: HashMap<u32, Vec<Rect>>,
        current: Option<(u32, usize)>,
    ) {
        {
            let mut st = self.imp().state.borrow_mut();
            st.highlights = highlights;
            st.current_hit = current;
        }
        self.queue_draw();
    }

    /// The selected text's boxes per page (content px), replacing the previous ones.
    pub fn set_selection(&self, selection: HashMap<u32, Vec<Rect>>) {
        self.imp().state.borrow_mut().selection = selection;
        self.queue_draw();
    }

    /// The link areas per page (content px); clicking one calls `connect_link`.
    pub fn set_links(&self, links: HashMap<u32, Vec<Rect>>) {
        self.imp().state.borrow_mut().links = links;
    }

    pub fn set_select_mode(&self, on: bool) {
        self.imp().state.borrow_mut().select_mode = on;
        self.set_cursor_from_name(on.then_some("text"));
    }

    /// The page under view point `at`, and where on it (content px, clamped to the page).
    fn page_point(&self, (x, y): (f64, f64)) -> Option<PagePoint> {
        let st = self.imp().state.borrow();
        if !st.paged {
            return None;
        }
        let s = st.vp.scale();
        let (ox, oy) = st.vp.origin();
        let (cx, cy) = ((x - ox) / s, (y - oy) / s);
        let (page, (px, py, w, h)) = match &st.doc {
            Some(doc) => {
                let page = doc.pages.page_at(cy);
                (page, doc.pages.rect(page))
            }
            None => (0, (0.0, 0.0, st.vp.content.0, st.vp.content.1)),
        };
        Some((page, ((cx - px).clamp(0.0, w), (cy - py).clamp(0.0, h))))
    }

    /// The link under view point `at`: its page and index.
    fn link_at(&self, at: (f64, f64)) -> Option<(u32, usize)> {
        let (page, (x, y)) = self.page_point(at)?;
        let st = self.imp().state.borrow();
        let i = st
            .links
            .get(&page)?
            .iter()
            .position(|&(lx, ly, lw, lh)| x >= lx && x <= lx + lw && y >= ly && y <= ly + lh)?;
        Some((page, i))
    }

    /// Called with the link clicked, or `None` for a click elsewhere on the page.
    pub fn connect_link(&self, f: impl Fn(Option<(u32, usize)>) + 'static) {
        *self.imp().on_link.borrow_mut() = Some(Rc::new(f));
    }

    /// Called while a text selection is dragged (`done` false) and when it is let go.
    pub fn connect_select(&self, f: impl Fn(PagePoint, PagePoint, bool) + 'static) {
        *self.imp().on_select.borrow_mut() = Some(Rc::new(f));
    }

    fn emit_select(&self, at: (f64, f64), done: bool) {
        let from = self.imp().state.borrow().select_from;
        let on_select = self.imp().on_select.borrow().clone();
        if let (Some(from), Some(to), Some(f)) = (from, self.page_point(at), on_select) {
            f(from, to, done);
        }
    }

    /// Scrolls `rect` on page `page` into view, centering it unless it is already fully
    /// on screen. Goes continuous for any page but the first.
    pub fn reveal(&self, page: u32, (x, y, w, h): Rect) {
        if !self.interactive() {
            return;
        }
        if page > 0 && !self.enter_doc() {
            return;
        }
        {
            let mut st = self.imp().state.borrow_mut();
            let st = &mut *st;
            let (px, py) = match &st.doc {
                Some(doc) if page < doc.pages.len() => {
                    let (px, py, _, _) = doc.pages.rect(page);
                    (px, py)
                }
                Some(_) => return,
                None => (0.0, 0.0),
            };
            let s = st.vp.scale();
            let (hw, hh) = (st.vp.view.0 / 2.0 / s, st.vp.view.1 / 2.0 / s);
            let (cx, cy) = st.vp.center;
            let (x, y) = (px + x, py + y);
            let inside = x >= cx - hw && x + w <= cx + hw && y >= cy - hh && y + h <= cy + hh;
            if !inside {
                st.vp.center = (x + w / 2.0, y + h / 2.0);
                st.vp.clamp();
            }
        }
        self.moved();
    }

    /// Turns the single PDF page into the continuous view, without moving it: page 1
    /// keeps its place, scale and texture. `false` when there is nothing to continue to.
    fn enter_doc(&self) -> bool {
        {
            let mut st = self.imp().state.borrow_mut();
            if st.doc.is_some() {
                return true;
            }
            if !st.paged || st.layout.as_ref().is_none_or(|p| p.len() < 2) {
                return false;
            }
            let pages = st.layout.take().expect("checked above");
            let (w, h) = pages.size();
            let (lw, lh) = pages.largest;
            let (view, zoom, center_y) = (st.vp.view, st.vp.zoom, st.vp.center.1);
            st.vp = Viewport::new((w, h), FitMode::Page(lw, lh));
            st.vp.set_view(view);
            st.vp.zoom = zoom;
            st.vp.center = (w / 2.0, center_y);
            st.vp.clamp();
            let mut textures = HashMap::new();
            if let Some(texture) = st.texture.clone() {
                textures.insert(0, texture);
            }
            let current = pages.page_at(st.vp.center.1);
            st.doc = Some(Doc {
                pages,
                textures,
                asked: HashMap::new(),
                current,
                keep: 0..0,
            });
        }
        crate::hit!("enter_doc");
        self.moved();
        true
    }

    /// The zoom level, or `None` when fitted to the window.
    pub fn free_zoom(&self) -> Option<f64> {
        match self.imp().state.borrow().vp.zoom {
            Zoom::Fit => None,
            Zoom::Free(z) => Some(z),
        }
    }

    /// Device px per logical px, fractional scaling included.
    pub fn device_scale(&self) -> f64 {
        self.native()
            .and_then(|native| native.surface())
            .map(|surface| surface.scale())
            .filter(|s| *s > 0.0)
            .unwrap_or_else(|| self.scale_factor() as f64)
    }

    /// Called (debounced) with the device px the content now covers, when the texture has
    /// noticeably fewer.
    pub fn connect_detail_wanted(&self, f: impl Fn(i32, i32) + 'static) {
        *self.imp().on_detail.borrow_mut() = Some(Rc::new(f));
    }

    /// Continuous view: called (throttled) with the pages to render at which width in
    /// device px, and the range worth keeping; anything outside it may be skipped.
    pub fn connect_pages_wanted(&self, f: impl Fn(Vec<(u32, i32)>, Range<u32>) + 'static) {
        *self.imp().on_pages.borrow_mut() = Some(Rc::new(f));
    }

    /// Continuous view: called with the page in the middle of the view when it changes.
    pub fn connect_page_shown(&self, f: impl Fn(u32) + 'static) {
        *self.imp().on_page.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_zoom_changed(&self, f: impl Fn(Option<f64>) + 'static) {
        *self.imp().on_zoom.borrow_mut() = Some(Rc::new(f));
    }

    /// The view moved: redraw, and see which pages it now needs.
    fn moved(&self) {
        self.queue_draw();
        self.schedule_pages();
    }

    fn zoomed(&self) {
        self.moved();
        let zoom = self.free_zoom();
        let on_zoom = self.imp().on_zoom.borrow().clone();
        if let Some(f) = on_zoom {
            f(zoom);
        }
        self.schedule_detail();
    }

    fn schedule_detail(&self) {
        let imp = self.imp();
        if let Some(id) = imp.detail_timer.take() {
            id.remove();
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(DETAIL_DELAY, move || {
            if let Some(view) = weak.upgrade() {
                // Already fired: removing it again would panic.
                view.imp().detail_timer.take();
                view.check_detail();
            }
        });
        *imp.detail_timer.borrow_mut() = Some(id);
    }

    fn check_detail(&self) {
        let want = {
            let st = self.imp().state.borrow();
            // The continuous view asks per page instead.
            if st.doc.is_some() {
                return;
            }
            let Some(texture) = st.texture.as_ref().filter(|_| !st.placeholder) else {
                return;
            };
            let k = st.vp.scale() * self.device_scale();
            let (w, h) = (st.vp.content.0 * k, st.vp.content.1 * k);
            (w > texture.width() as f64 * DETAIL_SLACK
                || h > texture.height() as f64 * DETAIL_SLACK)
                .then(|| (w.ceil() as i32, h.ceil() as i32))
        };
        let on_detail = self.imp().on_detail.borrow().clone();
        if let (Some((w, h)), Some(f)) = (want, on_detail) {
            f(w, h);
        }
    }

    /// At most every `PAGES_THROTTLE`, never postponed: pages keep arriving during a long
    /// scroll instead of only once it stops.
    fn schedule_pages(&self) {
        let imp = self.imp();
        if imp.state.borrow().doc.is_none() || imp.pages_timer.borrow().is_some() {
            return;
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(PAGES_THROTTLE, move || {
            if let Some(view) = weak.upgrade() {
                view.imp().pages_timer.take();
                view.check_pages();
            }
        });
        *imp.pages_timer.borrow_mut() = Some(id);
    }

    /// Which pages need (sharper) pixels: the visible ones plus half a screen either way.
    /// Pages further than `KEEP_AROUND` from that are dropped.
    fn check_pages(&self) {
        let device = self.device_scale();
        let (wants, keep, shown) = {
            let mut st = self.imp().state.borrow_mut();
            let st = &mut *st;
            let vp = st.vp;
            let Some(doc) = st.doc.as_mut() else { return };
            let s = vp.scale();
            let (_, oy) = vp.origin();
            let margin = vp.view.1 / 2.0 / s;
            let near = doc
                .pages
                .range(-oy / s - margin, (vp.view.1 - oy) / s + margin);
            let keep = near.start.saturating_sub(KEEP_AROUND)
                ..(near.end + KEEP_AROUND).min(doc.pages.len());
            doc.textures.retain(|i, _| keep.contains(i));
            doc.asked.retain(|i, _| keep.contains(i));

            let mut wants = Vec::new();
            for i in near {
                let (_, _, w, _) = doc.pages.rect(i);
                let need = (w * s * device).ceil() as i32;
                let have = doc.textures.get(&i).map_or(0, |t| t.width());
                let asked = doc.asked.get(&i).copied().unwrap_or(0);
                if (have as f64) * DETAIL_SLACK < need as f64 && asked < need {
                    doc.asked.insert(i, need);
                    wants.push((i, need));
                }
            }
            let keep_changed = doc.keep != keep;
            doc.keep = keep.clone();
            // The page in the middle, except at either end: scrolled all the way down
            // means the last page, even when the one above it holds the middle.
            let half = vp.view.1 / 2.0 / s;
            let current = if vp.center.1 + half >= doc.pages.size().1 - 0.5 {
                doc.pages.len() - 1
            } else if vp.center.1 - half <= 0.5 {
                0
            } else {
                doc.pages.page_at(vp.center.1)
            };
            let shown = (current != doc.current).then_some(current);
            doc.current = current;
            (
                (!wants.is_empty() || keep_changed).then_some(wants),
                keep,
                shown,
            )
        };
        let on_pages = self.imp().on_pages.borrow().clone();
        if let (Some(wants), Some(f)) = (wants, on_pages) {
            f(wants, keep);
        }
        let on_page = self.imp().on_page.borrow().clone();
        if let (Some(page), Some(f)) = (shown, on_page) {
            f(page);
        }
    }

    fn anchor(&self) -> (f64, f64) {
        let st = self.imp().state.borrow();
        st.pointer
            .unwrap_or((st.vp.view.0 / 2.0, st.vp.view.1 / 2.0))
    }

    fn interactive(&self) -> bool {
        let st = self.imp().state.borrow();
        !st.placeholder && (st.texture.is_some() || st.doc.is_some())
    }

    fn on_scroll(&self, unit: gdk::ScrollUnit, mods: gdk::ModifierType, dx: f64, dy: f64) {
        if !self.interactive() {
            return;
        }
        let wheel = unit == gdk::ScrollUnit::Wheel;
        let paged = self.imp().state.borrow().paged;
        if mods.contains(gdk::ModifierType::CONTROL_MASK) || (wheel && !paged) {
            let factor = if wheel {
                STEP.powf(-dy)
            } else {
                (-dy * 0.01).exp()
            };
            let anchor = self.anchor();
            self.imp().state.borrow_mut().vp.zoom_by(factor, anchor);
            self.zoomed();
            return;
        }

        let (dx, dy) = if wheel && mods.contains(gdk::ModifierType::SHIFT_MASK) {
            (dy, dx)
        } else {
            (dx, dy)
        };
        let k = if wheel { WHEEL_PAN } else { 1.0 };
        // Reading on: the single page becomes the continuous view.
        if paged && dy > 0.0 {
            self.enter_doc();
        }
        self.imp().state.borrow_mut().vp.pan((dx * k, dy * k));
        self.moved();
    }

    fn toggle_zoom(&self, at: (f64, f64)) {
        {
            let mut st = self.imp().state.borrow_mut();
            if st.vp.zoom == Zoom::Fit {
                let fit = st.vp.fit_scale();
                let target = if !st.paged && fit < 0.99 {
                    1.0
                } else {
                    fit * 2.0
                };
                st.vp.zoom_to(target, at);
            } else {
                st.vp.fit();
            }
        }
        self.zoomed();
    }

    fn install_controllers(&self) {
        let motion = gtk::EventControllerMotion::new();
        let weak = self.downgrade();
        motion.connect_motion(move |_, x, y| {
            if let Some(view) = weak.upgrade() {
                let idle = {
                    let mut st = view.imp().state.borrow_mut();
                    st.pointer = Some((x, y));
                    st.drag_from.is_none() && st.select_from.is_none()
                };
                if idle {
                    let cursor = if view.link_at((x, y)).is_some() {
                        Some("pointer")
                    } else if view.imp().state.borrow().select_mode {
                        Some("text")
                    } else {
                        None
                    };
                    view.set_cursor_from_name(cursor);
                }
            }
        });
        let weak = self.downgrade();
        motion.connect_leave(move |_| {
            if let Some(view) = weak.upgrade() {
                view.imp().state.borrow_mut().pointer = None;
            }
        });
        self.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let weak = self.downgrade();
        scroll.connect_scroll(move |c, dx, dy| {
            if let Some(view) = weak.upgrade() {
                view.on_scroll(c.unit(), c.current_event_state(), dx, dy);
            }
            glib::Propagation::Stop
        });
        self.add_controller(scroll);

        let drag = gtk::GestureDrag::new();
        drag.set_button(0);
        let weak = self.downgrade();
        drag.connect_drag_begin(move |g, _, _| {
            let Some(view) = weak.upgrade() else { return };
            if !matches!(g.current_button(), 1 | 2) || !view.interactive() {
                g.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            // PDFs: Shift+drag, or any drag in select mode, selects text.
            let shift = g
                .current_event_state()
                .contains(gdk::ModifierType::SHIFT_MASK);
            let select =
                g.current_button() == 1 && (shift || view.imp().state.borrow().select_mode);
            if select && let Some(at) = g.start_point().and_then(|p| view.page_point(p)) {
                view.imp().state.borrow_mut().select_from = Some(at);
                view.set_cursor_from_name(Some("text"));
                return;
            }
            let mut st = view.imp().state.borrow_mut();
            st.drag_from = Some(st.vp.center);
        });
        let weak = self.downgrade();
        drag.connect_drag_update(move |g, ox, oy| {
            let Some(view) = weak.upgrade() else { return };
            if view.imp().state.borrow().select_from.is_some() {
                if let Some((x, y)) = g.start_point() {
                    view.emit_select((x + ox, y + oy), false);
                }
                return;
            }
            {
                let mut st = view.imp().state.borrow_mut();
                let Some(from) = st.drag_from else { return };
                // The content follows the pointer.
                st.vp.center = from;
                st.vp.pan((-ox, -oy));
            }
            view.set_cursor_from_name(Some("grabbing"));
            view.moved();
        });
        let weak = self.downgrade();
        drag.connect_drag_end(move |g, ox, oy| {
            if let Some(view) = weak.upgrade() {
                let selecting = view.imp().state.borrow().select_from.is_some();
                // A press without movement is a click, not a selection.
                if selecting
                    && (ox, oy) != (0.0, 0.0)
                    && let Some((x, y)) = g.start_point()
                {
                    view.emit_select((x + ox, y + oy), true);
                }
                let mut st = view.imp().state.borrow_mut();
                st.drag_from = None;
                st.select_from = None;
                let cursor = st.select_mode.then_some("text");
                drop(st);
                view.set_cursor_from_name(cursor);
            }
        });
        self.add_controller(drag);

        let click = gtk::GestureClick::new();
        let weak = self.downgrade();
        click.connect_pressed(move |_, n, x, y| {
            if let Some(view) = weak.upgrade()
                && n == 2
                && view.interactive()
            {
                view.toggle_zoom((x, y));
            }
        });
        // Movement past the drag threshold cancels the click: panning never follows a link.
        let weak = self.downgrade();
        click.connect_released(move |g, n, x, y| {
            let Some(view) = weak.upgrade() else { return };
            if n != 1 || g.current_button() != 1 || !view.interactive() {
                return;
            }
            let link = view.link_at((x, y));
            let on_link = view.imp().on_link.borrow().clone();
            if let Some(f) = on_link {
                f(link);
            }
        });
        self.add_controller(click);

        let pinch = gtk::GestureZoom::new();
        let weak = self.downgrade();
        pinch.connect_begin(move |_, _| {
            if let Some(view) = weak.upgrade() {
                let mut st = view.imp().state.borrow_mut();
                st.pinch_from = Some(st.vp.scale());
            }
        });
        let weak = self.downgrade();
        pinch.connect_scale_changed(move |g, scale| {
            let Some(view) = weak.upgrade() else { return };
            if !view.interactive() {
                return;
            }
            let anchor = g.bounding_box_center().unwrap_or_else(|| view.anchor());
            {
                let mut st = view.imp().state.borrow_mut();
                let Some(from) = st.pinch_from else { return };
                st.vp.zoom_to(from * scale, anchor);
            }
            view.zoomed();
        });
        let weak = self.downgrade();
        pinch.connect_end(move |_, _| {
            if let Some(view) = weak.upgrade() {
                view.imp().state.borrow_mut().pinch_from = None;
            }
        });
        self.add_controller(pinch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo() -> Viewport {
        let mut vp = Viewport::new((1000.0, 1000.0), FitMode::ScaleDown);
        vp.set_view((500.0, 500.0));
        vp
    }

    /// Where content point `p` lands on screen.
    fn screen(vp: &Viewport, p: (f64, f64)) -> (f64, f64) {
        let (x, y) = vp.origin();
        let s = vp.scale();
        (x + p.0 * s, y + p.1 * s)
    }

    #[test]
    fn fit_scales_down_but_never_up() {
        assert_eq!(photo().fit_scale(), 0.5);
        let mut small = Viewport::new((100.0, 50.0), FitMode::ScaleDown);
        small.set_view((500.0, 500.0));
        assert_eq!(small.fit_scale(), 1.0);
        let mut page = Viewport::new((100.0, 50.0), FitMode::Contain);
        page.set_view((500.0, 500.0));
        assert_eq!(page.fit_scale(), 5.0);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let mut vp = photo();
        let anchor = (100.0, 120.0);
        let s = vp.scale();
        let (ox, oy) = vp.origin();
        let p = ((anchor.0 - ox) / s, (anchor.1 - oy) / s);
        vp.zoom_to(2.0, anchor);
        assert_eq!(vp.zoom, Zoom::Free(2.0));
        let (x, y) = screen(&vp, p);
        assert!((x - anchor.0).abs() < 1e-9 && (y - anchor.1).abs() < 1e-9);
    }

    #[test]
    fn content_never_leaves_a_gap() {
        let mut vp = photo();
        vp.zoom_to(2.0, (250.0, 250.0));
        let moved = vp.pan((-10_000.0, 0.0));
        assert_eq!(vp.origin().0, 0.0);
        assert!(moved.0 > -10_000.0);
        assert_eq!(vp.pan((-1.0, 0.0)), (0.0, 0.0));
        vp.pan((10_000.0, 10_000.0));
        let (x, y) = vp.origin();
        assert_eq!((x + 2000.0, y + 2000.0), (500.0, 500.0));
    }

    #[test]
    fn small_content_stays_centered() {
        let mut vp = Viewport::new((100.0, 100.0), FitMode::ScaleDown);
        vp.set_view((500.0, 300.0));
        vp.pan((50.0, 50.0));
        assert_eq!(vp.origin(), (200.0, 100.0));
    }

    #[test]
    fn zooming_back_out_returns_to_fit() {
        let mut vp = photo();
        vp.zoom_to(3.0, (0.0, 0.0));
        vp.zoom_to(0.01, (0.0, 0.0));
        assert_eq!(vp.zoom, Zoom::Fit);
        assert_eq!(vp.center, (500.0, 500.0));
        vp.zoom_to(1000.0, (0.0, 0.0));
        assert_eq!(vp.zoom, Zoom::Free(MAX_ZOOM));
    }

    #[test]
    fn pages_stack_with_gaps() {
        let pages = Pages::new(vec![(100.0, 200.0), (50.0, 100.0), (100.0, 200.0)]);
        assert_eq!(
            pages.size(),
            (100.0, 200.0 + 100.0 + 200.0 + 2.0 * PAGE_GAP)
        );
        assert_eq!(pages.rect(1), (25.0, 200.0 + PAGE_GAP, 50.0, 100.0));
        assert_eq!(pages.page_at(-5.0), 0);
        assert_eq!(pages.page_at(199.0), 0);
        // In the gap: still the page above.
        assert_eq!(pages.page_at(205.0), 0);
        assert_eq!(pages.page_at(200.0 + PAGE_GAP), 1);
        assert_eq!(pages.page_at(1e9), 2);
        assert_eq!(pages.range(150.0, 250.0), 0..2);
        assert_eq!(pages.largest, (100.0, 200.0));
    }

    #[test]
    fn fit_width_spans_the_view() {
        let mut vp = Viewport::new((800.0, 10_000.0), FitMode::Page(800.0, 1000.0));
        vp.set_view((400.0, 500.0));
        vp.fit_width();
        assert_eq!(vp.scale(), 0.5);
        assert_eq!(vp.origin().0, 0.0);
        let mut wide = Viewport::new((200.0, 100.0), FitMode::Contain);
        wide.set_view((400.0, 500.0));
        wide.fit_width();
        assert_eq!(wide.scale(), 2.0);
    }

    #[test]
    fn documents_fit_one_page_and_zoom_out_further() {
        let mut vp = Viewport::new((800.0, 10_000.0), FitMode::Page(800.0, 1000.0));
        vp.set_view((400.0, 500.0));
        assert_eq!(vp.fit_scale(), 0.5);
        vp.zoom_to(0.01, (200.0, 250.0));
        assert_eq!(vp.zoom, Zoom::Free(0.125));
        // Taller than the view: scrolls instead of centering.
        vp.fit();
        vp.pan((0.0, 1000.0));
        assert!(vp.origin().1 < 0.0);
    }
}
