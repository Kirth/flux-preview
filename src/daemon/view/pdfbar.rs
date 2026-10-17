//! The PDF controls: a bottom bar for pages, zoom and rotation, and the Ctrl+F search
//! bar. Built on the first PDF; handlers only post messages (ZAP §5).

use super::ZoomCmd;
use crate::app::{AppMsg, PageCmd};
use gtk::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

const PRESETS: [(&str, ZoomCmd); 9] = [
    ("Fit page", ZoomCmd::Fit),
    ("Fit width", ZoomCmd::FitWidth),
    ("50%", ZoomCmd::Set(0.5)),
    ("75%", ZoomCmd::Set(0.75)),
    ("100%", ZoomCmd::Set(1.0)),
    ("125%", ZoomCmd::Set(1.25)),
    ("150%", ZoomCmd::Set(1.5)),
    ("200%", ZoomCmd::Set(2.0)),
    ("400%", ZoomCmd::Set(4.0)),
];

pub struct PdfBar {
    pub bottom: gtk::CenterBox,
    pub search: gtk::SearchBar,
    entry: gtk::SearchEntry,
    count: gtk::Label,
    page: gtk::Entry,
    pages: gtk::Label,
    /// The page on screen (0-based), to put back when an edit is abandoned.
    shown: Rc<Cell<i32>>,
    zoom: gtk::MenuButton,
    /// Shows the outline sidebar; hidden when the PDF has no outline.
    pub outline: gtk::ToggleButton,
    select: gtk::ToggleButton,
}

fn button(
    icon: &str,
    tooltip: &str,
    sender: &relm4::Sender<AppMsg>,
    msg: fn() -> AppMsg,
) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .focus_on_click(false)
        .build();
    let s = sender.clone();
    button.connect_clicked(move |_| s.emit(msg()));
    button
}

fn group() -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    group.add_css_class("toolbar");
    group
}

impl PdfBar {
    pub fn new(sender: &relm4::Sender<AppMsg>) -> Self {
        let shown = Rc::new(Cell::new(0));

        let page = gtk::Entry::builder()
            .width_chars(4)
            .max_width_chars(6)
            .xalign(1.0)
            .input_purpose(gtk::InputPurpose::Digits)
            .tooltip_text("Go to page")
            .valign(gtk::Align::Center)
            .build();
        let s = sender.clone();
        let restore = shown.clone();
        page.connect_activate(move |entry| {
            match entry.text().trim().parse::<u32>() {
                Ok(n) if n >= 1 => s.emit(AppMsg::Page(PageCmd::Go(n - 1))),
                _ => entry.set_text(&(restore.get() + 1).to_string()),
            }
            // Back to the page, so the view's keys work again.
            if let Some(root) = entry.root() {
                root.set_focus(None::<&gtk::Widget>);
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let restore = shown.clone();
        let weak = page.downgrade();
        focus.connect_leave(move |_| {
            if let Some(entry) = weak.upgrade() {
                entry.set_text(&(restore.get() + 1).to_string());
            }
        });
        page.add_controller(focus);
        let pages = gtk::Label::new(Some("/ 1"));
        pages.add_css_class("dim-label");

        let nav = group();
        nav.append(&button(
            "go-previous-symbolic",
            "Previous page",
            sender,
            || AppMsg::Page(PageCmd::Step(-1)),
        ));
        nav.append(&page);
        nav.append(&pages);
        nav.append(&button("go-next-symbolic", "Next page", sender, || {
            AppMsg::Page(PageCmd::Step(1))
        }));

        let presets = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = gtk::Popover::builder().child(&presets).build();
        for (label, cmd) in PRESETS {
            let item = gtk::Button::builder().label(label).build();
            item.add_css_class("flat");
            if let Some(label) = item.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.0);
            }
            let s = sender.clone();
            let weak = popover.downgrade();
            item.connect_clicked(move |_| {
                if let Some(popover) = weak.upgrade() {
                    popover.popdown();
                }
                s.emit(AppMsg::Zoom(cmd));
            });
            presets.append(&item);
        }
        let zoom = gtk::MenuButton::builder()
            .label("Fit")
            .popover(&popover)
            .tooltip_text("Zoom")
            .focus_on_click(false)
            .build();
        let zooms = group();
        zooms.append(&button("zoom-out-symbolic", "Zoom out", sender, || {
            AppMsg::Zoom(ZoomCmd::Out)
        }));
        zooms.append(&zoom);
        zooms.append(&button("zoom-in-symbolic", "Zoom in", sender, || {
            AppMsg::Zoom(ZoomCmd::In)
        }));

        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Find in document")
            .hexpand(true)
            .build();
        let count = gtk::Label::new(None);
        count.add_css_class("dim-label");
        count.add_css_class("numeric");
        let find = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        find.append(&entry);
        find.append(&count);
        find.append(&button("go-up-symbolic", "Previous match", sender, || {
            AppMsg::SearchStep(-1)
        }));
        find.append(&button("go-down-symbolic", "Next match", sender, || {
            AppMsg::SearchStep(1)
        }));
        let clamp = adw::Clamp::builder().maximum_size(480).child(&find).build();
        let search = gtk::SearchBar::builder().child(&clamp).build();
        search.connect_entry(&entry);

        let s = sender.clone();
        entry.connect_search_changed(move |entry| {
            s.emit(AppMsg::SearchChanged(entry.text().into()))
        });
        let s = sender.clone();
        entry.connect_activate(move |_| s.emit(AppMsg::SearchStep(1)));
        let s = sender.clone();
        entry.connect_next_match(move |_| s.emit(AppMsg::SearchStep(1)));
        let s = sender.clone();
        entry.connect_previous_match(move |_| s.emit(AppMsg::SearchStep(-1)));
        let back = gtk::ShortcutController::new();
        let s = sender.clone();
        back.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Shift>Return|<Shift>KP_Enter"),
            Some(gtk::CallbackAction::new(move |_, _| {
                s.emit(AppMsg::SearchStep(-1));
                glib::Propagation::Stop
            })),
        ));
        entry.add_controller(back);
        let s = sender.clone();
        search.connect_search_mode_enabled_notify(move |bar| {
            if !bar.is_search_mode() {
                s.emit(AppMsg::SearchClose);
            }
        });

        let toggle = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Find (Ctrl+F)")
            .focus_on_click(false)
            .build();
        search
            .bind_property("search-mode-enabled", &toggle, "active")
            .bidirectional()
            .sync_create()
            .build();
        let outline = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Outline")
            .focus_on_click(false)
            .visible(false)
            .build();
        nav.prepend(&outline);

        let select = gtk::ToggleButton::builder()
            .icon_name("edit-select-symbolic")
            .tooltip_text("Select text (or Shift+drag); Ctrl+C copies")
            .focus_on_click(false)
            .build();
        let s = sender.clone();
        select.connect_toggled(move |b| s.emit(AppMsg::SelectMode(b.is_active())));

        let tools = group();
        tools.append(&select);
        tools.append(&button(
            "object-rotate-right-symbolic",
            "Rotate (R, Shift+R)",
            sender,
            || AppMsg::Rotate(90),
        ));
        tools.append(&toggle);

        let bottom = gtk::CenterBox::builder()
            .start_widget(&nav)
            .center_widget(&zooms)
            .end_widget(&tools)
            .build();

        Self {
            bottom,
            search,
            entry,
            count,
            page,
            pages,
            shown,
            zoom,
            outline,
            select,
        }
    }

    /// Back to panning, for the next file.
    pub fn reset_select(&self) {
        self.select.set_active(false);
    }

    /// `page` is 0-based. Left alone while the page entry is being typed in.
    pub fn set_page(&self, page: i32, pages: i32) {
        self.shown.set(page);
        let typing = self
            .page
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| focus.is_ancestor(&self.page));
        if !typing {
            self.page.set_text(&(page + 1).to_string());
        }
        self.pages.set_text(&format!("/ {}", pages.max(1)));
    }

    /// `None`: fitted to the window.
    pub fn set_zoom(&self, zoom: Option<f64>) {
        let label = match zoom {
            Some(z) => format!("{:.0}%", z * 100.0),
            None => "Fit".to_owned(),
        };
        self.zoom.set_label(&label);
    }

    pub fn set_count(&self, text: &str) {
        self.count.set_text(text);
    }

    pub fn open_search(&self) {
        self.search.set_search_mode(true);
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    /// Hides the bar and forgets the query; the app drops the results itself.
    pub fn close_search(&self) {
        self.search.set_search_mode(false);
        self.entry.set_text("");
        self.count.set_text("");
    }

    pub fn is_searching(&self) -> bool {
        self.search.is_search_mode()
    }
}
