use super::{Page, format_size, format_unix};
use crate::load::{Listing, Node, Tree, group_digits};
use gtk::prelude::*;
use std::rc::Rc;

/// One row: a node of the shared tree. `BoxedAnyObject` spares a GObject subclass.
struct Entry {
    tree: Rc<Tree>,
    idx: u32,
}

impl Entry {
    fn node(&self) -> &Node {
        &self.tree.nodes[self.idx as usize]
    }
}

/// Archive contents: a summary, then an expandable tree. Read-only; nothing is extracted.
pub struct ArchivePage {
    root: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    notes: gtk::Grid,
    banner: adw::Banner,
    view: gtk::ColumnView,
}

impl Page for ArchivePage {
    const NAME: &'static str = "archive";

    fn build() -> Self {
        let title = gtk::Label::builder().xalign(0.0).build();
        title.add_css_class("title-4");
        let subtitle = gtk::Label::builder().xalign(0.0).wrap(true).build();
        subtitle.add_css_class("dim-label");
        let notes = gtk::Grid::builder()
            .column_spacing(12)
            .row_spacing(2)
            .margin_top(6)
            .visible(false)
            .build();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        header.append(&title);
        header.append(&subtitle);
        header.append(&notes);

        let banner = adw::Banner::new("");
        let view = gtk::ColumnView::builder()
            .reorderable(false)
            .show_row_separators(false)
            .build();
        view.add_css_class("data-table");
        view.append_column(&name_column());
        view.append_column(&text_column("Size", 1.0, size_text));
        view.append_column(&text_column("Modified", 0.0, |node| {
            (node.mtime.and_then(format_unix).unwrap_or_default(), None)
        }));
        // Double-click (or activate) a folder to open or close it.
        view.connect_activate(|view, pos| {
            let row = view
                .model()
                .and_then(|m| m.item(pos))
                .and_downcast::<gtk::TreeListRow>();
            if let Some(row) = row.filter(gtk::TreeListRow::is_expandable) {
                row.set_expanded(!row.is_expanded());
            }
        });
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&banner);
        root.append(&scroller);
        Self {
            root,
            title,
            subtitle,
            notes,
            banner,
            view,
        }
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}

impl ArchivePage {
    pub fn set(&self, listing: Listing) {
        crate::hit!("archive_set");
        let Listing {
            format,
            tree,
            packed,
            encrypted,
            truncated,
            notes,
        } = listing;
        let tree = Rc::new(tree);
        let top = tree.root();

        let folders = tree.folders() as u64;
        self.title.set_label(&format!(
            "{}  ·  {}",
            count(top.files, "file", "files"),
            count(folders, "folder", "folders")
        ));
        let mut parts = Vec::new();
        if top.known && top.files > 0 {
            parts.push(format!("{} unpacked", format_size(top.size)));
            if let Some(ratio) = (packed * 100).checked_div(top.size) {
                parts.push(format!("{ratio}% on disk"));
            }
        }
        parts.push(format);
        self.subtitle.set_label(&parts.join("  ·  "));

        while let Some(child) = self.notes.first_child() {
            self.notes.remove(&child);
        }
        for (row, (key, value)) in notes.iter().enumerate() {
            let key = gtk::Label::builder().label(key).xalign(0.0).build();
            key.add_css_class("dim-label");
            let value = gtk::Label::builder()
                .label(value)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            self.notes.attach(&key, 0, row as i32, 1, 1);
            self.notes.attach(&value, 1, row as i32, 1, 1);
        }
        self.notes.set_visible(!notes.is_empty());

        let mut warnings: Vec<String> = truncated.into_iter().collect();
        if encrypted > 0 {
            warnings.push(format!(
                "{} encrypted",
                count(encrypted as u64, "file", "files")
            ));
        }
        self.banner.set_title(&warnings.join("  ·  "));
        self.banner.set_revealed(!warnings.is_empty());

        let model = gtk::TreeListModel::new(children(&tree, 0), false, false, |item| {
            let entry = item
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<Entry>();
            let node = entry.node();
            (node.is_dir && !node.children.is_empty())
                .then(|| children(&entry.tree, entry.idx).upcast())
        });
        // The common `project-1.0/` layout: open the one folder.
        if let [only] = top.children[..]
            && tree.nodes[only as usize].is_dir
            && let Some(row) = model.row(0)
        {
            row.set_expanded(true);
        }
        self.view
            .set_model(Some(&gtk::SingleSelection::new(Some(model))));
        self.view
            .scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
        self.view.grab_focus();
    }
}

/// A folder's children, built when it is first expanded.
fn children(tree: &Rc<Tree>, idx: u32) -> gio::ListStore {
    let items: Vec<glib::BoxedAnyObject> = tree.nodes[idx as usize]
        .children
        .iter()
        .map(|&idx| {
            glib::BoxedAnyObject::new(Entry {
                tree: tree.clone(),
                idx,
            })
        })
        .collect();
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    store.extend_from_slice(&items);
    store
}

fn count(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

/// Folders: their total and, as a tooltip, how many files that is.
fn size_text(node: &Node) -> (String, Option<String>) {
    match (node.is_dir, node.known) {
        (true, _) => (
            format_size(node.size),
            Some(count(node.files, "file", "files")),
        ),
        (false, true) => (format_size(node.size), None),
        (false, false) => ("—".into(), Some("Unknown until unpacked".into())),
    }
}

fn row_node<R>(item: &glib::Object, f: impl FnOnce(&Node) -> R) -> Option<R> {
    let row = item.downcast_ref::<gtk::TreeListRow>()?;
    let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let entry = boxed.borrow::<Entry>();
    Some(f(entry.node()))
}

fn name_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let icon = gtk::Image::new();
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&icon);
        content.append(&label);
        let expander = gtk::TreeExpander::builder().child(&content).build();
        item.set_child(Some(&expander));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
            return;
        };
        expander.set_list_row(Some(&row));
        let content = expander.child().and_downcast::<gtk::Box>();
        let icon = content
            .as_ref()
            .and_then(|c| c.first_child())
            .and_downcast::<gtk::Image>();
        let label = content
            .as_ref()
            .and_then(|c| c.last_child())
            .and_downcast::<gtk::Label>();
        let (Some(icon), Some(label)) = (icon, label) else {
            return;
        };
        row_node(row.upcast_ref(), |node| {
            label.set_label(&node.name);
            if node.is_dir {
                icon.set_icon_name(Some("folder-symbolic"));
                icon.set_tooltip_text(None);
            } else if node.encrypted {
                icon.set_icon_name(Some("changes-prevent-symbolic"));
                icon.set_tooltip_text(Some("Encrypted"));
            } else {
                // From the name alone: no I/O on the GTK thread.
                let (ctype, _) = gio::content_type_guess(Some(node.name.as_str()), None::<&[u8]>);
                icon.set_from_gicon(&gio::content_type_get_symbolic_icon(&ctype));
                icon.set_tooltip_text(None);
            }
        });
    });
    let column = gtk::ColumnViewColumn::new(Some("Name"), Some(factory));
    column.set_expand(true);
    column.set_resizable(true);
    column
}

fn text_column(
    title: &str,
    xalign: f32,
    text: fn(&Node) -> (String, Option<String>),
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let label = gtk::Label::builder().xalign(xalign).build();
        label.add_css_class("numeric");
        label.add_css_class("dim-label");
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let Some(label) = item.child().and_downcast::<gtk::Label>() else {
            return;
        };
        if let Some((value, tooltip)) = item.item().and_then(|row| row_node(&row, text)) {
            label.set_label(&value);
            label.set_tooltip_text(tooltip.as_deref());
        }
    });
    let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    column.set_resizable(true);
    column
}
