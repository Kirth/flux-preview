//! Just enough of poppler-glib, called directly: the `poppler` crate has no text search,
//! selection, links or outline, and keeps its pointers private. Pool threads only.
//!
//! Rects here are `(x, y, w, h)` in points with a top-left origin, on the unrotated page.

use poppler::cairo;
use std::ffi::{CStr, CString, c_char, c_double, c_int, c_uint, c_void};
use std::path::Path;
use std::ptr;

pub type RectPt = (f64, f64, f64, f64);

/// Where a link or outline entry leads.
#[derive(Clone, Debug, PartialEq)]
pub enum Dest {
    /// 0-based page; `y` from the page's top, in points, when the PDF says.
    Page {
        page: u32,
        y: Option<f64>,
    },
    Uri(String),
}

#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub dest: Option<Dest>,
    /// Shown expanded by default.
    pub open: bool,
    pub children: Vec<OutlineItem>,
}

pub struct Document(*mut ffi::Document);

pub struct Page(*mut ffi::Page);

impl Drop for Document {
    fn drop(&mut self) {
        unsafe { glib::gobject_ffi::g_object_unref(self.0 as *mut _) }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        unsafe { glib::gobject_ffi::g_object_unref(self.0 as *mut _) }
    }
}

/// Takes ownership of a g_malloc'd string.
unsafe fn take_string(s: *mut c_char) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let out = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
    unsafe { glib::ffi::g_free(s as *mut c_void) };
    Some(out)
}

/// Bottom-left origin in, top-left out.
fn flip(r: &ffi::Rectangle, page_h: f64) -> RectPt {
    let (x1, x2) = (r.x1.min(r.x2), r.x1.max(r.x2));
    let (y1, y2) = (r.y1.min(r.y2), r.y1.max(r.y2));
    (x1, page_h - y2, x2 - x1, y2 - y1)
}

impl Document {
    pub fn open(path: &Path) -> Result<Self, String> {
        use gio::prelude::FileExt;
        let uri =
            CString::new(gio::File::for_path(path).uri().as_str()).map_err(|e| e.to_string())?;
        let mut error: *mut glib::ffi::GError = ptr::null_mut();
        let doc =
            unsafe { ffi::poppler_document_new_from_file(uri.as_ptr(), ptr::null(), &mut error) };
        if doc.is_null() {
            if error.is_null() {
                return Err("cannot open PDF".into());
            }
            let message = unsafe { CStr::from_ptr((*error).message) }
                .to_string_lossy()
                .into_owned();
            unsafe { glib::ffi::g_error_free(error) };
            return Err(message);
        }
        Ok(Self(doc))
    }

    pub fn pages(&self) -> u32 {
        unsafe { ffi::poppler_document_get_n_pages(self.0) }.max(0) as u32
    }

    pub fn page(&self, index: u32) -> Option<Page> {
        let page = unsafe { ffi::poppler_document_get_page(self.0, index as c_int) };
        (!page.is_null()).then_some(Page(page))
    }

    /// The table of contents; empty when the PDF has none.
    pub fn outline(&self) -> Vec<OutlineItem> {
        let iter = unsafe { ffi::poppler_index_iter_new(self.0) };
        if iter.is_null() {
            return Vec::new();
        }
        let items = unsafe { self.outline_level(iter) };
        unsafe { ffi::poppler_index_iter_free(iter) };
        items
    }

    /// Walks one level, recursing into children. Consumes nothing: `iter` is freed by the caller.
    unsafe fn outline_level(&self, iter: *mut ffi::IndexIter) -> Vec<OutlineItem> {
        let mut items = Vec::new();
        loop {
            let action = unsafe { ffi::poppler_index_iter_get_action(iter) };
            if !action.is_null() {
                let any = unsafe { &*(action as *const ffi::ActionAny) };
                let title = if any.title.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(any.title) }
                        .to_string_lossy()
                        .trim()
                        .to_owned()
                };
                let dest = unsafe { self.resolve(action) };
                unsafe { ffi::poppler_action_free(action) };
                let child = unsafe { ffi::poppler_index_iter_get_child(iter) };
                let children = if child.is_null() {
                    Vec::new()
                } else {
                    let children = unsafe { self.outline_level(child) };
                    unsafe { ffi::poppler_index_iter_free(child) };
                    children
                };
                items.push(OutlineItem {
                    title,
                    dest,
                    open: unsafe { ffi::poppler_index_iter_is_open(iter) } != 0,
                    children,
                });
            }
            if unsafe { ffi::poppler_index_iter_next(iter) } == 0 {
                break;
            }
        }
        items
    }

    /// What an action leads to, if it is a jump within the document or a URI.
    unsafe fn resolve(&self, action: *const ffi::Action) -> Option<Dest> {
        let any = unsafe { &*(action as *const ffi::ActionAny) };
        match any.kind {
            ffi::ACTION_GOTO_DEST => {
                let dest = unsafe { (*(action as *const ffi::ActionGotoDest)).dest };
                unsafe { self.resolve_dest(dest) }
            }
            ffi::ACTION_URI => {
                let uri = unsafe { (*(action as *const ffi::ActionUri)).uri };
                (!uri.is_null())
                    .then(|| {
                        unsafe { CStr::from_ptr(uri) }
                            .to_string_lossy()
                            .into_owned()
                    })
                    .map(Dest::Uri)
            }
            _ => None,
        }
    }

    unsafe fn resolve_dest(&self, dest: *const ffi::Dest) -> Option<Dest> {
        if dest.is_null() {
            return None;
        }
        let d = unsafe { &*dest };
        if d.kind == ffi::DEST_NAMED {
            if d.named_dest.is_null() {
                return None;
            }
            let found = unsafe { ffi::poppler_document_find_dest(self.0, d.named_dest) };
            if found.is_null() {
                return None;
            }
            let out = unsafe { self.resolve_dest(found) };
            unsafe { ffi::poppler_dest_free(found) };
            return out;
        }
        let page = u32::try_from(d.page_num - 1).ok()?;
        let has_top = d.changes & 0b10 != 0
            && matches!(
                d.kind,
                ffi::DEST_XYZ | ffi::DEST_FITH | ffi::DEST_FITBH | ffi::DEST_FITR
            );
        let y = has_top
            .then(|| self.page(page).map(|p| p.size().1 - d.top))
            .flatten()
            .map(|y| y.max(0.0));
        Some(Dest::Page { page, y })
    }

    /// The links on page `index`.
    pub fn links(&self, index: u32) -> Vec<(RectPt, Dest)> {
        let Some(page) = self.page(index) else {
            return Vec::new();
        };
        let h = page.size().1;
        let list = unsafe { ffi::poppler_page_get_link_mapping(page.0) };
        let mut links = Vec::new();
        let mut node = list;
        while !node.is_null() {
            let mapping = unsafe { &*((*node).data as *const ffi::LinkMapping) };
            if !mapping.action.is_null()
                && let Some(dest) = unsafe { self.resolve(mapping.action) }
            {
                links.push((flip(&mapping.area, h), dest));
            }
            node = unsafe { (*node).next };
        }
        unsafe { ffi::poppler_page_free_link_mapping(list) };
        links
    }
}

impl Page {
    pub fn size(&self) -> (f64, f64) {
        let (mut w, mut h) = (0.0, 0.0);
        unsafe { ffi::poppler_page_get_size(self.0, &mut w, &mut h) };
        (w, h)
    }

    pub fn render(&self, cx: &cairo::Context) {
        unsafe { ffi::poppler_page_render(self.0, cx.to_raw_none() as *mut c_void) }
    }

    /// Case-insensitive matches of `query`.
    pub fn find(&self, query: &CStr) -> Vec<RectPt> {
        let h = self.size().1;
        let list = unsafe { ffi::poppler_page_find_text_with_options(self.0, query.as_ptr(), 0) };
        let mut rects = Vec::new();
        let mut node = list;
        while !node.is_null() {
            rects.push(flip(
                unsafe { &*((*node).data as *const ffi::Rectangle) },
                h,
            ));
            node = unsafe { (*node).next };
        }
        unsafe { glib::ffi::g_list_free_full(list, Some(ffi::free_rectangle)) };
        rects
    }

    fn selection(from: (f64, f64), to: (f64, f64)) -> ffi::Rectangle {
        ffi::Rectangle {
            x1: from.0,
            y1: from.1,
            x2: to.0,
            y2: to.1,
        }
    }

    /// The text a drag from `from` to `to` (top-left points) selects, in reading order.
    pub fn selected_text(&self, from: (f64, f64), to: (f64, f64)) -> String {
        let mut rect = Self::selection(from, to);
        unsafe {
            take_string(ffi::poppler_page_get_selected_text(
                self.0,
                ffi::SELECTION_GLYPH,
                &mut rect,
            ))
        }
        .unwrap_or_default()
    }

    /// The boxes covering that selection.
    pub fn selected_region(&self, from: (f64, f64), to: (f64, f64)) -> Vec<RectPt> {
        let mut rect = Self::selection(from, to);
        let region = unsafe {
            ffi::poppler_page_get_selected_region(self.0, 1.0, ffi::SELECTION_GLYPH, &mut rect)
        };
        if region.is_null() {
            return Vec::new();
        }
        let region = unsafe { cairo::Region::from_raw_full(region as *mut _) };
        (0..region.num_rectangles())
            .map(|i| {
                let r = region.rectangle(i);
                (
                    r.x() as f64,
                    r.y() as f64,
                    r.width() as f64,
                    r.height() as f64,
                )
            })
            .collect()
    }
}

mod ffi {
    use super::*;

    #[repr(C)]
    pub struct Rectangle {
        pub x1: c_double,
        pub y1: c_double,
        pub x2: c_double,
        pub y2: c_double,
    }

    #[repr(C)]
    pub struct LinkMapping {
        pub area: Rectangle,
        pub action: *mut Action,
    }

    pub const ACTION_GOTO_DEST: c_int = 2;
    pub const ACTION_URI: c_int = 5;
    pub const DEST_XYZ: c_int = 1;
    pub const DEST_FITH: c_int = 3;
    pub const DEST_FITR: c_int = 5;
    pub const DEST_FITBH: c_int = 7;
    pub const DEST_NAMED: c_int = 9;
    pub const SELECTION_GLYPH: c_int = 0;

    /// The fields every `PopplerAction` variant starts with.
    #[repr(C)]
    pub struct ActionAny {
        pub kind: c_int,
        pub title: *const c_char,
    }

    #[repr(C)]
    pub struct ActionGotoDest {
        pub kind: c_int,
        pub title: *const c_char,
        pub dest: *const Dest,
    }

    #[repr(C)]
    pub struct ActionUri {
        pub kind: c_int,
        pub title: *const c_char,
        pub uri: *const c_char,
    }

    #[repr(C)]
    pub struct Dest {
        pub kind: c_int,
        pub page_num: c_int,
        pub left: c_double,
        pub bottom: c_double,
        pub right: c_double,
        pub top: c_double,
        pub zoom: c_double,
        pub named_dest: *const c_char,
        /// Bitfields: change_left, change_top, change_zoom.
        pub changes: c_uint,
    }

    pub enum Document {}
    pub enum Page {}
    pub enum Action {}
    pub enum IndexIter {}

    #[link(name = "poppler-glib")]
    unsafe extern "C" {
        pub fn poppler_document_new_from_file(
            uri: *const c_char,
            password: *const c_char,
            error: *mut *mut glib::ffi::GError,
        ) -> *mut Document;
        pub fn poppler_document_get_n_pages(doc: *mut Document) -> c_int;
        pub fn poppler_document_get_page(doc: *mut Document, index: c_int) -> *mut Page;
        pub fn poppler_document_find_dest(doc: *mut Document, name: *const c_char) -> *mut Dest;
        pub fn poppler_dest_free(dest: *mut Dest);
        pub fn poppler_action_free(action: *mut Action);

        pub fn poppler_index_iter_new(doc: *mut Document) -> *mut IndexIter;
        pub fn poppler_index_iter_free(iter: *mut IndexIter);
        pub fn poppler_index_iter_get_child(iter: *mut IndexIter) -> *mut IndexIter;
        pub fn poppler_index_iter_is_open(iter: *mut IndexIter) -> glib::ffi::gboolean;
        pub fn poppler_index_iter_get_action(iter: *mut IndexIter) -> *mut Action;
        pub fn poppler_index_iter_next(iter: *mut IndexIter) -> glib::ffi::gboolean;

        pub fn poppler_page_get_size(page: *mut Page, w: *mut c_double, h: *mut c_double);
        pub fn poppler_page_render(page: *mut Page, cairo: *mut c_void);
        pub fn poppler_page_find_text_with_options(
            page: *mut Page,
            text: *const c_char,
            options: c_uint,
        ) -> *mut glib::ffi::GList;
        pub fn poppler_rectangle_free(rect: *mut Rectangle);
        pub fn poppler_page_get_link_mapping(page: *mut Page) -> *mut glib::ffi::GList;
        pub fn poppler_page_free_link_mapping(list: *mut glib::ffi::GList);
        pub fn poppler_page_get_selected_text(
            page: *mut Page,
            style: c_int,
            selection: *mut Rectangle,
        ) -> *mut c_char;
        pub fn poppler_page_get_selected_region(
            page: *mut Page,
            scale: c_double,
            style: c_int,
            selection: *mut Rectangle,
        ) -> *mut c_void;
    }

    pub unsafe extern "C" fn free_rectangle(rect: *mut c_void) {
        unsafe { poppler_rectangle_free(rect as *mut Rectangle) }
    }
}
