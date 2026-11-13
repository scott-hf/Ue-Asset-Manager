//! Multi-select filters of the library: a "Filters" menu with check rows (the content sources
//! "Fab" and "Local" first, then the categories) and a row of removable pills for every enabled
//! filter. The filter state is a plain `FilterSet` (see `models::library_merge`); this type only
//! draws it and reports changes.
//!
//! The filters are not persisted: a new GSettings key would panic at startup on installs whose
//! compiled schema is older than the binary.

use crate::models::library_merge::{category_label, FilterSet, PillKind, Source};
use gtk4::prelude::*;
use gtk4::{self, glib};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::{Rc, Weak};

const SOURCE_PREFIX: &str = "source:";
const CATEGORY_PREFIX: &str = "cat:";

fn source_key(source: Source) -> String {
    format!("{SOURCE_PREFIX}{}", source.key())
}

fn category_key(path: &str) -> String {
    format!("{CATEGORY_PREFIX}{path}")
}

pub struct FilterBar {
    list: gtk4::ListBox,
    pills: gtk4::FlowBox,
    state: RefCell<FilterSet>,
    /// Check button of every menu row, by key (`source:fab`, `cat:plugins/tools`).
    checks: RefCell<HashMap<String, gtk4::CheckButton>>,
    on_change: Box<dyn Fn()>,
    /// Set while the rows are rebuilt, so the toggled handlers stay quiet.
    rebuilding: Cell<bool>,
}

impl std::fmt::Debug for FilterBar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterBar")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl FilterBar {
    /// Attach the filter menu to `button` and the pill row to `pills`. `on_change` runs after
    /// every change of the enabled filters.
    pub fn new(
        button: &gtk4::MenuButton,
        pills: &gtk4::FlowBox,
        on_change: impl Fn() + 'static,
    ) -> Rc<Self> {
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.connect_row_activated(|_, row| {
            if let Some(check) = row.child().and_downcast::<gtk4::CheckButton>() {
                check.activate();
            }
        });
        let scroll = gtk4::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .min_content_width(260)
            .max_content_height(420)
            .propagate_natural_height(true)
            .build();
        let popover = gtk4::Popover::new();
        popover.set_child(Some(&scroll));
        button.set_popover(Some(&popover));

        pills.set_selection_mode(gtk4::SelectionMode::None);
        let bar = Rc::new(Self {
            list,
            pills: pills.clone(),
            state: RefCell::new(FilterSet::default()),
            checks: RefCell::new(HashMap::new()),
            on_change: Box::new(on_change),
            rebuilding: Cell::new(false),
        });
        bar.rebuild_rows(&BTreeMap::new());
        bar.rebuild_pills();
        bar
    }

    /// The currently enabled filters.
    pub fn filters(&self) -> FilterSet {
        self.state.borrow().clone()
    }

    /// Rebuild the category rows after new categories were found. Enabled filters are kept.
    pub fn set_categories(self: &Rc<Self>, hierarchy: &BTreeMap<String, BTreeSet<String>>) {
        self.rebuild_rows(hierarchy);
    }

    fn rebuild_rows(self: &Rc<Self>, hierarchy: &BTreeMap<String, BTreeSet<String>>) {
        self.rebuilding.set(true);
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        self.checks.borrow_mut().clear();

        for source in [Source::Fab, Source::Local] {
            let active = self.state.borrow().source_enabled(source);
            self.add_check_row(source_key(source), source.label(), 0, active);
        }
        if !hierarchy.is_empty() {
            let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
            self.list.append(&separator);
            if let Some(row) = self.list.last_child().and_downcast::<gtk4::ListBoxRow>() {
                row.set_activatable(false);
            }
        }
        for (top, subs) in hierarchy {
            let active = self.state.borrow().categories.contains(top);
            self.add_check_row(category_key(top), &category_label(top), 0, active);
            for sub in subs {
                let path = format!("{top}/{sub}");
                let active = self.state.borrow().categories.contains(&path);
                self.add_check_row(category_key(&path), &category_label(sub), 18, active);
            }
        }
        self.rebuilding.set(false);
    }

    fn add_check_row(self: &Rc<Self>, key: String, label: &str, indent: i32, active: bool) {
        let check = gtk4::CheckButton::with_label(label);
        check.set_active(active);
        check.set_margin_start(indent);
        check.set_margin_top(2);
        check.set_margin_bottom(2);
        let weak: Weak<Self> = Rc::downgrade(self);
        let row_key = key.clone();
        check.connect_toggled(move |c| {
            if let Some(bar) = weak.upgrade() {
                if !bar.rebuilding.get() {
                    bar.apply(&row_key, c.is_active());
                }
            }
        });
        self.list.append(&check);
        self.checks.borrow_mut().insert(key, check);
    }

    /// Enable or disable one filter, then redraw the pills and report the change.
    fn apply(self: &Rc<Self>, key: &str, on: bool) {
        {
            let mut state = self.state.borrow_mut();
            if let Some(source) = key.strip_prefix(SOURCE_PREFIX) {
                let source = if source == Source::Fab.key() {
                    Source::Fab
                } else {
                    Source::Local
                };
                state.set_source(source, on);
            } else if let Some(path) = key.strip_prefix(CATEGORY_PREFIX) {
                state.set_category(path, on);
            }
        }
        self.rebuild_pills();
        (self.on_change)();
    }

    /// Switch a filter off (the pill's close button): through its menu row when there is one, so
    /// the check mark follows.
    fn disable(self: &Rc<Self>, key: &str) {
        let check = self.checks.borrow().get(key).cloned();
        match check {
            Some(check) if check.is_active() => check.set_active(false),
            _ => self.apply(key, false),
        }
    }

    fn rebuild_pills(self: &Rc<Self>) {
        while let Some(child) = self.pills.first_child() {
            self.pills.remove(&child);
        }
        // (key, text) of every pill; the borrow of the state ends before widgets are built.
        let pills: Vec<(String, String)> = self
            .state
            .borrow()
            .pills()
            .into_iter()
            .map(|p| match p {
                PillKind::Source(s) => (source_key(s), s.label().to_string()),
                PillKind::Category(c) => (category_key(c), category_label(c)),
            })
            .collect();
        for (key, text) in pills {
            self.pills.append(&self.build_pill(key, &text));
        }
        self.pills.set_visible(self.pills.first_child().is_some());
    }

    fn build_pill(self: &Rc<Self>, key: String, text: &str) -> gtk4::Box {
        let pill = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        pill.add_css_class("filter-pill");
        let label = gtk4::Label::new(Some(text));
        pill.append(&label);
        let close = gtk4::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("circular");
        close.set_valign(gtk4::Align::Center);
        close.set_tooltip_text(Some(&format!("Remove the {text} filter")));
        let weak: Weak<Self> = Rc::downgrade(self);
        close.connect_clicked(move |_| {
            if let Some(bar) = weak.upgrade() {
                // Rebuilding the pills inside the click handler of one of them is not safe:
                // finish the click first.
                let key = key.clone();
                glib::idle_add_local_once(move || bar.disable(&key));
            }
        });
        pill.append(&close);
        pill
    }
}
