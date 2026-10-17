//! A texture you can zoom and pan: the canvas of the image page.
//!
//! Drawn in `snapshot()` with a single transform instead of resizing a child inside a
//! ScrolledWindow, so zooming never waits for a layout pass and the point under the
//! cursor stays exactly put. The texture is always stretched over the content rect, so a
//! sharper decode can be swapped in without the view moving.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

const MAX_ZOOM: f64 = 32.0;
/// One wheel notch or key press.
const STEP: f64 = 1.25;
/// Keyboard pan, in screen px.
pub const PAN_STEP: f64 = 64.0;
const WHEEL_PAN: f64 = 48.0;
/// Touchpad: scrolling this far against a page edge flips the page.
const FLIP_OVERSCROLL: f64 = 120.0;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitMode {
    /// Images: never upscaled to fit.
    ScaleDown,
    /// PDF pages and placeholders: always fill the window.
    Contain,
}

#[derive(Clone, Copy, Debug)]
pub enum ZoomCmd {
    In,
    Out,
    Fit,
    Actual,
    /// Move the view by screen px.
    Pan(f64, f64),
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
        let s = (self.view.0 / self.content.0).min(self.view.1 / self.content.1);
        match self.fit {
            FitMode::ScaleDown => s.min(1.0),
            FitMode::Contain => s,
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
        let new = scale.clamp(fit.min(1.0), MAX_ZOOM);
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

    /// Shows the top (`dir > 0`, arrived going forward) or bottom edge of the content.
    pub fn enter_from(&mut self, dir: i32) {
        self.center.1 = if dir > 0 { 0.0 } else { self.content.1 };
        self.clamp();
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

type DetailFn = Rc<dyn Fn(i32, i32)>;
type PageFn = Rc<dyn Fn(i32)>;
type ZoomFn = Rc<dyn Fn(Option<f64>)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct State {
        pub vp: Viewport,
        pub texture: Option<gdk::Texture>,
        pub placeholder: bool,
        /// PDF: scrolling pans, and flips pages at the edges.
        pub paged: bool,
        pub pointer: Option<(f64, f64)>,
        /// `center` when the drag started.
        pub drag_from: Option<(f64, f64)>,
        /// Scale when the pinch started.
        pub pinch_from: Option<f64>,
        pub overscroll: f64,
    }

    #[derive(Default)]
    pub struct ZoomView {
        pub state: RefCell<State>,
        pub detail_timer: RefCell<Option<glib::SourceId>>,
        pub on_detail: RefCell<Option<DetailFn>>,
        pub on_page: RefCell<Option<PageFn>>,
        pub on_zoom: RefCell<Option<ZoomFn>>,
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
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let st = self.state.borrow();
            let Some(texture) = &st.texture else {
                return;
            };
            let s = st.vp.scale();
            let (x, y) = st.vp.origin();
            let (w, h) = (st.vp.content.0 * s, st.vp.content.1 * s);
            let per_texel = w * self.obj().device_scale() / texture.width().max(1) as f64;
            let filter = if st.placeholder || per_texel < NEAREST_FROM {
                if per_texel < 0.5 {
                    // Mipmapped: big photos shrunk to the window stay smooth.
                    gsk::ScalingFilter::Trilinear
                } else {
                    gsk::ScalingFilter::Linear
                }
            } else {
                gsk::ScalingFilter::Nearest
            };
            snapshot.append_scaled_texture(
                texture,
                filter,
                &graphene::Rect::new(x as f32, y as f32, w as f32, h as f32),
            );
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
            st.drag_from = None;
            st.pinch_from = None;
            st.overscroll = 0.0;
        }
        self.queue_draw();
        if !placeholder {
            self.schedule_detail();
        }
    }

    /// Another page of the same document: keeps the zoom, enters from the edge it was
    /// flipped towards.
    pub fn set_page(&self, texture: &gdk::Texture, content: (f64, f64), dir: i32) {
        {
            let mut st = self.imp().state.borrow_mut();
            st.vp.content = (content.0.max(1.0), content.1.max(1.0));
            st.vp.center.0 = st.vp.content.0 / 2.0;
            st.vp.enter_from(dir);
            st.texture = Some(texture.clone());
            st.overscroll = 0.0;
        }
        self.queue_draw();
        self.schedule_detail();
    }

    /// Sharper pixels for what is on screen; the view does not move.
    pub fn set_texture(&self, texture: &gdk::Texture) {
        self.imp().state.borrow_mut().texture = Some(texture.clone());
        self.queue_draw();
    }

    pub fn apply(&self, cmd: ZoomCmd) {
        let mut st = self.imp().state.borrow_mut();
        if st.placeholder || st.texture.is_none() {
            return;
        }
        let middle = (st.vp.view.0 / 2.0, st.vp.view.1 / 2.0);
        match cmd {
            ZoomCmd::In => st.vp.zoom_by(STEP, middle),
            ZoomCmd::Out => st.vp.zoom_by(1.0 / STEP, middle),
            ZoomCmd::Fit => st.vp.fit(),
            ZoomCmd::Actual => st.vp.zoom_to(1.0, middle),
            ZoomCmd::Pan(dx, dy) => {
                st.vp.pan((dx, dy));
                drop(st);
                self.queue_draw();
                return;
            }
        }
        drop(st);
        self.zoomed();
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

    /// Called with ±1 when scrolling runs past the bottom or top of a page.
    pub fn connect_page_edge(&self, f: impl Fn(i32) + 'static) {
        *self.imp().on_page.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_zoom_changed(&self, f: impl Fn(Option<f64>) + 'static) {
        *self.imp().on_zoom.borrow_mut() = Some(Rc::new(f));
    }

    fn zoomed(&self) {
        self.queue_draw();
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

    fn flip(&self, dir: i32) {
        let on_page = self.imp().on_page.borrow().clone();
        if let Some(f) = on_page {
            f(dir);
        }
    }

    fn anchor(&self) -> (f64, f64) {
        let st = self.imp().state.borrow();
        st.pointer
            .unwrap_or((st.vp.view.0 / 2.0, st.vp.view.1 / 2.0))
    }

    fn interactive(&self) -> bool {
        let st = self.imp().state.borrow();
        !st.placeholder && st.texture.is_some()
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
        let mut flip = None;
        {
            let mut st = self.imp().state.borrow_mut();
            let moved = st.vp.pan((dx * k, dy * k));
            if paged && dy != 0.0 && moved.1.abs() < 0.5 {
                if wheel {
                    flip = Some(dy.signum() as i32);
                } else {
                    // Fresh direction: start over.
                    if st.overscroll * dy < 0.0 {
                        st.overscroll = 0.0;
                    }
                    st.overscroll += dy;
                    if st.overscroll.abs() > FLIP_OVERSCROLL {
                        flip = Some(st.overscroll.signum() as i32);
                        st.overscroll = 0.0;
                    }
                }
            } else {
                st.overscroll = 0.0;
            }
        }
        self.queue_draw();
        if let Some(dir) = flip {
            self.flip(dir);
        }
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
                view.imp().state.borrow_mut().pointer = Some((x, y));
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
            let mut st = view.imp().state.borrow_mut();
            st.drag_from = Some(st.vp.center);
        });
        let weak = self.downgrade();
        drag.connect_drag_update(move |_, ox, oy| {
            let Some(view) = weak.upgrade() else { return };
            {
                let mut st = view.imp().state.borrow_mut();
                let Some(from) = st.drag_from else { return };
                // The content follows the pointer.
                st.vp.center = from;
                st.vp.pan((-ox, -oy));
            }
            view.set_cursor_from_name(Some("grabbing"));
            view.queue_draw();
        });
        let weak = self.downgrade();
        drag.connect_drag_end(move |_, _, _| {
            if let Some(view) = weak.upgrade() {
                view.imp().state.borrow_mut().drag_from = None;
                view.set_cursor_from_name(None);
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
    fn pages_enter_from_the_edge() {
        let mut vp = Viewport::new((800.0, 1100.0), FitMode::Contain);
        vp.set_view((800.0, 600.0));
        vp.zoom_to(2.0, (400.0, 300.0));
        vp.enter_from(1);
        assert_eq!(vp.origin().1, 0.0);
        vp.enter_from(-1);
        assert_eq!(vp.origin().1 + 2200.0, 600.0);
    }
}
