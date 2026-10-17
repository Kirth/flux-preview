//! The PDF outline (table of contents) as an expandable list, for the sidebar.

use crate::app::AppMsg;
use crate::load::{Dest, OutlineItem};
use gtk::prelude::*;
use std::rc::Rc;

/// The outline flattened into an arena, so rows can point into it cheaply.
struct Tree {
    titles: Vec<String>,
    dests: Vec<Option<Dest>>,
    open: Vec<bool>,
    children: Vec<Vec<u32>>,
}

impl Tree {
    /// Node 0 is a root holding the top level.
    fn new(items: &[OutlineItem]) -> Self {
        let mut tree = Self {
            titles: vec![String::new()],
            dests: vec![None],
            open: vec![true],
            children: vec![Vec::new()],
        };
        tree.add(0, items);
        tree
    }

    fn add(&mut self, parent: u32, items: &[OutlineItem]) {
        for item in items {
            let idx = self.titles.len() as u32;
            self.titles.push(item.title.clone());
            self.dests.push(item.dest.clone());
            self.open.push(item.open);
            self.children.push(Vec::new());
            self.children[parent as usize].push(idx);
            self.add(idx, &item.children);
        }
    }
}

/// One row: a node of the shared tree. `BoxedAnyObject` spares a GObject subclass.
struct Entry {
    tree: Rc<Tree>,
    idx: u32,
}

pub struct Outline {
    pub root: gtk::ScrolledWindow,
    view: gtk::ListView,
}

impl Outline {
    pub fn new(sender: &relm4::Sender<AppMsg>) -> Self {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let expander = gtk::TreeExpander::builder().child(&label).build();
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
            let Some(label) = expander.child().and_downcast::<gtk::Label>() else {
                return;
            };
            if let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() {
                let entry = boxed.borrow::<Entry>();
                let title = &entry.tree.titles[entry.idx as usize];
                label.set_label(title);
                label.set_tooltip_text(Some(title));
            }
        });
        let view = gtk::ListView::builder()
            .factory(&factory)
            .single_click_activate(true)
            .build();
        view.add_css_class("navigation-sidebar");
        let s = sender.clone();
        view.connect_activate(move |view, pos| {
            let dest = view
                .model()
                .and_then(|m| m.item(pos))
                .and_downcast::<gtk::TreeListRow>()
                .and_then(|row| row.item().and_downcast::<glib::BoxedAnyObject>())
                .and_then(|boxed| {
                    let entry = boxed.borrow::<Entry>();
                    entry.tree.dests[entry.idx as usize].clone()
                });
            if let Some(dest) = dest {
                s.emit(AppMsg::GoTo(dest));
            }
        });
        let root = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        Self { root, view }
    }

    pub fn set(&self, items: &[OutlineItem]) {
        let tree = Rc::new(Tree::new(items));
        let model = gtk::TreeListModel::new(children(&tree, 0), false, false, |item| {
            let entry = item
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<Entry>();
            (!entry.tree.children[entry.idx as usize].is_empty())
                .then(|| children(&entry.tree, entry.idx).upcast())
        });
        // Expand what the PDF marks as open; expanding inserts rows right after, so a
        // forward walk visits them too.
        let mut i = 0;
        while let Some(row) = model.row(i) {
            let open = row
                .item()
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|b| {
                    let entry = b.borrow::<Entry>();
                    entry.tree.open[entry.idx as usize]
                });
            if open && row.is_expandable() {
                row.set_expanded(true);
            }
            i += 1;
        }
        self.view
            .set_model(Some(&gtk::NoSelection::new(Some(model))));
    }
}

/// A node's children, built when it is first expanded.
fn children(tree: &Rc<Tree>, idx: u32) -> gio::ListStore {
    let items: Vec<glib::BoxedAnyObject> = tree.children[idx as usize]
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
