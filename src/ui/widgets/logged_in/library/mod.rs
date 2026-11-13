use crate::models::asset_data::{AssetData, AssetType, LocalRef};
use crate::models::fab_overlay::FabOverlay;
use crate::models::library_merge::{self, ItemIdentity, MatchKeys};
use crate::tools::asset_info::Search;
use crate::ui::widgets::logged_in::refresh::Refresh;
use asset::EpicAsset;
use glib::clone;
use gtk4::{self, prelude::*, CustomSorter};
use gtk4::{gio, glib, subclass::prelude::*, CompositeTemplate};
use gtk_macros::action;
use log::{debug, error, trace, warn};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use ue_asset_library::LibraryEntry;

mod actions;
pub mod asset;
pub mod asset_detail;
pub mod filter_bar;
pub mod image_stack;
pub mod sidebar;

pub mod imp {
    use super::*;
    use crate::config;
    use crate::ui::widgets::download_manager::EpicDownloadManager;
    use crate::window::EpicAssetManagerWindow;
    use gtk4::gio;
    use gtk4::gio::ListStore;
    use gtk4::glib::{Object, ParamSpec, ParamSpecString, ParamSpecUInt};
    use once_cell::sync::OnceCell;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    use threadpool::ThreadPool;

    #[derive(Debug, CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/library.ui")]
    pub struct EpicLibraryBox {
        pub details: OnceCell<asset_detail::EpicAssetDetails>,
        pub sidebar: OnceCell<sidebar::EpicSidebar>,
        #[template_child]
        pub asset_grid: TemplateChild<gtk4::GridView>,
        #[template_child]
        pub asset_search: TemplateChild<gtk4::SearchEntry>,
        #[template_child]
        pub filter_button: TemplateChild<gtk4::MenuButton>,
        #[template_child]
        pub filter_pills: TemplateChild<gtk4::FlowBox>,
        #[template_child]
        pub downloaded_filter: TemplateChild<gtk4::ToggleButton>,
        #[template_child]
        pub favorites_filter: TemplateChild<gtk4::ToggleButton>,
        #[template_child]
        pub select_order_by: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub order: TemplateChild<gtk4::Button>,
        #[template_child]
        pub count_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub refresh_progress: TemplateChild<gtk4::ProgressBar>,
        pub filter: RefCell<Option<String>>,
        pub search: RefCell<Option<String>>,
        pub actions: gio::SimpleActionGroup,
        pub window: OnceCell<EpicAssetManagerWindow>,
        pub download_manager: OnceCell<EpicDownloadManager>,
        pub filter_model: gtk4::FilterListModel,
        pub sorter_model: gtk4::SortListModel,
        pub grid_model: ListStore,
        pub order_by_ids: RefCell<Vec<String>>,
        pub category_hierarchy: RefCell<BTreeMap<String, BTreeSet<String>>>,
        /// The filter menu and pills (set once in `setup_widgets`).
        pub filter_bar: OnceCell<std::rc::Rc<filter_bar::FilterBar>>,
        pub categories_dirty: std::cell::Cell<bool>,
        pub loaded_assets: RefCell<HashMap<String, egs_api::api::types::asset_info::AssetInfo>>,
        /// Every Epic catalog id (twins included) to its library entry.
        pub loaded_data: RefCell<HashMap<String, crate::models::asset_data::AssetData>>,
        /// Epic library entries by cleaned name, to find the twin of a new item.
        pub name_groups: RefCell<HashMap<String, Vec<AssetData>>>,
        /// Fab listing id to the library entry that has it.
        pub listing_index: RefCell<HashMap<String, AssetData>>,
        /// Fab library thumbnails by listing id, for entries whose Epic item has no picture.
        pub fab_thumbnails: RefCell<HashMap<String, gtk4::gdk::Texture>>,
        /// Every local library zip, with its identity keys.
        pub local_entries: RefCell<Vec<(LocalRef, MatchKeys)>>,
        /// Entries that exist only in the local library, by group key.
        pub local_only: RefCell<HashMap<String, AssetData>>,
        pub asset_product_names: RefCell<HashMap<String, String>>,
        pub asset_load_pool: ThreadPool,
        pub image_load_pool: ThreadPool,
        pub assets_pending: std::sync::RwLock<Vec<Object>>,
        pub categories: RefCell<HashSet<String>>,
        pub settings: gio::Settings,
        loading: RefCell<u32>,
        loaded: RefCell<u32>,
        item: RefCell<Option<String>>,
        product: RefCell<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicLibraryBox {
        const NAME: &'static str = "EpicLibraryBox";
        type Type = super::EpicLibraryBox;
        type ParentType = gtk4::Box;

        fn new() -> Self {
            Self {
                details: OnceCell::new(),
                sidebar: OnceCell::new(),
                asset_grid: TemplateChild::default(),
                asset_search: TemplateChild::default(),
                filter_button: TemplateChild::default(),
                filter_pills: TemplateChild::default(),
                downloaded_filter: TemplateChild::default(),
                favorites_filter: TemplateChild::default(),
                select_order_by: TemplateChild::default(),
                order: TemplateChild::default(),
                count_label: TemplateChild::default(),
                refresh_progress: TemplateChild::default(),
                filter: RefCell::new(None),
                search: RefCell::new(None),
                actions: gio::SimpleActionGroup::new(),
                window: OnceCell::new(),
                download_manager: OnceCell::new(),
                filter_model: gtk4::FilterListModel::new(
                    gio::ListModel::NONE.cloned(),
                    gtk4::Filter::NONE.cloned(),
                ),
                sorter_model: gtk4::SortListModel::new(
                    gio::ListModel::NONE.cloned(),
                    gtk4::Sorter::NONE.cloned(),
                ),
                grid_model: gio::ListStore::new::<crate::models::asset_data::AssetData>(),
                order_by_ids: RefCell::new(Vec::new()),
                category_hierarchy: RefCell::new(BTreeMap::new()),
                filter_bar: OnceCell::new(),
                categories_dirty: std::cell::Cell::new(false),
                loaded_assets: RefCell::new(HashMap::new()),
                loaded_data: RefCell::new(HashMap::new()),
                name_groups: RefCell::new(HashMap::new()),
                listing_index: RefCell::new(HashMap::new()),
                fab_thumbnails: RefCell::new(HashMap::new()),
                local_entries: RefCell::new(Vec::new()),
                local_only: RefCell::new(HashMap::new()),
                asset_product_names: RefCell::new(HashMap::new()),
                asset_load_pool: ThreadPool::with_name("Asset Load Pool".to_string(), 5),
                image_load_pool: ThreadPool::with_name("Image Load Pool".to_string(), 5),
                assets_pending: std::sync::RwLock::new(vec![]),
                categories: RefCell::new(HashSet::new()),
                settings: gio::Settings::new(config::APP_ID),
                loading: RefCell::new(0),
                loaded: RefCell::new(0),
                item: RefCell::new(None),
                product: RefCell::new(None),
            }
        }

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        // You must call `Widget`'s `init_template()` within `instance_init()`.
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for EpicLibraryBox {
        fn properties() -> &'static [ParamSpec] {
            use once_cell::sync::Lazy;

            static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
                vec![
                    ParamSpecUInt::builder("to-load")
                        .minimum(0)
                        .default_value(0)
                        .build(),
                    ParamSpecUInt::builder("loaded")
                        .minimum(0)
                        .default_value(0)
                        .build(),
                    ParamSpecString::builder("filter").build(),
                    ParamSpecString::builder("search").build(),
                    ParamSpecString::builder("item").build(),
                    ParamSpecString::builder("product").build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &ParamSpec) {
            match pspec.name() {
                "to-load" => {
                    self.loading.replace(value.get().unwrap());
                }
                "loaded" => {
                    self.loaded.replace(value.get().unwrap());
                }
                "filter" => {
                    let filter: Option<String> = value.get().unwrap();

                    self.filter.replace(filter.filter(|f| !f.is_empty()));
                    self.obj().apply_filter();
                }
                "search" => {
                    let search: Option<String> = value.get().unwrap();
                    self.search.replace(search.filter(|f| !f.is_empty()));
                    self.obj().apply_filter();
                }
                "item" => {
                    let item = value.get().unwrap();
                    self.product.replace(None);
                    self.item.replace(item);
                    self.obj().open_asset();
                }
                "product" => {
                    let product = value.get().unwrap();
                    self.item.replace(None);
                    self.product.replace(product);
                    self.obj().open_asset();
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &ParamSpec) -> glib::Value {
            match pspec.name() {
                "to-load" => self.loading.borrow().to_value(),
                "loaded" => self.loaded.borrow().to_value(),
                "filter" => self.filter.borrow().to_value(),
                "search" => self.search.borrow().to_value(),
                "item" => self.item.borrow().to_value(),
                "product" => self.product.borrow().to_value(),
                _ => unimplemented!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.bind_properties();
            obj.setup_actions();
            obj.setup_widgets();
        }
    }

    impl WidgetImpl for EpicLibraryBox {}
    impl BoxImpl for EpicLibraryBox {}
}

glib::wrapper! {
    pub struct EpicLibraryBox(ObjectSubclass<imp::EpicLibraryBox>)
        @extends gtk4::Widget, gtk4::Box,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

impl Default for EpicLibraryBox {
    fn default() -> Self {
        Self::new()
    }
}

impl EpicLibraryBox {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_download_manager(
        &self,
        dm: &crate::ui::widgets::download_manager::EpicDownloadManager,
    ) {
        let self_ = self.imp();
        // Do not run this twice
        if self_.download_manager.get().is_some() {
            return;
        }

        self_.download_manager.set(dm.clone()).unwrap();
        // Note: details panel download manager is now set at logged_in level
    }

    pub fn set_details(&self, details: &asset_detail::EpicAssetDetails) {
        let self_ = self.imp();
        if self_.details.get().is_some() {
            return;
        }
        self_.details.set(details.clone()).unwrap();
    }

    pub fn set_sidebar(&self, sidebar: &sidebar::EpicSidebar) {
        let self_ = self.imp();
        if self_.sidebar.get().is_some() {
            return;
        }
        self_.sidebar.set(sidebar.clone()).unwrap();
        // Connect sidebar to this library
        sidebar.set_logged_in(self);
        // Set window on sidebar if we have it
        if let Some(window) = self_.window.get() {
            sidebar.set_window(window);
        }
    }

    pub fn set_window(&self, window: &crate::window::EpicAssetManagerWindow) {
        let self_ = self.imp();
        // Do not run this twice
        if self_.window.get().is_some() {
            return;
        }

        self_.window.set(window.clone()).unwrap();
        // Note: details panel window is now set at logged_in level
        if let Some(sidebar) = self_.sidebar.get() {
            sidebar.set_window(&window.clone());
        }
        self_
            .asset_search
            .set_key_capture_widget(Some(&window.clone()));
        let factory = gtk4::SignalListItemFactory::new();
        // Create the children and connect signals (only once per widget)
        factory.connect_setup(clone!(
            #[weak(rename_to=library)]
            self,
            move |_factory, item| {
                trace!("Factory setup called - creating EpicAsset");

                let row = EpicAsset::new();
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                item.set_child(Some(&row));

                // Connect action button signals once during setup
                row.connect_local(
                    "download-requested",
                    false,
                    clone!(
                        #[weak]
                        library,
                        #[upgrade_or]
                        None,
                        move |values| {
                            trace!("Signal handler called for download-requested");
                            let asset = values[0].get::<EpicAsset>().unwrap();
                            library.handle_asset_action(&asset, "download");
                            None
                        }
                    ),
                );

                row.connect_local(
                    "add-to-project-requested",
                    false,
                    clone!(
                        #[weak]
                        library,
                        #[upgrade_or]
                        None,
                        move |values| {
                            let asset = values[0].get::<EpicAsset>().unwrap();
                            library.handle_asset_action(&asset, "add_to_project");
                            None
                        }
                    ),
                );

                row.connect_local(
                    "create-project-requested",
                    false,
                    clone!(
                        #[weak]
                        library,
                        #[upgrade_or]
                        None,
                        move |values| {
                            let asset = values[0].get::<EpicAsset>().unwrap();
                            library.handle_asset_action(&asset, "create_project");
                            None
                        }
                    ),
                );

                row.connect_local(
                    "tile-clicked",
                    false,
                    clone!(
                        #[weak]
                        library,
                        #[upgrade_or]
                        None,
                        move |values| {
                            let asset = values[0].get::<EpicAsset>().unwrap();
                            library.handle_asset_action(&asset, "show_details");
                            None
                        }
                    ),
                );
            }
        ));

        // Populate children
        factory.connect_bind(move |_, list_item| {
            let item = list_item.downcast_ref::<gtk4::ListItem>().unwrap();
            Self::populate_model(item);
        });

        self_.filter_model.set_model(Some(&self_.grid_model));
        self.update_count();
        self_.sorter_model.set_model(Some(&self_.filter_model));
        self_
            .sorter_model
            .set_sorter(Some(&Self::sorter("name", true)));
        // Use NoSelection to allow button clicks to work inside grid items
        let selection_model = gtk4::NoSelection::new(Some(self_.sorter_model.clone()));
        self_.asset_grid.set_model(Some(&selection_model));
        self_.asset_grid.set_factory(Some(&factory));

        self.fetch_assets();
        glib::timeout_add_seconds_local(
            15 * 60 + (rand::random::<u32>() % 15 * 60),
            clone!(
                #[weak(rename_to=obj)]
                self,
                #[upgrade_or_panic]
                move || {
                    debug!("Library timed refresh starting");
                    obj.run_refresh();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    fn handle_asset_action(&self, asset_widget: &EpicAsset, action: &str) {
        debug!("handle_asset_action called with action: {}", action);

        // Get the asset data from the widget
        let Some(data) = asset_widget.imp().data.borrow().clone() else {
            log::error!("No asset data in widget!");
            return;
        };
        log::info!("Got asset data with id: {}", data.id());
        // The entry's Epic item (the one with the thumbnail); entries that exist only in the
        // local library have none.
        let asset_info = data.epic_items().into_iter().next();

        match (action, asset_info) {
            ("create_project", Some(asset_info)) => {
                self.open_create_project_dialog(&asset_info);
            }
            ("download" | "add_to_project" | "create_project", _) => {
                // A local-only project has no marketplace flow: creating it is done on the
                // local library page.
                if !data.has_fab() && data.kind().is_some_and(|k| matches!(k, AssetType::Project)) {
                    self.show_local_entry(&data);
                } else {
                    // Ask for the project, source and version before downloading
                    self.open_download_dialog(&data);
                }
            }
            ("show_details", Some(asset_info)) => {
                // Open asset details panel
                self.show_asset_details(&asset_info);
            }
            ("show_details", None) => self.show_local_entry(&data),
            _ => {}
        }
    }

    /// Show a local-only entry on the local library page, where it can be matched, imported or
    /// turned into a project.
    fn show_local_entry(&self, data: &AssetData) {
        let Some(sha) = data
            .locals()
            .first()
            .map(|l| l.entry.sidecar.sha256.clone())
        else {
            return;
        };
        if let Some(window) = self.imp().window.get() {
            window.imp().logged_in_stack.show_local_entry(&sha);
        }
    }

    /// Ask which project, source (Fab release or local zip) and version to use.
    pub fn open_download_dialog(&self, data: &AssetData) {
        let self_ = self.imp();
        debug!("open_download_dialog called for: {}", data.name());

        match (self_.window.get(), self_.download_manager.get()) {
            (Some(window), Some(dm)) => {
                crate::ui::widgets::logged_in::download_dialog::EpicDownloadDialog::present_entry(
                    window, dm, data,
                );
            }
            _ => warn!("Window or download manager not available for the download dialog"),
        }
    }

    /// The marketplace items and local zips that belong to one asset, for the download dialog.
    /// `asset_ids` are Epic catalog ids to look the entry up by; `keys` find it by listing id or
    /// name when there is no id (the Fab page). Local project zips are not offered.
    pub fn dialog_parts(
        &self,
        asset_ids: &[String],
        keys: &MatchKeys,
    ) -> (
        Vec<egs_api::api::types::asset_info::AssetInfo>,
        Vec<LibraryEntry>,
    ) {
        let self_ = self.imp();
        let group = {
            let by_id = self_.loaded_data.borrow();
            asset_ids
                .iter()
                .find_map(|id| by_id.get(id).cloned())
                .or_else(|| {
                    let by_listing = self_.listing_index.borrow();
                    keys.listing_ids
                        .iter()
                        .find_map(|l| by_listing.get(l).cloned())
                })
                .or_else(|| {
                    // No listing id on either side: fall back to the cleaned name.
                    let groups = self_.name_groups.borrow();
                    keys.name_keys.iter().find_map(|k| {
                        groups
                            .get(k)
                            .and_then(|g| {
                                g.iter()
                                    .find(|d| library_merge::matches_local(&d.match_keys(), keys))
                            })
                            .cloned()
                    })
                })
        };
        let (epics, keys) = match &group {
            Some(g) => {
                let mut merged = g.match_keys();
                merged.listing_ids.extend(keys.listing_ids.iter().cloned());
                merged.name_keys.extend(keys.name_keys.iter().cloned());
                (g.epic_items(), merged)
            }
            None => (Vec::new(), keys.clone()),
        };
        let locals = self_
            .local_entries
            .borrow()
            .iter()
            .filter(|(l, k)| {
                l.entry.sidecar.kind != ue_asset_library::Kind::Project
                    && library_merge::matches_local(&keys, k)
            })
            .map(|(l, _)| l.entry.clone())
            .collect();
        (epics, locals)
    }

    fn show_asset_details(&self, asset_info: &egs_api::api::types::asset_info::AssetInfo) {
        let self_ = self.imp();

        if let Some(details) = self_.details.get() {
            details.set_asset(asset_info);
            details.set_property("expanded", true);
            details.set_property("visible", true);
        }
    }

    fn open_create_project_dialog(&self, asset_info: &egs_api::api::types::asset_info::AssetInfo) {
        let self_ = self.imp();

        let dialog = actions::EpicCreateProjectDialog::new();

        // Set transient parent (find the window)
        if let Some(window) = self_.window.get() {
            dialog.set_transient_for(Some(window));
        }

        // Set download manager
        if let Some(dm) = self_.download_manager.get() {
            dialog.set_download_manager(dm);
        }

        // Set the asset
        dialog.set_asset(asset_info);

        // Set selected version from the first release info
        if let Some(release_info) = asset_info.release_info.as_ref() {
            if let Some(first) = release_info.first() {
                if let Some(version) = &first.app_id {
                    dialog.set_selected_version(version);
                }
            }
        }

        dialog.present();
    }

    fn populate_model(list_item: &gtk4::ListItem) {
        let data = list_item
            .item()
            .unwrap()
            .downcast::<crate::models::asset_data::AssetData>()
            .unwrap();

        let child = list_item.child().unwrap().downcast::<EpicAsset>().unwrap();
        child.set_data(&data);
    }

    fn sorter(by: &str, asc: bool) -> CustomSorter {
        match by {
            "released" => gtk4::CustomSorter::new(move |obj1, obj2| {
                let info1 = obj1
                    .downcast_ref::<crate::models::asset_data::AssetData>()
                    .unwrap()
                    .release();
                let info2 = obj2
                    .downcast_ref::<crate::models::asset_data::AssetData>()
                    .unwrap()
                    .release();
                if info1.is_none() {
                    return gtk4::Ordering::Smaller;
                } else if info2.is_none() {
                    return gtk4::Ordering::Larger;
                }
                if asc {
                    info1.unwrap().cmp(&info2.unwrap()).into()
                } else {
                    info2.unwrap().cmp(&info1.unwrap()).into()
                }
            }),
            "updated" => gtk4::CustomSorter::new(move |obj1, obj2| {
                let info1 = obj1
                    .downcast_ref::<crate::models::asset_data::AssetData>()
                    .unwrap()
                    .last_modified();
                let info2 = obj2
                    .downcast_ref::<crate::models::asset_data::AssetData>()
                    .unwrap()
                    .last_modified();
                if info1.is_none() {
                    return gtk4::Ordering::Smaller;
                } else if info2.is_none() {
                    return gtk4::Ordering::Larger;
                }
                if asc {
                    info1.unwrap().cmp(&info2.unwrap()).into()
                } else {
                    info2.unwrap().cmp(&info1.unwrap()).into()
                }
            }),
            _ => gtk4::CustomSorter::new(move |obj1, obj2| {
                let info1 = obj1
                    .downcast_ref::<crate::models::asset_data::AssetData>()
                    .unwrap();
                let info2 = obj2
                    .downcast_ref::<crate::models::asset_data::AssetData>()
                    .unwrap();
                if asc {
                    info1
                        .name()
                        .to_lowercase()
                        .cmp(&info2.name().to_lowercase())
                        .into()
                } else {
                    info2
                        .name()
                        .to_lowercase()
                        .cmp(&info1.name().to_lowercase())
                        .into()
                }
            }),
        }
    }

    /// Open asset based on a name from xdg-open
    fn open_asset(&self) {
        let self_ = self.imp();
        let details = match self_.details.get() {
            Some(d) => d,
            None => return,
        };
        self.item().map_or_else(
            || {
                if let Some(product) = self.product() {
                    let assets = self_.loaded_assets.borrow();
                    let products = self_.asset_product_names.borrow();
                    products.get(&product).map_or_else(
                        || {
                            for prod in products.keys() {
                                if product.starts_with(prod) {
                                    if let Some(id) = products.get(prod) {
                                        if let Some(a) = assets.get(id) {
                                            details.set_asset(a);
                                        }
                                    }
                                    break;
                                }
                            }
                        },
                        |id| {
                            if let Some(a) = assets.get(id) {
                                details.set_asset(a);
                            }
                        },
                    );
                }
            },
            |id| {
                let assets = self_.loaded_assets.borrow();
                if let Some(a) = assets.get(&id) {
                    details.set_asset(a);
                }
            },
        );
    }

    pub fn flush_assets(&self) {
        let self_ = self.imp();
        self.sync_category_filters();
        if let Ok(mut vec) = self_.assets_pending.write() {
            if vec.is_empty() {
                return;
            }
            self_.grid_model.splice(0, 0, vec.as_slice());
            vec.clear();
        }
        // New Epic items can be the match of local zips that were shown on their own so far
        self.reconcile_local();
        self.update_count();
        // Scroll to top if nothing is selected
        let has_asset = self_.details.get().map_or(false, |d| d.has_asset());
        if !has_asset {
            if let Some(adj) = self_.asset_grid.vadjustment() {
                adj.set_value(0.0);
            };
        }
        self.check_refresh();
        self.open_asset();
    }

    fn check_refresh(&self) {
        if self.can_be_refreshed() {
            self.refresh_state_changed();
        }
    }

    pub fn update_count(&self) {
        let self_ = self.imp();
        let count = self_.filter_model.n_items();
        self_.count_label.set_label(&format!(
            "{} {}",
            count,
            if count == 1 { "item" } else { "items" }
        ));
    }

    pub fn bind_properties(&self) {
        let self_ = self.imp();
        self_
            .asset_search
            .bind_property("text", self, "search")
            .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
            .build();
    }

    pub fn setup_widgets(&self) {
        let self_ = self.imp();

        let order_by = ["name", "updated", "released"];
        let order_labels = ["Name", "Updated", "Released"];
        let model = gtk4::StringList::new(&[] as &[&str]);
        for label in order_labels {
            model.append(label);
        }
        self_.select_order_by.set_model(Some(&model));
        self_
            .order_by_ids
            .replace(order_by.iter().map(|id| id.to_string()).collect());
        if self_.select_order_by.selected() == gtk4::INVALID_LIST_POSITION {
            self_.select_order_by.set_selected(0);
        }

        // Multi-select filters: "Fab" and "Local" first (both on), then the categories.
        let bar = filter_bar::FilterBar::new(
            &self_.filter_button,
            &self_.filter_pills,
            clone!(
                #[weak(rename_to=library)]
                self,
                move || library.apply_filter()
            ),
        );
        let _ = self_.filter_bar.set(bar);

        if let Some(sidebar) = self_.sidebar.get() {
            sidebar.set_logged_in(self);
        }

        self_.select_order_by.connect_selected_notify(clone!(
            #[weak(rename_to=library)]
            self,
            move |_| {
                library.order_changed();
            }
        ));

        self_.asset_search.connect_search_changed(clone!(
            #[weak(rename_to=library)]
            self,
            move |_| {
                let _ = library.imp();
            }
        ));

        self_.downloaded_filter.connect_toggled(clone!(
            #[weak(rename_to=library)]
            self,
            move |_| {
                library.apply_filter();
            }
        ));

        self_.favorites_filter.connect_toggled(clone!(
            #[weak(rename_to=library)]
            self,
            move |_| {
                library.apply_filter();
            }
        ));
    }

    pub fn order_changed(&self) {
        let self_ = self.imp();
        let asc = self_.order.icon_name().map_or(false, |name| {
            matches!(name.as_str(), "view-sort-ascending-symbolic")
        });
        if let Some(by) = self.selected_order_by() {
            self_.sorter_model.set_sorter(Some(&Self::sorter(&by, asc)));
        }
    }

    fn selected_order_by(&self) -> Option<String> {
        let self_ = self.imp();
        let selected = self_.select_order_by.selected();
        if selected == gtk4::INVALID_LIST_POSITION {
            return None;
        }
        self_.order_by_ids.borrow().get(selected as usize).cloned()
    }

    pub fn setup_actions(&self) {
        let self_ = self.imp();

        action!(
            self_.actions,
            "show_download_details",
            clone!(
                #[weak(rename_to=library)]
                self,
                move |_, _| {
                    library.show_download_details();
                }
            )
        );

        action!(
            self_.actions,
            "order",
            clone!(
                #[weak(rename_to=library)]
                self,
                move |_, _| {
                    library.order();
                }
            )
        );

        self.insert_action_group("library", Some(&self_.actions));
    }

    fn show_download_details(&self) {
        let self_ = self.imp();
        if let Some(w) = self_.window.get() {
            w.show_download_manager();
        }
    }

    fn order(&self) {
        let self_ = self.imp();
        if let Some(name) = self_.order.icon_name() {
            match name.as_str() {
                "view-sort-ascending-symbolic" => {
                    self_.order.set_icon_name("view-sort-descending-symbolic");
                }
                _ => self_.order.set_icon_name("view-sort-ascending-symbolic"),
            }
        };
        self.order_changed();
    }

    pub fn filter(&self) -> Option<String> {
        self.property("filter")
    }

    pub fn loading(&self) -> u32 {
        self.property("to-load")
    }

    pub fn loaded(&self) -> u32 {
        self.property("loaded")
    }

    pub fn search(&self) -> Option<String> {
        self.property("search")
    }

    pub fn item(&self) -> Option<String> {
        self.property("item")
    }

    pub fn product(&self) -> Option<String> {
        self.property("product")
    }

    pub fn apply_filter(&self) {
        let self_ = self.imp();
        let search = self.search();
        let filter_p = self.filter();
        let downloaded_only = self_.downloaded_filter.is_active();
        let favorites_only = self_.favorites_filter.is_active();
        // Enabled source and category filters (the pills below the search bar)
        let filters = self_
            .filter_bar
            .get()
            .map(|b| b.filters())
            .unwrap_or_default();

        if filter_p.is_none()
            && search.is_none()
            && !downloaded_only
            && !favorites_only
            && filters.is_default()
        {
            self_.filter_model.set_filter(None::<&gtk4::CustomFilter>);
            self.update_count();
            return;
        }

        let filter = gtk4::CustomFilter::new(move |object| {
            let asset = object
                .downcast_ref::<crate::models::asset_data::AssetData>()
                .unwrap();
            let matches_search = search.as_ref().is_none_or(|se| {
                asset
                    .name()
                    .to_ascii_lowercase()
                    .contains(&se.to_ascii_lowercase())
            });
            let matches_category = filter_p.as_ref().is_none_or(|f| asset.check_category(f));
            let matches_downloaded = !downloaded_only || asset.downloaded();
            let matches_favorites = !favorites_only || asset.favorite();
            let matches_filters = filters.passes(asset.has_fab(), asset.has_local(), &|c| {
                asset.check_category(c)
            });

            matches_search
                && matches_category
                && matches_downloaded
                && matches_favorites
                && matches_filters
        });
        self_.filter_model.set_filter(Some(&filter));
        self.update_count();
    }

    pub fn add_category_to_dropdown(&self, path: &str) {
        let self_ = self.imp();
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let top_level = parts[0];
        let subcategory = parts.get(1).copied();

        let mut hierarchy = self_.category_hierarchy.borrow_mut();
        let is_new_top = !hierarchy.contains_key(top_level);

        if is_new_top {
            hierarchy.insert(top_level.to_string(), BTreeSet::new());
        }

        let is_new_sub = if let Some(sub) = subcategory {
            hierarchy
                .entry(top_level.to_string())
                .or_default()
                .insert(sub.to_string())
        } else {
            false
        };

        if is_new_top || is_new_sub {
            // The menu rows are rebuilt once per flush, not once per new category
            self_.categories_dirty.set(true);
        }
    }

    /// Rebuild the category rows of the filter menu when new categories were found.
    fn sync_category_filters(&self) {
        let self_ = self.imp();
        if !self_.categories_dirty.replace(false) {
            return;
        }
        if let Some(bar) = self_.filter_bar.get() {
            bar.set_categories(&self_.category_hierarchy.borrow());
        }
    }

    pub fn add_asset(
        &self,
        asset: &egs_api::api::types::asset_info::AssetInfo,
        image: Option<gtk4::gdk::Texture>,
    ) {
        let self_ = self.imp();
        if let Some(categories) = &asset.categories {
            for category in categories {
                let mut cats = self_.categories.borrow_mut();
                if cats.insert(category.path.clone()) {
                    self.add_category_to_dropdown(&category.path);
                }
            }
        };
        if let Some(window) = self.main_window() {
            let win_ = window.imp();
            let sender = win_.model.borrow().sender.clone();
            let _ = sender.send_blocking(crate::ui::messages::Msg::EndAssetProcessing);
            if self_.loaded_assets.borrow().contains_key(&asset.id) {
                // TODO: update asset if there are changes
                trace!("Duplicate asset: {}", asset.id);
                self.check_refresh();
                return;
            }
            self_
                .loaded_assets
                .borrow_mut()
                .insert(asset.id.clone(), asset.clone());
            if let Some(title) = asset.title.clone() {
                let title: String = title
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
                    .collect();
                let title: String = title.to_lowercase().replace(' ', "-");
                self_
                    .asset_product_names
                    .borrow_mut()
                    .insert(title, asset.id.clone());
            }

            // Epic lists a marketplace asset twice when it was migrated to Fab: the legacy item
            // (with the thumbnail) and its Fab copy (without). They are one entry here.
            let identity = ItemIdentity::of(asset);
            let name_key = library_merge::name_key(&identity.title);
            let twin_of = self_
                .name_groups
                .borrow()
                .get(&name_key)
                .and_then(|groups| {
                    groups
                        .iter()
                        .find(|g| {
                            let items: Vec<ItemIdentity> =
                                g.epic_items().iter().map(ItemIdentity::of).collect();
                            library_merge::can_join(&items, &identity)
                        })
                        .cloned()
                });
            let data = if let Some(group) = twin_of {
                trace!("Merging twin {} into {}", asset.id, group.id());
                group.add_twin(asset, image);
                group
            } else {
                let data = AssetData::new(asset, image);
                if !name_key.is_empty() {
                    self_
                        .name_groups
                        .borrow_mut()
                        .entry(name_key)
                        .or_default()
                        .push(data.clone());
                }
                // IMPORTANT: Insert the SAME object into both loaded_data and grid_model
                // so that updates to loaded_data are reflected in the GridView
                if let Ok(mut vec) = self_.assets_pending.write() {
                    vec.push(data.clone().upcast());
                }
                data
            };
            self_
                .loaded_data
                .borrow_mut()
                .insert(asset.id.clone(), data.clone());
            if let Some(listing) = identity.listing_id {
                self_
                    .listing_index
                    .borrow_mut()
                    .insert(listing.clone(), data.clone());
                if let Some(texture) = self_.fab_thumbnails.borrow().get(&listing) {
                    data.adopt_thumbnail(texture);
                }
            }
        }
    }

    /// Offer the picture of a Fab library asset. Entries whose Epic item has no thumbnail (the Fab
    /// copy of a migrated asset) show it instead of the generic placeholder.
    pub fn offer_fab_thumbnail(&self, listing_id: &str, texture: &gtk4::gdk::Texture) {
        let self_ = self.imp();
        self_
            .fab_thumbnails
            .borrow_mut()
            .insert(listing_id.to_string(), texture.clone());
        let data = self_.listing_index.borrow().get(listing_id).cloned();
        if let Some(data) = data {
            data.adopt_thumbnail(texture);
        }
    }

    /// Replace the set of local library zips (called whenever the local page finished a scan).
    pub fn set_local_entries(&self, entries: Vec<(LibraryEntry, Option<FabOverlay>)>) {
        let locals: Vec<(LocalRef, MatchKeys)> = entries
            .into_iter()
            .map(|(entry, overlay)| {
                let local = LocalRef::new(entry, overlay);
                let keys = local.keys();
                (local, keys)
            })
            .collect();
        self.imp().local_entries.replace(locals);
        self.reconcile_local();
    }

    /// Attach every local zip to the Epic/Fab entry it belongs to (same Fab listing id, else same
    /// cleaned name) and show the zips that match nothing as entries of their own. Safe to run at
    /// any time and in any order of arrival: it recomputes the result from both sides.
    pub fn reconcile_local(&self) {
        let self_ = self.imp();
        let locals = self_.local_entries.borrow();

        let mut by_listing: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, (_, keys)) in locals.iter().enumerate() {
            for l in &keys.listing_ids {
                by_listing.entry(l.as_str()).or_default().push(i);
            }
            for n in &keys.name_keys {
                by_name.entry(n.as_str()).or_default().push(i);
            }
        }

        let mut used = vec![false; locals.len()];
        let mut changed = false;
        let groups: Vec<AssetData> = self_
            .name_groups
            .borrow()
            .values()
            .flatten()
            .cloned()
            .collect();
        for group in &groups {
            let keys = group.match_keys();
            let mut matched: BTreeSet<usize> = BTreeSet::new();
            let candidates = keys
                .listing_ids
                .iter()
                .filter_map(|l| by_listing.get(l.as_str()))
                .chain(
                    keys.name_keys
                        .iter()
                        .filter_map(|n| by_name.get(n.as_str())),
                )
                .flatten();
            for &i in candidates {
                if library_merge::matches_local(&keys, &locals[i].1) {
                    matched.insert(i);
                }
            }
            for &i in &matched {
                used[i] = true;
            }
            let attached = matched.iter().map(|&i| locals[i].0.clone()).collect();
            changed |= group.set_locals(attached);
        }

        // Zips that match no Epic/Fab item: one entry per cleaned name and kind
        let mut wanted: BTreeMap<String, Vec<LocalRef>> = BTreeMap::new();
        for (i, (local, _)) in locals.iter().enumerate() {
            if used[i] {
                continue;
            }
            let sc = &local.entry.sidecar;
            let key = format!(
                "{}:{:?}",
                library_merge::name_key(&sc.display_name),
                sc.kind
            );
            wanted.entry(key).or_default().push(local.clone());
        }
        drop(locals);

        let mut only = self_.local_only.borrow_mut();
        let gone: Vec<String> = only
            .keys()
            .filter(|k| !wanted.contains_key(*k))
            .cloned()
            .collect();
        for key in gone {
            if let Some(data) = only.remove(&key) {
                if let Some(pos) = self_.grid_model.find(&data) {
                    self_.grid_model.remove(pos);
                }
                changed = true;
            }
        }
        let mut created: Vec<AssetData> = Vec::new();
        for (key, refs) in wanted {
            match only.get(&key) {
                Some(data) => changed |= data.set_locals(refs),
                None => {
                    let data = AssetData::new_local(&key, refs);
                    only.insert(key, data.clone());
                    created.push(data);
                    changed = true;
                }
            }
        }
        drop(only);
        for data in &created {
            self_.grid_model.append(data);
            self.register_local_only(data);
        }
        if changed {
            self.apply_filter();
            self.update_count();
        }
    }

    /// Fab picture and listing index of a new local-only entry.
    fn register_local_only(&self, data: &AssetData) {
        let self_ = self.imp();
        for listing in data.match_keys().listing_ids {
            if let Some(texture) = self_.fab_thumbnails.borrow().get(&listing) {
                data.adopt_thumbnail(texture);
            }
            self_
                .listing_index
                .borrow_mut()
                .insert(listing, data.clone());
        }
        if data.image().is_some() {
            return;
        }
        // The Fab match stored the listing's picture url; fetch it off the main thread.
        let url = data
            .locals()
            .iter()
            .find_map(|l| l.overlay.as_ref().and_then(|o| o.thumbnail_url.clone()));
        let Some(url) = url else {
            return;
        };
        let handle = gio::spawn_blocking(move || {
            crate::HTTP_CLIENT
                .get(url)
                .send()
                .ok()
                .and_then(|r| r.bytes().ok())
                .map(|b| b.to_vec())
        });
        glib::spawn_future_local(clone!(
            #[weak]
            data,
            async move {
                let Ok(Some(bytes)) = handle.await else {
                    return;
                };
                if let Ok(texture) = gtk4::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                {
                    data.adopt_thumbnail(&texture);
                }
            }
        ));
    }

    pub fn start_processing_asset(&self) {
        self.set_property("to-load", self.loading() + 1);
        self.update_progress();
    }

    pub fn end_processing_asset(&self) {
        self.set_property("loaded", self.loaded() + 1);
        self.update_progress();
    }

    fn update_progress(&self) {
        let self_ = self.imp();
        let loading = self.loading();
        let loaded = self.loaded();
        let fraction = if loading > 0 {
            f64::from(loaded) / f64::from(loading)
        } else {
            0.0
        };
        self_.refresh_progress.set_fraction(fraction);
        self_.refresh_progress.set_visible(loaded != loading);
    }

    pub fn load_thumbnail(&self, asset: &egs_api::api::types::asset_info::AssetInfo) {
        let self_ = self.imp();
        if let Some(window) = self.main_window() {
            let win_ = window.imp();
            let sender = win_.model.borrow().sender.clone();
            match asset.thumbnail() {
                None => {
                    let _ = sender.send_blocking(crate::ui::messages::Msg::ProcessAssetThumbnail(
                        asset.clone(),
                        None,
                    ));
                }
                Some(t) => {
                    let cache_dir = self_.settings.string("cache-directory").to_string();
                    let mut cache_path = PathBuf::from(cache_dir);
                    cache_path.push("images");
                    let name = Path::new(t.url.path()).extension().and_then(OsStr::to_str);
                    cache_path.push(format!("{}.{}", t.md5, name.unwrap_or("png")));
                    let asset = asset.clone();
                    self_.image_load_pool.execute(move || {
                        if !crate::RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        if cache_path.as_path().exists() {
                            match gtk4::gdk::Texture::from_file(&gio::File::for_path(
                                cache_path.as_path(),
                            )) {
                                Ok(t) => {
                                    let _ = sender.send_blocking(
                                        crate::ui::messages::Msg::ProcessAssetThumbnail(
                                            asset.clone(),
                                            Some(t),
                                        ),
                                    );
                                }
                                Err(e) => {
                                    error!(
                                        "Unable to load file {}{} to texture: {}",
                                        t.url, t.md5, e
                                    );
                                }
                            };
                        } else {
                            let _ = sender.send_blocking(crate::ui::messages::Msg::DownloadImage(
                                t,
                                asset.clone(),
                            ));
                        }
                    });
                }
            }
        }
    }

    fn main_window(&self) -> Option<&crate::window::EpicAssetManagerWindow> {
        let self_ = self.imp();
        self_.window.get()
    }

    pub fn fetch_assets(&self) {
        let self_ = self.imp();
        self.set_property("to-load", 0u32);
        self.set_property("loaded", 0u32);
        self_
            .refresh_progress
            .set_tooltip_text(Some("Loading from cache"));
        if let Some(window) = self.main_window() {
            let win_ = window.imp();
            let cache_dir = self_.settings.string("cache-directory").to_string();
            let cache_path = PathBuf::from(cache_dir);
            debug!("Fetching assets");
            let mut cached: Vec<String> = vec![];
            if cache_path.is_dir() {
                debug!("Checking cache");
                match std::fs::read_dir(cache_path) {
                    Ok(entries) => {
                        for entry in entries {
                            let sender = win_.model.borrow().sender.clone();
                            if let Ok(entr) = entry {
                                let mut asset_file = entr.path();
                                cached.push(entr.file_name().into_string().unwrap_or_default());
                                asset_file.push("asset_info.json");
                                self_.asset_load_pool.execute(move || {
                                    // Load assets from cache

                                    if !crate::RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
                                        return;
                                    }

                                    if asset_file.exists() {
                                        let _ = sender.send_blocking(
                                            crate::ui::messages::Msg::StartAssetProcessing,
                                        );
                                        if let Ok(f) = std::fs::File::open(asset_file.as_path()) {
                                            if let Ok(asset) = serde_json::from_reader(f) {
                                                let _ = sender.send_blocking(
                                                    crate::ui::messages::Msg::ProcessAssetInfo(
                                                        asset,
                                                    ),
                                                );
                                            }
                                        };
                                    }
                                });
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to read cache directory: {}", e);
                    }
                }
            };
            self.set_property("to-load", 0u32);
            self.set_property("loaded", 0u32);
            self_
                .refresh_progress
                .set_tooltip_text(Some("Loading from Epic Store"));
            let mut eg = win_.model.borrow().epic_games.borrow().clone();
            let sender = win_.model.borrow().sender.clone();
            // Start loading assets from the API
            self_.asset_load_pool.execute(move || {
                let mut assets = crate::RUNTIME.block_on(eg.list_assets(None, None));
                assets.sort_by(|a, b| {
                    let contains_a = cached.contains(&a.catalog_item_id);
                    let contains_b = cached.contains(&b.catalog_item_id);
                    if contains_a && contains_b {
                        std::cmp::Ordering::Equal
                    } else if contains_a {
                        std::cmp::Ordering::Greater
                    } else if contains_b {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                });
                for asset in assets {
                    let _ = sender.send_blocking(crate::ui::messages::Msg::StartAssetProcessing);
                    let _ = sender.send_blocking(crate::ui::messages::Msg::ProcessEpicAsset(asset));
                }
            });
            self.refresh_state_changed();
            glib::idle_add_local(clone!(
                #[weak(rename_to=library)]
                self,
                #[upgrade_or_panic]
                move || {
                    if library.flush_loop() {
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
                }
            ));
        }
    }

    fn flush_loop(&self) -> bool {
        self.flush_assets();
        self.refresh_state_changed();
        self.check_refresh();
        !self.can_be_refreshed()
    }

    pub fn process_epic_asset(&self, epic_asset: &egs_api::api::types::epic_asset::EpicAsset) {
        let self_ = self.imp();
        if let Some(window) = self.main_window() {
            let win_ = window.imp();
            let mut cache_dir = PathBuf::from(self_.settings.string("cache-directory").to_string());
            cache_dir.push(&epic_asset.catalog_item_id);
            let mut cache_dir_c = cache_dir.clone();
            let ea = epic_asset.clone();

            // Write the Epic Asset file
            self_.asset_load_pool.execute(move || {
                cache_dir_c.push("epic_asset.json");
                let Some(parent) = cache_dir_c.parent() else {
                    error!("Missing parent directory for epic asset cache");
                    return;
                };
                if let Err(e) = fs::create_dir_all(parent) {
                    error!("Failed to create epic asset cache directory: {}", e);
                    return;
                }
                let asset_file = File::create(cache_dir_c.as_path());
                let mut asset_file = match asset_file {
                    Ok(asset_file) => asset_file,
                    Err(e) => {
                        error!("Failed to create epic asset cache file: {}", e);
                        return;
                    }
                };
                let payload = match serde_json::to_string(&ea) {
                    Ok(payload) => payload,
                    Err(e) => {
                        error!("Failed to serialize epic asset: {}", e);
                        return;
                    }
                };
                if let Err(e) = asset_file.write_all(payload.as_bytes()) {
                    error!("Failed to write epic asset cache file: {}", e);
                }
            });

            let mut eg = win_.model.borrow().epic_games.borrow().clone();
            let sender = win_.model.borrow().sender.clone();
            let mut cache_dir_c = cache_dir;
            let epic_asset = epic_asset.clone();
            self_.asset_load_pool.execute(move || {
                if !crate::RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let asset_result = crate::RUNTIME.block_on(eg.try_asset_info(&epic_asset));
                match &asset_result {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        warn!(
                            "asset_info returned None for catalog_item_id={} namespace={}",
                            epic_asset.catalog_item_id, epic_asset.namespace
                        );
                    }
                    Err(e) => {
                        warn!(
                            "asset_info failed for catalog_item_id={}: {:?}",
                            epic_asset.catalog_item_id, e
                        );
                    }
                }
                if let Some(asset) = asset_result.ok().flatten() {
                    // TODO: Check with already added assets to see if it needs updating
                    cache_dir_c.push("asset_info.json");
                    if let Some(parent) = cache_dir_c.parent() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            error!("Failed to create asset info cache directory: {}", e);
                        } else {
                            match File::create(cache_dir_c.as_path()) {
                                Ok(mut asset_file) => match serde_json::to_string(&asset) {
                                    Ok(payload) => {
                                        if let Err(e) = asset_file.write_all(payload.as_bytes()) {
                                            error!("Failed to write asset info cache file: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to serialize asset info: {}", e);
                                    }
                                },
                                Err(e) => {
                                    error!("Failed to create asset info cache file: {}", e);
                                }
                            }
                        }
                    } else {
                        error!("Missing parent directory for asset info cache");
                    }
                    let _ = sender.send_blocking(crate::ui::messages::Msg::ProcessAssetInfo(asset));
                }
            });
        }
    }

    pub fn refresh_asset(&self, id: &str) {
        let self_ = self.imp();
        if let Some(data) = self_.loaded_data.borrow().get(id) {
            data.refresh();
        }
        self.apply_filter();
    }

    pub fn set_asset_downloading(&self, id: &str, downloading: bool) {
        // loaded_data holds the same objects the grid is bound to, under every catalog id of the
        // entry (a downloaded release can belong to either item of a merged pair).
        let data = self.imp().loaded_data.borrow().get(id).cloned();
        if let Some(data) = data {
            data.set_downloading(downloading);
            if !downloading {
                data.set_download_progress(0.0);
            }
        }
    }

    pub fn set_asset_download_progress(&self, id: &str, progress: f64, speed: &str) {
        let data = self.imp().loaded_data.borrow().get(id).cloned();
        if let Some(data) = data {
            data.set_download_info(progress, speed);
        }
    }
}

impl Refresh for EpicLibraryBox {
    fn run_refresh(&self) {
        self.fetch_assets();
    }
    fn can_be_refreshed(&self) -> bool {
        let self_ = self.imp();
        trace!(
            "asset_load_pool queued: {}",
            self_.asset_load_pool.queued_count()
        );
        trace!(
            "asset_load_pool active: {}",
            self_.asset_load_pool.active_count()
        );
        trace!(
            "image_load_pool queued: {}",
            self_.image_load_pool.queued_count()
        );
        trace!(
            "image_load_pool active: {}",
            self_.image_load_pool.active_count()
        );
        self_.asset_load_pool.queued_count()
            + self_.asset_load_pool.active_count()
            + self_.image_load_pool.queued_count()
            + self_.image_load_pool.active_count()
            == 0
    }
    fn refresh_state_changed(&self) {
        // No-op in unified view - refresh button always available
    }
}
