//! Picker for the individual assets (packages) of a local library zip.
//!
//! The index of the pack (`ue_asset_library::load_or_build_index`) is read on a worker thread.
//! Packages are listed flat, sorted by game path, with a lazily loaded thumbnail per visible row
//! (`read_thumbnail` on a small thread pool, decoded textures cached in memory). Ticking an asset
//! auto-ticks, greyed, everything it needs (`closure`); the footer summarises the choice. The
//! result is a [`SubsetChoice`] that the import dialogs turn into a `Selection`.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib::clone;
use gtk4::{self, gdk, glib, CompositeTemplate};
use log::{debug, warn};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use ue_asset_library::{AssetIndex, ImportReport, LibraryEntry, Selection};

type SelectCallback = Rc<dyn Fn(SubsetChoice)>;

const ALL_CLASSES: &str = "All classes";
const UNKNOWN_CLASS: &str = "(unknown)";
const THUMBNAIL_THREADS: usize = 3;
const MAX_LISTED_EXTERNAL: usize = 5;

/// What the user picked: the packages ticked by hand (the rest that is needed follows from the
/// dependency closure at import time).
#[derive(Debug, Clone)]
pub struct SubsetChoice {
    pub index: Arc<AssetIndex>,
    /// Game paths ticked by the user.
    pub game_paths: Vec<String>,
    /// Packages that are only in the import because something ticked needs them.
    pub required: usize,
}

impl SubsetChoice {
    pub fn selection(&self) -> Selection {
        Selection {
            game_paths: self.game_paths.clone(),
            include_deps: true,
        }
    }

    /// "Importing 3 of 21 assets (+5 required)"
    pub fn label(&self) -> String {
        let mut text = format!(
            "Importing {} of {} assets",
            self.game_paths.len(),
            self.index.packages.len()
        );
        if self.required > 0 {
            text.push_str(&format!(" (+{} required)", self.required));
        }
        text
    }
}

/// Result notification text of an import; mentions what the imported assets reference but the
/// pack does not contain.
pub fn import_result_text(
    name: &str,
    subset: Option<&SubsetChoice>,
    report: &ImportReport,
) -> String {
    let mut text = match subset {
        Some(s) => format!(
            "Imported {} of {} assets (+{} required) from \"{name}\": {} files written, {} skipped, {} backed up.",
            s.game_paths.len(),
            s.index.packages.len(),
            s.required,
            report.written.len(),
            report.skipped.len(),
            report.backed_up.len()
        ),
        None => format!(
            "Imported \"{name}\": {} files written, {} skipped, {} backed up.",
            report.written.len(),
            report.skipped.len(),
            report.backed_up.len()
        ),
    };
    for w in &report.warnings {
        text.push_str(&format!(" Warning: {w}"));
    }
    if !report.external_missing.is_empty() {
        let listed: Vec<&str> = report
            .external_missing
            .iter()
            .take(MAX_LISTED_EXTERNAL)
            .map(String::as_str)
            .collect();
        text.push_str(&format!(
            " Warning: {} referenced packages are not part of this pack: {}{}",
            report.external_missing.len(),
            listed.join(", "),
            if report.external_missing.len() > MAX_LISTED_EXTERNAL {
                ", ..."
            } else {
                "."
            }
        ));
    }
    text
}

pub enum ThumbResult {
    /// The row scrolled away before the job started.
    Skipped,
    Done(Option<Vec<u8>>),
}

/// Messages from the worker threads back to the dialog.
pub enum Msg {
    Index(Result<AssetIndex, String>),
    Thumb(String, ThumbResult),
}

/// Derived from the ticked set after every change.
#[derive(Default)]
pub struct Derived {
    /// Per package index: the ticked package (index) it is needed by; `None` for ticked
    /// packages and packages that are not needed.
    pub required_by: Vec<Option<usize>>,
    pub required: usize,
    pub bytes: u64,
}

pub mod imp {
    use super::*;
    use adw::subclass::dialog::AdwDialogImpl;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/asset_picker.ui")]
    pub struct EpicAssetPicker {
        pub entry: RefCell<Option<LibraryEntry>>,
        pub index: RefCell<Option<Arc<AssetIndex>>>,
        pub positions: RefCell<HashMap<String, usize>>,
        pub checked: RefCell<BTreeSet<String>>,
        pub derived: RefCell<Derived>,
        /// Per package index: indices of the packages it needs (built once per index).
        pub adjacency: RefCell<Vec<Vec<usize>>>,
        /// Class names behind the dropdown entries after "All classes".
        pub classes: RefCell<Vec<String>>,
        pub textures: RefCell<HashMap<String, Option<gdk::Texture>>>,
        pub pending: RefCell<HashSet<String>>,
        /// Game paths whose rows are currently bound (jobs for others are skipped).
        pub wanted: Arc<Mutex<HashSet<String>>>,
        pub bound: RefCell<Vec<glib::WeakRef<gtk4::ListItem>>>,
        pub refreshing: Cell<bool>,
        pub model: gtk4::StringList,
        pub filter: gtk4::CustomFilter,
        pub filtered: gtk4::FilterListModel,
        pub on_select: RefCell<Option<SelectCallback>>,
        pub pool: threadpool::ThreadPool,
        pub sender: async_channel::Sender<Msg>,
        pub receiver: RefCell<Option<async_channel::Receiver<Msg>>>,
        #[template_child]
        pub stack: TemplateChild<gtk4::Stack>,
        #[template_child]
        pub search_entry: TemplateChild<gtk4::SearchEntry>,
        #[template_child]
        pub class_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub select_shown_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub clear_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub list_view: TemplateChild<gtk4::ListView>,
        #[template_child]
        pub summary_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub footer_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub warning_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub error_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub cancel_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub import_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicAssetPicker {
        const NAME: &'static str = "EpicAssetPicker";
        type Type = super::EpicAssetPicker;
        type ParentType = adw::Dialog;

        fn new() -> Self {
            let (sender, receiver) = async_channel::unbounded();
            let model = gtk4::StringList::new(&[]);
            let filter = gtk4::CustomFilter::new(|_| true);
            let filtered = gtk4::FilterListModel::new(Some(model.clone()), Some(filter.clone()));
            Self {
                entry: RefCell::new(None),
                index: RefCell::new(None),
                positions: RefCell::new(HashMap::new()),
                checked: RefCell::new(BTreeSet::new()),
                derived: RefCell::new(Derived::default()),
                adjacency: RefCell::new(Vec::new()),
                classes: RefCell::new(Vec::new()),
                textures: RefCell::new(HashMap::new()),
                pending: RefCell::new(HashSet::new()),
                wanted: Arc::new(Mutex::new(HashSet::new())),
                bound: RefCell::new(Vec::new()),
                refreshing: Cell::new(false),
                model,
                filter,
                filtered,
                on_select: RefCell::new(None),
                pool: threadpool::ThreadPool::new(THUMBNAIL_THREADS),
                sender,
                receiver: RefCell::new(Some(receiver)),
                stack: TemplateChild::default(),
                search_entry: TemplateChild::default(),
                class_dropdown: TemplateChild::default(),
                select_shown_button: TemplateChild::default(),
                clear_button: TemplateChild::default(),
                list_view: TemplateChild::default(),
                summary_label: TemplateChild::default(),
                footer_label: TemplateChild::default(),
                warning_banner: TemplateChild::default(),
                error_page: TemplateChild::default(),
                cancel_button: TemplateChild::default(),
                import_button: TemplateChild::default(),
            }
        }

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for EpicAssetPicker {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_list();
            obj.setup_events();
            obj.setup_receiver();
        }
    }

    impl WidgetImpl for EpicAssetPicker {}
    impl AdwDialogImpl for EpicAssetPicker {}
}

glib::wrapper! {
    pub struct EpicAssetPicker(ObjectSubclass<imp::EpicAssetPicker>)
        @extends gtk4::Widget, adw::Dialog,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::ShortcutManager;
}

/// "12.3 MB" / "840 KB"
fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", (b / 1024.0).ceil())
    }
}

fn fallback_icon(class: Option<&str>) -> &'static str {
    let class = class.unwrap_or_default();
    if class.starts_with("Texture") || class.contains("Render") {
        "image-x-generic-symbolic"
    } else if class.starts_with("Sound") || class.contains("Audio") {
        "audio-x-generic-symbolic"
    } else if class == "World" || class == "Level" {
        "mark-location-symbolic"
    } else {
        "package-x-generic-symbolic"
    }
}

fn game_path_parts(game_path: &str) -> (&str, &str) {
    match game_path.rfind('/') {
        Some(i) => (&game_path[..i], &game_path[i + 1..]),
        None => ("", game_path),
    }
}

type RowParts = (
    gtk4::CheckButton,
    gtk4::Image,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
);

fn row_parts(item: &gtk4::ListItem) -> Option<RowParts> {
    let row = item.child().and_downcast::<gtk4::Box>()?;
    let check = row.first_child().and_downcast::<gtk4::CheckButton>()?;
    let image = check.next_sibling().and_downcast::<gtk4::Image>()?;
    let texts = image.next_sibling().and_downcast::<gtk4::Box>()?;
    let size = texts.next_sibling().and_downcast::<gtk4::Label>()?;
    let title = texts.first_child().and_downcast::<gtk4::Label>()?;
    let subtitle = texts.last_child().and_downcast::<gtk4::Label>()?;
    Some((check, image, title, subtitle, size))
}

fn item_path(item: &gtk4::ListItem) -> Option<String> {
    item.item()
        .and_downcast::<gtk4::StringObject>()
        .map(|s| s.string().to_string())
}

impl EpicAssetPicker {
    /// Open the picker for a local zip. `cached` skips reading the index again; `preselected`
    /// are the game paths ticked from the start. `on_select` gets the choice when the user
    /// confirms; the dialog closes itself.
    pub fn present_for(
        parent: &impl IsA<gtk4::Widget>,
        entry: LibraryEntry,
        cached: Option<Arc<AssetIndex>>,
        preselected: &[String],
        on_select: impl Fn(SubsetChoice) + 'static,
    ) -> Self {
        let dialog: Self = glib::Object::new();
        let self_ = dialog.imp();
        dialog.set_title(&format!("Choose assets - {}", entry.sidecar.display_name));
        self_.checked.replace(preselected.iter().cloned().collect());
        self_.on_select.replace(Some(Rc::new(on_select)));
        self_.entry.replace(Some(entry.clone()));
        match cached {
            Some(index) => dialog.set_index(index),
            None => {
                self_.stack.set_visible_child_name("loading");
                let sender = self_.sender.clone();
                std::thread::spawn(move || {
                    let result =
                        ue_asset_library::load_or_build_index(&entry).map_err(|e| e.to_string());
                    let _ = sender.send_blocking(Msg::Index(result));
                });
            }
        }
        adw::prelude::AdwDialogExt::present(&dialog, Some(parent));
        dialog
    }

    fn setup_list(&self) {
        let self_ = self.imp();
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
                row.set_margin_top(6);
                row.set_margin_bottom(6);
                row.set_margin_start(8);
                row.set_margin_end(8);
                let check = gtk4::CheckButton::new();
                check.set_valign(gtk4::Align::Center);
                let image = gtk4::Image::from_icon_name("package-x-generic-symbolic");
                image.set_pixel_size(48);
                let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
                texts.set_hexpand(true);
                texts.set_valign(gtk4::Align::Center);
                let title = gtk4::Label::new(None);
                title.set_xalign(0.0);
                title.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
                title.add_css_class("heading");
                let subtitle = gtk4::Label::new(None);
                subtitle.set_xalign(0.0);
                subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
                subtitle.add_css_class("caption");
                subtitle.add_css_class("dim-label");
                texts.append(&title);
                texts.append(&subtitle);
                let size = gtk4::Label::new(None);
                size.set_valign(gtk4::Align::Center);
                size.add_css_class("caption");
                size.add_css_class("dim-label");
                row.append(&check);
                row.append(&image);
                row.append(&texts);
                row.append(&size);
                item.set_child(Some(&row));

                let weak_item = item.downgrade();
                check.connect_toggled(clone!(
                    #[weak]
                    dialog,
                    move |check| {
                        if dialog.imp().refreshing.get() {
                            return;
                        }
                        if let Some(path) = weak_item.upgrade().as_ref().and_then(item_path) {
                            dialog.on_toggled(&path, check.is_active());
                        }
                    }
                ));
            }
        ));
        factory.connect_bind(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                dialog.imp().bound.borrow_mut().push(item.downgrade());
                dialog.refresh_item(item);
            }
        ));
        factory.connect_unbind(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let self_ = dialog.imp();
                self_
                    .bound
                    .borrow_mut()
                    .retain(|w| w.upgrade().is_some_and(|i| &i != item));
                if let Some(path) = item_path(item) {
                    if let Ok(mut wanted) = self_.wanted.lock() {
                        wanted.remove(&path);
                    }
                }
            }
        ));

        self_.filter.set_filter_func(clone!(
            #[weak(rename_to=dialog)]
            self,
            #[upgrade_or_default]
            move |obj| {
                obj.downcast_ref::<gtk4::StringObject>()
                    .is_none_or(|s| dialog.matches(&s.string()))
            }
        ));
        let selection = gtk4::NoSelection::new(Some(self_.filtered.clone()));
        self_.list_view.set_model(Some(&selection));
        self_.list_view.set_factory(Some(&factory));
    }

    fn setup_events(&self) {
        let self_ = self.imp();
        self_.cancel_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                dialog.force_close();
            }
        ));
        self_.import_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.confirm()
        ));
        self_.search_entry.connect_search_changed(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.imp().filter.changed(gtk4::FilterChange::Different)
        ));
        self_.class_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.imp().filter.changed(gtk4::FilterChange::Different)
        ));
        self_.select_shown_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                let self_ = dialog.imp();
                {
                    let mut checked = self_.checked.borrow_mut();
                    for i in 0..self_.filtered.n_items() {
                        if let Some(s) = self_.filtered.item(i).and_downcast::<gtk4::StringObject>()
                        {
                            checked.insert(s.string().to_string());
                        }
                    }
                }
                dialog.after_change();
            }
        ));
        self_.clear_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                dialog.imp().checked.borrow_mut().clear();
                dialog.after_change();
            }
        ));
    }

    fn setup_receiver(&self) {
        let self_ = self.imp();
        let Some(receiver) = self_.receiver.borrow_mut().take() else {
            return;
        };
        glib::spawn_future_local(clone!(
            #[weak(rename_to=dialog)]
            self,
            async move {
                while let Ok(msg) = receiver.recv().await {
                    dialog.handle_msg(msg);
                }
            }
        ));
    }

    fn handle_msg(&self, msg: Msg) {
        let self_ = self.imp();
        match msg {
            Msg::Index(Ok(index)) => self.set_index(Arc::new(index)),
            Msg::Index(Err(e)) => {
                warn!("Unable to read the asset index: {e}");
                self.show_error("Unable to list the assets", &e);
            }
            Msg::Thumb(path, result) => {
                self_.pending.borrow_mut().remove(&path);
                if let ThumbResult::Done(bytes) = result {
                    let texture = bytes.and_then(|b| {
                        gdk::Texture::from_bytes(&glib::Bytes::from_owned(b))
                            .map_err(|e| debug!("Thumbnail of {path} is not decodable: {e}"))
                            .ok()
                    });
                    self_.textures.borrow_mut().insert(path, texture);
                }
                self.refresh_bound();
            }
        }
    }

    fn show_error(&self, title: &str, description: &str) {
        let self_ = self.imp();
        self_.error_page.set_title(title);
        self_.error_page.set_description(Some(description));
        self_.stack.set_visible_child_name("error");
        self_.import_button.set_sensitive(false);
    }

    fn set_index(&self, index: Arc<AssetIndex>) {
        let self_ = self.imp();
        if index.packages.is_empty() {
            self_.index.replace(Some(index));
            self.show_error(
                "No assets found",
                "This pack has no packages that can be picked one by one.",
            );
            return;
        }
        let positions: HashMap<String, usize> = index
            .packages
            .iter()
            .enumerate()
            .map(|(i, p)| (p.game_path.clone(), i))
            .collect();
        let classes: BTreeSet<String> = index
            .packages
            .iter()
            .map(|p| p.class.clone().unwrap_or_else(|| UNKNOWN_CLASS.to_string()))
            .collect();
        let classes: Vec<String> = classes.into_iter().collect();
        let mut labels: Vec<&str> = vec![ALL_CLASSES];
        labels.extend(classes.iter().map(String::as_str));
        self_
            .class_dropdown
            .set_model(Some(&gtk4::StringList::new(&labels)));
        self_.classes.replace(classes);

        self_
            .checked
            .borrow_mut()
            .retain(|p| positions.contains_key(p));
        let adjacency: Vec<Vec<usize>> = index
            .packages
            .iter()
            .map(|p| {
                p.deps
                    .iter()
                    .filter_map(|d| positions.get(d).copied())
                    .collect()
            })
            .collect();
        self_.adjacency.replace(adjacency);
        self_.positions.replace(positions);

        let unreadable = index
            .packages
            .iter()
            .filter(|p| p.header_unreadable)
            .count();
        if unreadable > 0 {
            self_.warning_banner.set_title(&format!(
                "{unreadable} assets could not be read completely; what they need is unknown, so the import may miss files."
            ));
            self_.warning_banner.set_revealed(true);
        }
        if !index.other_files.is_empty() {
            self_.footer_label.set_label(&format!(
                "{} other files (config, media) are not part of a selection.",
                index.other_files.len()
            ));
            self_.footer_label.set_visible(true);
        }
        let paths: Vec<&str> = index
            .packages
            .iter()
            .map(|p| p.game_path.as_str())
            .collect();
        self_.index.replace(Some(index.clone()));
        let old = self_.model.n_items();
        self_.model.splice(0, old, &paths);
        self_.stack.set_visible_child_name("list");
        self.recompute();
        self.update_summary();
        self.refresh_bound();
    }

    fn matches(&self, game_path: &str) -> bool {
        let self_ = self.imp();
        let search = self_.search_entry.text().to_lowercase();
        if !search.is_empty() {
            let lower = game_path.to_lowercase();
            if !search.split_whitespace().all(|t| lower.contains(t)) {
                return false;
            }
        }
        let selected = self_.class_dropdown.selected();
        if selected != gtk4::INVALID_LIST_POSITION && selected > 0 {
            let classes = self_.classes.borrow();
            let Some(wanted) = classes.get(selected as usize - 1) else {
                return true;
            };
            let index = self_.index.borrow();
            let positions = self_.positions.borrow();
            let class = index
                .as_ref()
                .zip(positions.get(game_path))
                .map(|(index, &i)| {
                    index.packages[i]
                        .class
                        .clone()
                        .unwrap_or_else(|| UNKNOWN_CLASS.to_string())
                });
            return class.as_deref() == Some(wanted.as_str());
        }
        true
    }

    fn on_toggled(&self, path: &str, active: bool) {
        {
            let mut checked = self.imp().checked.borrow_mut();
            if active {
                checked.insert(path.to_string());
            } else {
                checked.remove(path);
            }
        }
        self.after_change();
    }

    fn after_change(&self) {
        self.recompute();
        self.update_summary();
        self.refresh_bound();
    }

    /// Dependency closure of the ticked packages: counts, size and who needs what.
    fn recompute(&self) {
        let self_ = self.imp();
        let Some(index) = self_.index.borrow().clone() else {
            return;
        };
        // The same transitive closure as `ue_asset_library::closure`, over the adjacency lists
        // built once in `set_index`: linear in the packages reached, so it stays on the main
        // thread. Breadth first from every ticked package, the first discoverer is "required by".
        let adjacency = self_.adjacency.borrow();
        let n = adjacency.len();
        let mut derived = Derived {
            required_by: vec![None; n],
            ..Derived::default()
        };
        let mut seen = vec![false; n];
        let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
        {
            let positions = self_.positions.borrow();
            for path in self_.checked.borrow().iter() {
                if let Some(&i) = positions.get(path) {
                    if !seen[i] {
                        seen[i] = true;
                        derived.bytes += index.packages[i].size;
                        queue.push_back((i, i));
                    }
                }
            }
        }
        while let Some((package, root)) = queue.pop_front() {
            for &dep in &adjacency[package] {
                if !seen[dep] {
                    seen[dep] = true;
                    derived.required += 1;
                    derived.bytes += index.packages[dep].size;
                    derived.required_by[dep] = Some(root);
                    queue.push_back((dep, root));
                }
            }
        }
        drop(adjacency);
        self_.derived.replace(derived);
    }

    fn update_summary(&self) {
        let self_ = self.imp();
        let selected = self_.checked.borrow().len();
        let derived = self_.derived.borrow();
        self_.import_button.set_sensitive(selected > 0);
        self_.summary_label.set_label(&if selected == 0 {
            "Nothing selected".to_string()
        } else {
            format!(
                "{selected} selected + {} required, {}",
                derived.required,
                human_size(derived.bytes)
            )
        });
    }

    fn refresh_bound(&self) {
        let items: Vec<gtk4::ListItem> = self
            .imp()
            .bound
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect();
        for item in items {
            self.refresh_item(&item);
        }
    }

    fn refresh_item(&self, item: &gtk4::ListItem) {
        let self_ = self.imp();
        let (Some(path), Some((check, image, title, subtitle, size))) =
            (item_path(item), row_parts(item))
        else {
            return;
        };
        let Some(index) = self_.index.borrow().clone() else {
            return;
        };
        let Some(&pos) = self_.positions.borrow().get(&path) else {
            return;
        };
        let info = &index.packages[pos];
        let (folder, name) = game_path_parts(&path);
        title.set_label(name);
        size.set_label(&human_size(info.size));

        let ticked = self_.checked.borrow().contains(&path);
        let required_by = self_
            .derived
            .borrow()
            .required_by
            .get(pos)
            .copied()
            .flatten();
        let locked = !ticked && required_by.is_some();
        self_.refreshing.set(true);
        check.set_active(ticked || locked);
        check.set_sensitive(!locked);
        self_.refreshing.set(false);
        item.set_activatable(false);

        let mut detail = String::new();
        if let (true, Some(by)) = (locked, required_by) {
            detail.push_str(&format!(
                "Required by {}",
                game_path_parts(&index.packages[by].game_path).1
            ));
            detail.push_str("  |  ");
        }
        detail.push_str(info.class.as_deref().unwrap_or(UNKNOWN_CLASS));
        detail.push_str("  |  ");
        detail.push_str(folder);
        if info.header_unreadable {
            detail.push_str("  |  header unreadable");
        }
        subtitle.set_label(&detail);
        row_style(&title, locked);

        // Thumbnail: cached texture, else a class icon while it loads (or when there is none)
        let cached = self_.textures.borrow().get(&path).cloned();
        match cached {
            Some(Some(texture)) => image.set_paintable(Some(&texture)),
            Some(None) => image.set_icon_name(Some(fallback_icon(info.class.as_deref()))),
            None => {
                image.set_icon_name(Some(fallback_icon(info.class.as_deref())));
                if info.has_thumbnail {
                    self.request_thumbnail(&path);
                }
            }
        }
    }

    fn request_thumbnail(&self, path: &str) {
        let self_ = self.imp();
        if let Ok(mut wanted) = self_.wanted.lock() {
            wanted.insert(path.to_string());
        }
        if !self_.pending.borrow_mut().insert(path.to_string()) {
            return;
        }
        let Some(entry) = self_.entry.borrow().clone() else {
            return;
        };
        let wanted = self_.wanted.clone();
        let sender = self_.sender.clone();
        let path = path.to_string();
        self_.pool.execute(move || {
            let still_wanted = wanted.lock().map(|w| w.contains(&path)).unwrap_or(true);
            let result = if still_wanted {
                match ue_asset_library::read_thumbnail(&entry, &path) {
                    Ok(bytes) => ThumbResult::Done(bytes),
                    Err(e) => {
                        debug!("No thumbnail for {path}: {e}");
                        ThumbResult::Done(None)
                    }
                }
            } else {
                ThumbResult::Skipped
            };
            let _ = sender.send_blocking(Msg::Thumb(path, result));
        });
    }

    fn confirm(&self) {
        let self_ = self.imp();
        let Some(index) = self_.index.borrow().clone() else {
            return;
        };
        let game_paths: Vec<String> = self_.checked.borrow().iter().cloned().collect();
        if game_paths.is_empty() {
            return;
        }
        let choice = SubsetChoice {
            index,
            game_paths,
            required: self_.derived.borrow().required,
        };
        let callback = self_.on_select.borrow().clone();
        if let Some(callback) = callback {
            callback(choice);
        }
        self.force_close();
    }
}

/// Locked (required, not ticked) rows are shown dimmed.
fn row_style(title: &gtk4::Label, locked: bool) {
    if locked {
        title.add_css_class("dim-label");
    } else {
        title.remove_css_class("dim-label");
    }
}
