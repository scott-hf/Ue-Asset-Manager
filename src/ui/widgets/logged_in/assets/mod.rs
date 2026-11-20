//! "Assets" page: the cross-pack asset library on top of `ue_asset_catalog`.
//!
//! Every asset of every content and project pack (plus the read-only generated assets of the
//! pipeline portal) is searchable. The catalog is loaded on a worker thread the first time the
//! page is opened, searches run on worker threads and come back over `async_channel`, thumbnails
//! are loaded lazily per visible card. Ticked assets from any pack go into a project through
//! `import::plan_import` / `apply_plan` (see `add_dialog`).

pub mod add_dialog;
mod hidden_packs;
mod thumbs;

use crate::models::project_data::ProjectData;
use crate::models::project_registry as registry;
use crate::window::EpicAssetManagerWindow;
use gtk4::glib::clone;
use gtk4::subclass::prelude::*;
use gtk4::{self, gio, prelude::*};
use gtk4::{glib, CompositeTemplate};
use hidden_packs::HiddenPack;
use log::info;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use thumbs::ThumbLoader;
use ue_asset_catalog::catalog::{Asset, SourceKind};
use ue_asset_catalog::generated::{self, PortalConfig};
use ue_asset_catalog::{tags, Catalog, LoadOptions, LoadReport, SearchQuery};

/// Hits fetched per search and per "Show more".
const PAGE: usize = 200;
const MAX_LISTED_TAGS: usize = 40;
const MAX_LISTED_NAMES: usize = 12;

/// A registered project that can be the target of "Add to project" or the engine filter.
#[derive(Clone, Debug)]
pub struct ProjectChoice {
    pub name: String,
    pub uproject: String,
    pub engine: Option<(u32, u32)>,
}

impl ProjectChoice {
    pub fn label(&self) -> String {
        match self.engine {
            Some((a, b)) => format!("{} (UE {a}.{b})", self.name),
            None => self.name.clone(),
        }
    }
}

/// The projects known to the Projects page, sorted by name, with their resolved engine version.
pub fn collect_projects(window: &EpicAssetManagerWindow) -> Vec<ProjectChoice> {
    let settings = gio::Settings::new(crate::config::APP_ID);
    let logged_in = window.imp().logged_in_stack.clone();
    let engines_box = logged_in.imp().engines.clone();
    let model = &logged_in.imp().projects.imp().grid_model;
    let mut out: Vec<ProjectChoice> = Vec::new();
    for i in 0..model.n_items() {
        let Some(data) = model.item(i).and_downcast::<ProjectData>() else {
            continue;
        };
        let Some(path) = data.path() else {
            continue;
        };
        let association = data
            .uproject()
            .map(|u| u.engine_association)
            .unwrap_or_default();
        let engine = registry::resolve_engine_version(&settings, &path, &association, || {
            engines_box
                .engine_from_assoociation(&association)
                .and_then(|e| {
                    e.version
                        .valid()
                        .then(|| format!("{}.{}", e.version.major_version, e.version.minor_version))
                })
        })
        .and_then(|(v, _)| registry::version_tuple(&v));
        out.push(ProjectChoice {
            name: data.name().unwrap_or_else(|| path.clone()),
            uproject: path,
            engine,
        });
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SourceFilter {
    All,
    Packs,
    Generated,
}

struct SearchRequest {
    text: String,
    /// Family slugs of the packs the user filtered out.
    exclude: Vec<String>,
    kind: Option<String>,
    pack: Option<String>,
    family: Option<usize>,
    engine: Option<(u32, u32)>,
    source: SourceFilter,
    want: usize,
}

pub struct SearchResult {
    pub ids: Vec<usize>,
    pub total: usize,
    pub more: bool,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub selected: usize,
    pub required: usize,
    pub bytes: u64,
}

pub struct Loaded {
    pub catalog: Catalog,
    pub report: LoadReport,
    pub generated_warnings: Vec<String>,
}

pub enum Msg {
    Loaded(u64, Box<Loaded>),
    Results(u64, SearchResult),
    Summary(u64, Summary),
}

/// One search, page by page (the catalog caps a page at 100), with the filters the catalog
/// itself cannot express applied here.
fn run_search(catalog: &Catalog, r: &SearchRequest) -> SearchResult {
    let mut ids: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    let mut total: usize;
    let mut more = false;
    let mut note: Option<String> = None;
    loop {
        let query = SearchQuery {
            text: r.text.clone(),
            kind: r.kind.clone(),
            tags: if r.source == SourceFilter::Generated {
                vec!["source:generated".to_string()]
            } else {
                Vec::new()
            },
            pack: r.pack.clone(),
            exclude_packs: r.exclude.clone(),
            engine_version: r.engine,
            limit: 100,
            offset,
            ..SearchQuery::default()
        };
        let out = catalog.search(&query);
        total = out.total;
        if note.is_none() {
            note = out.notes.first().cloned();
        }
        let n = out.hits.len();
        for hit in &out.hits {
            let asset = &catalog.assets[hit.asset];
            if r.source == SourceFilter::Packs && asset.source == SourceKind::Generated {
                continue;
            }
            if r.family.is_some_and(|f| asset.family != f) {
                continue;
            }
            if ids.len() >= r.want {
                more = true;
                break;
            }
            ids.push(hit.asset);
        }
        if more || n < 100 || offset + n >= total {
            break;
        }
        offset += n;
    }
    SearchResult {
        ids,
        total,
        more,
        note,
    }
}

/// Selected / required / size for the footer. Required packages come from each pack's
/// dependency closure (the representative release; the exact plan is made when adding).
fn compute_summary(catalog: &Catalog, selected: &[usize]) -> Summary {
    let mut summary = Summary {
        selected: selected.len(),
        ..Summary::default()
    };
    let mut by_pack: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for &a in selected {
        let asset = &catalog.assets[a];
        if asset.source == SourceKind::Generated {
            continue;
        }
        let Some(release) = asset.releases.first() else {
            continue;
        };
        let Some(package) = catalog.package(*release) else {
            continue;
        };
        by_pack
            .entry(release.pack)
            .or_default()
            .push(package.game_path.clone());
    }
    for (pack, mut paths) in by_pack {
        let Some(index) = catalog.packs[pack].index.as_ref() else {
            continue;
        };
        paths.sort();
        paths.dedup();
        let needed = ue_asset_library::closure(index, &paths);
        let sizes: HashMap<&str, u64> = index
            .packages
            .iter()
            .map(|p| (p.game_path.as_str(), p.size))
            .collect();
        summary.required += needed.len().saturating_sub(paths.len());
        summary.bytes += needed
            .iter()
            .filter_map(|p| sizes.get(p.as_str()))
            .sum::<u64>();
    }
    summary
}

fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", (b / 1024.0).ceil())
    }
}

/// "staticmesh" -> "static mesh"
fn type_label(name: &str) -> &str {
    match name {
        "staticmesh" => "static mesh",
        "skeletalmesh" => "skeletal mesh",
        other => other,
    }
}

/// A pack name cut to `max` characters for a menu entry.
fn truncate(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        name.to_string()
    } else {
        let cut: String = name.chars().take(max.saturating_sub(1)).collect();
        format!("{}...", cut.trim_end())
    }
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "texture" | "material" => "image-x-generic-symbolic",
        "staticmesh" | "skeletalmesh" | "mesh" => "package-x-generic-symbolic",
        "sound" => "audio-x-generic-symbolic",
        "level" => "mark-location-symbolic",
        "animation" => "media-playback-start-symbolic",
        _ => "package-x-generic-symbolic",
    }
}

pub mod imp {
    use super::*;
    use once_cell::sync::OnceCell;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/assets.ui")]
    pub struct EpicAssetsBox {
        pub window: OnceCell<EpicAssetManagerWindow>,
        pub settings: gio::Settings,
        pub catalog: RefCell<Option<Arc<Catalog>>>,
        pub loaded: Cell<bool>,
        pub loading: Cell<bool>,
        pub load_serial: Cell<u64>,
        pub search_serial: Cell<u64>,
        pub summary_serial: Cell<u64>,
        pub want: Cell<usize>,
        /// Catalog family behind each pack dropdown entry after "All packs".
        pub families: RefCell<Vec<usize>>,
        pub projects: RefCell<Vec<ProjectChoice>>,
        pub selected: RefCell<BTreeSet<usize>>,
        pub hidden: RefCell<Vec<HiddenPack>>,
        pub summary: RefCell<Option<Summary>>,
        /// Asset indices currently in `model`, in order.
        pub shown: RefCell<Vec<usize>>,
        pub model: gtk4::StringList,
        pub selection: gtk4::SingleSelection,
        pub thumbs: OnceCell<Rc<ThumbLoader>>,
        pub bound: RefCell<Vec<glib::WeakRef<gtk4::ListItem>>>,
        pub refreshing: Cell<bool>,
        pub sender: async_channel::Sender<Msg>,
        pub receiver: RefCell<Option<async_channel::Receiver<Msg>>>,
        #[template_child]
        pub count_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub refresh_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub info_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub content_stack: TemplateChild<gtk4::Stack>,
        #[template_child]
        pub loading_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub empty_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub search_entry: TemplateChild<gtk4::SearchEntry>,
        #[template_child]
        pub type_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub source_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub pack_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub project_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub hidden_button: TemplateChild<gtk4::MenuButton>,
        #[template_child]
        pub hidden_box: TemplateChild<gtk4::Box>,
        #[template_child]
        pub engine_check: TemplateChild<gtk4::CheckButton>,
        #[template_child]
        pub grid_view: TemplateChild<gtk4::GridView>,
        #[template_child]
        pub detail_image: TemplateChild<gtk4::Image>,
        #[template_child]
        pub detail_name: TemplateChild<gtk4::Label>,
        #[template_child]
        pub detail_path: TemplateChild<gtk4::Label>,
        #[template_child]
        pub detail_text: TemplateChild<gtk4::Label>,
        #[template_child]
        pub summary_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub caption_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub more_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub clear_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub add_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicAssetsBox {
        const NAME: &'static str = "EpicAssetsBox";
        type Type = super::EpicAssetsBox;
        type ParentType = gtk4::Box;

        fn new() -> Self {
            let (sender, receiver) = async_channel::unbounded();
            let model = gtk4::StringList::new(&[]);
            let selection = gtk4::SingleSelection::builder()
                .model(&model)
                .autoselect(false)
                .can_unselect(true)
                .build();
            Self {
                window: OnceCell::new(),
                settings: gio::Settings::new(crate::config::APP_ID),
                catalog: RefCell::new(None),
                loaded: Cell::new(false),
                loading: Cell::new(false),
                load_serial: Cell::new(0),
                search_serial: Cell::new(0),
                summary_serial: Cell::new(0),
                want: Cell::new(PAGE),
                families: RefCell::new(Vec::new()),
                projects: RefCell::new(Vec::new()),
                selected: RefCell::new(BTreeSet::new()),
                hidden: RefCell::new(hidden_packs::load()),
                summary: RefCell::new(None),
                shown: RefCell::new(Vec::new()),
                model,
                selection,
                thumbs: OnceCell::new(),
                bound: RefCell::new(Vec::new()),
                refreshing: Cell::new(false),
                sender,
                receiver: RefCell::new(Some(receiver)),
                count_label: TemplateChild::default(),
                refresh_button: TemplateChild::default(),
                info_banner: TemplateChild::default(),
                content_stack: TemplateChild::default(),
                loading_label: TemplateChild::default(),
                empty_page: TemplateChild::default(),
                search_entry: TemplateChild::default(),
                type_dropdown: TemplateChild::default(),
                source_dropdown: TemplateChild::default(),
                pack_dropdown: TemplateChild::default(),
                project_dropdown: TemplateChild::default(),
                hidden_button: TemplateChild::default(),
                hidden_box: TemplateChild::default(),
                engine_check: TemplateChild::default(),
                grid_view: TemplateChild::default(),
                detail_image: TemplateChild::default(),
                detail_name: TemplateChild::default(),
                detail_path: TemplateChild::default(),
                detail_text: TemplateChild::default(),
                summary_label: TemplateChild::default(),
                caption_label: TemplateChild::default(),
                more_button: TemplateChild::default(),
                clear_button: TemplateChild::default(),
                add_button: TemplateChild::default(),
            }
        }

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for EpicAssetsBox {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_widgets();
            obj.setup_grid();
            obj.setup_events();
            obj.setup_receiver();
        }
    }

    impl WidgetImpl for EpicAssetsBox {}
    impl BoxImpl for EpicAssetsBox {}
}

glib::wrapper! {
    pub struct EpicAssetsBox(ObjectSubclass<imp::EpicAssetsBox>)
        @extends gtk4::Widget, gtk4::Box,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

impl Default for EpicAssetsBox {
    fn default() -> Self {
        Self::new()
    }
}

type CardParts = (gtk4::CheckButton, gtk4::Image, gtk4::Label, gtk4::Label);

fn card_parts(item: &gtk4::ListItem) -> Option<CardParts> {
    let card = item.child().and_downcast::<gtk4::Box>()?;
    let overlay = card.first_child().and_downcast::<gtk4::Overlay>()?;
    let image = overlay.child().and_downcast::<gtk4::Image>()?;
    let check = image.next_sibling().and_downcast::<gtk4::CheckButton>()?;
    let name = overlay.next_sibling().and_downcast::<gtk4::Label>()?;
    let sub = name.next_sibling().and_downcast::<gtk4::Label>()?;
    Some((check, image, name, sub))
}

fn item_asset(item: &gtk4::ListItem) -> Option<usize> {
    item.item()
        .and_downcast::<gtk4::StringObject>()
        .and_then(|s| s.string().parse().ok())
}

impl EpicAssetsBox {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_window(&self, window: &EpicAssetManagerWindow) {
        let _ = self.imp().window.set(window.clone());
    }

    fn setup_widgets(&self) {
        let self_ = self.imp();
        let mut types: Vec<&str> = vec!["All types"];
        types.extend(tags::TYPE_NAMES.iter().map(|n| type_label(n)));
        self_
            .type_dropdown
            .set_model(Some(&gtk4::StringList::new(&types)));
        self_
            .source_dropdown
            .set_model(Some(&gtk4::StringList::new(&[
                "Packs and generated",
                "Packs only",
                "Generated only",
            ])));
        self_
            .pack_dropdown
            .set_model(Some(&gtk4::StringList::new(&["All packs"])));
        let expression = gtk4::PropertyExpression::new(
            gtk4::StringObject::static_type(),
            None::<&gtk4::Expression>,
            "string",
        );
        self_.pack_dropdown.set_expression(Some(&expression));
        self_
            .project_dropdown
            .set_model(Some(&gtk4::StringList::new(&["Any project"])));
        self.rebuild_hidden();
        let weak = self.downgrade();
        let _ = self_.thumbs.set(ThumbLoader::new(move || {
            if let Some(page) = weak.upgrade() {
                page.refresh_bound();
            }
        }));
    }

    fn setup_grid(&self) {
        let self_ = self.imp();
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(clone!(
            #[weak(rename_to=page)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
                card.set_margin_top(6);
                card.set_margin_bottom(6);
                card.set_margin_start(6);
                card.set_margin_end(6);
                card.set_halign(gtk4::Align::Center);
                let overlay = gtk4::Overlay::new();
                let image = gtk4::Image::from_icon_name("package-x-generic-symbolic");
                image.set_pixel_size(96);
                overlay.set_child(Some(&image));
                let check = gtk4::CheckButton::new();
                check.set_halign(gtk4::Align::Start);
                check.set_valign(gtk4::Align::Start);
                overlay.add_overlay(&check);
                let name = gtk4::Label::new(None);
                name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
                name.set_max_width_chars(16);
                name.add_css_class("caption-heading");
                let sub = gtk4::Label::new(None);
                sub.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                sub.set_max_width_chars(18);
                sub.add_css_class("caption");
                sub.add_css_class("dim-label");
                card.append(&overlay);
                card.append(&name);
                card.append(&sub);
                item.set_child(Some(&card));

                let menu_item = item.downgrade();
                let click = gtk4::GestureClick::builder().button(3).build();
                click.connect_pressed(clone!(
                    #[weak]
                    page,
                    #[weak]
                    card,
                    #[strong]
                    menu_item,
                    move |_, _, x, y| {
                        if let Some(asset) = menu_item.upgrade().as_ref().and_then(item_asset) {
                            page.show_card_menu(&card, asset, x, y);
                        }
                    }
                ));
                card.add_controller(click);
                let press = gtk4::GestureLongPress::new();
                press.connect_pressed(clone!(
                    #[weak]
                    page,
                    #[weak]
                    card,
                    #[strong]
                    menu_item,
                    move |_, x, y| {
                        if let Some(asset) = menu_item.upgrade().as_ref().and_then(item_asset) {
                            page.show_card_menu(&card, asset, x, y);
                        }
                    }
                ));
                card.add_controller(press);

                let weak_item = item.downgrade();
                check.connect_toggled(clone!(
                    #[weak]
                    page,
                    move |check| {
                        if page.imp().refreshing.get() {
                            return;
                        }
                        if let Some(asset) = weak_item.upgrade().as_ref().and_then(item_asset) {
                            page.on_toggled(asset, check.is_active());
                        }
                    }
                ));
            }
        ));
        factory.connect_bind(clone!(
            #[weak(rename_to=page)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                page.imp().bound.borrow_mut().push(item.downgrade());
                page.refresh_card(item);
            }
        ));
        factory.connect_unbind(clone!(
            #[weak(rename_to=page)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let self_ = page.imp();
                self_
                    .bound
                    .borrow_mut()
                    .retain(|w| w.upgrade().is_some_and(|i| &i != item));
                let catalog = self_.catalog.borrow().clone();
                if let (Some(asset), Some(catalog), Some(thumbs)) =
                    (item_asset(item), catalog, self_.thumbs.get())
                {
                    if let Some(a) = catalog.assets.get(asset) {
                        thumbs.unwant(&a.id);
                    }
                }
            }
        ));
        self_.grid_view.set_model(Some(&self_.selection));
        self_.grid_view.set_factory(Some(&factory));
    }

    fn setup_events(&self) {
        let self_ = self.imp();
        self_.refresh_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.start_load()
        ));
        self_.search_entry.connect_search_changed(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.new_search()
        ));
        self_.type_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.new_search()
        ));
        self_.source_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.new_search()
        ));
        self_.pack_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.new_search()
        ));
        self_.project_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| {
                page.update_engine_check();
                page.new_search();
            }
        ));
        self_.engine_check.connect_toggled(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.new_search()
        ));
        self_.more_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| {
                let self_ = page.imp();
                self_.want.set(self_.want.get() + PAGE);
                page.run_search();
            }
        ));
        self_.clear_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| {
                page.imp().selected.borrow_mut().clear();
                page.after_selection_change();
                page.refresh_bound();
            }
        ));
        self_.add_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.open_add_dialog()
        ));
        self_.selection.connect_selection_changed(clone!(
            #[weak(rename_to=page)]
            self,
            move |_, _, _| page.update_detail()
        ));
    }

    fn setup_receiver(&self) {
        let self_ = self.imp();
        let Some(receiver) = self_.receiver.borrow_mut().take() else {
            return;
        };
        glib::spawn_future_local(clone!(
            #[weak(rename_to=page)]
            self,
            async move {
                while let Ok(msg) = receiver.recv().await {
                    page.handle_msg(msg);
                }
            }
        ));
    }

    // ---------------------------------------------------------------- loading

    /// Called whenever the page becomes visible: load the catalog the first time, and refresh
    /// the list of projects (they change while the app runs).
    pub fn on_shown(&self) {
        self.refresh_projects();
        let self_ = self.imp();
        if !self_.loaded.get() && !self_.loading.get() {
            self.start_load();
        }
    }

    fn configured_dirs(&self) -> Vec<PathBuf> {
        self.imp()
            .settings
            .strv("local-library-directories")
            .iter()
            .map(|d| PathBuf::from(d.as_str()))
            .collect()
    }

    /// Read the catalog on a worker thread (about a minute cold over the network, under a second
    /// from the local cache). The UI shows a spinner with the elapsed time meanwhile.
    pub fn start_load(&self) {
        let self_ = self.imp();
        let dirs = self.configured_dirs();
        let serial = self_.load_serial.get() + 1;
        self_.load_serial.set(serial);
        if dirs.is_empty() {
            self_.loading.set(false);
            self_.loaded.set(false);
            self_.empty_page.set_title("No local library configured");
            self_.empty_page.set_description(Some(
                "Add a Local Library directory in the preferences, then reload.",
            ));
            self_.content_stack.set_visible_child_name("empty");
            return;
        }
        self_.loading.set(true);
        self_.refresh_button.set_sensitive(false);
        self_.info_banner.set_revealed(false);
        self_.content_stack.set_visible_child_name("loading");
        let started = Instant::now();
        self_.loading_label.set_label("Reading the library...");
        glib::timeout_add_seconds_local(
            1,
            clone!(
                #[weak(rename_to=page)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let self_ = page.imp();
                    if !self_.loading.get() || self_.load_serial.get() != serial {
                        return glib::ControlFlow::Break;
                    }
                    self_.loading_label.set_label(&format!(
                        "Reading the library... {} s\nThe first load over the network can take about a minute; later loads use a local cache.",
                        started.elapsed().as_secs()
                    ));
                    glib::ControlFlow::Continue
                }
            ),
        );
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let cache = ue_asset_catalog::install_log::default_cache_path();
            let options = LoadOptions {
                cache_path: cache.clone(),
                ..LoadOptions::default()
            };
            let (mut catalog, report) = Catalog::load(&dirs, &options);
            let mut portal = PortalConfig::new(generated::DEFAULT_PORTAL_URL);
            portal.cache_path = cache.and_then(|p| p.parent().map(|d| d.join("generated.json")));
            let (items, fetched) = generated::load(&portal);
            catalog.add_generated(&items);
            let _ = sender.send_blocking(Msg::Loaded(
                serial,
                Box::new(Loaded {
                    catalog,
                    report,
                    generated_warnings: fetched.warnings,
                }),
            ));
        });
    }

    fn handle_msg(&self, msg: Msg) {
        let self_ = self.imp();
        match msg {
            Msg::Loaded(serial, loaded) => {
                if serial != self_.load_serial.get() {
                    return;
                }
                self.on_loaded(*loaded);
            }
            Msg::Results(serial, result) => {
                if serial != self_.search_serial.get() {
                    return;
                }
                self.show_results(result);
            }
            Msg::Summary(serial, summary) => {
                if serial != self_.summary_serial.get() {
                    return;
                }
                self_.summary.replace(Some(summary));
                self.update_footer();
            }
        }
    }

    fn on_loaded(&self, loaded: Loaded) {
        let self_ = self.imp();
        self_.loading.set(false);
        self_.loaded.set(true);
        self_.refresh_button.set_sensitive(true);
        let Loaded {
            catalog,
            report,
            generated_warnings,
        } = loaded;
        let stats = catalog.stats();
        info!(
            "Asset catalog: {} assets from {} packs in {} ms ({} cached, {} overlays, {} built, {} failed)",
            stats.assets,
            stats.packs,
            report.elapsed_ms,
            report.from_cache,
            report.from_overlay,
            report.built,
            report.failed.len()
        );
        let mut notes: Vec<String> = Vec::new();
        if !report.failed.is_empty() {
            notes.push(format!(
                "{} packs could not be read (first: {}).",
                report.failed.len(),
                report.failed[0].1
            ));
        }
        if let Some(w) = generated_warnings.first() {
            notes.push(format!("Generated assets: {w}"));
        }
        if let Some(w) = report.warnings.first() {
            notes.push(w.clone());
        }
        if notes.is_empty() {
            self_.info_banner.set_revealed(false);
        } else {
            self_.info_banner.set_title(&notes.join(" "));
            self_.info_banner.set_revealed(true);
        }
        self_.count_label.set_label(&format!(
            "{} assets in {} packs",
            stats.assets, stats.indexed_packs
        ));

        // Pack dropdown: one entry per product (releases of a pack are one family).
        let mut families: Vec<usize> = catalog
            .families
            .iter()
            .enumerate()
            .filter(|(_, f)| f.source == SourceKind::Pack && !f.packs.is_empty())
            .map(|(i, _)| i)
            .collect();
        families.sort_by_key(|&i| catalog.families[i].name.to_lowercase());
        let mut labels: Vec<&str> = vec!["All packs"];
        labels.extend(families.iter().map(|&i| catalog.families[i].name.as_str()));
        self_
            .pack_dropdown
            .set_model(Some(&gtk4::StringList::new(&labels)));
        self_.families.replace(families);

        let has_assets = !catalog.assets.is_empty();
        self_.catalog.replace(Some(Arc::new(catalog)));
        self_.selected.borrow_mut().clear();
        self_.summary.replace(None);
        self_.shown.borrow_mut().clear();
        let old = self_.model.n_items();
        self_.model.splice(0, old, &[]);
        self.update_footer();
        if has_assets {
            self_.content_stack.set_visible_child_name("ready");
            self.new_search();
        } else {
            self_.empty_page.set_title("No assets");
            self_.empty_page.set_description(Some(
                "The library has no indexed packs yet. Check the Local Library directories in the preferences.",
            ));
            self_.content_stack.set_visible_child_name("empty");
        }
    }

    // ---------------------------------------------------------------- projects

    fn refresh_projects(&self) {
        let self_ = self.imp();
        let Some(window) = self_.window.get() else {
            return;
        };
        let previous = self.selected_project().map(|p| p.uproject).or_else(|| {
            window
                .imp()
                .logged_in_stack
                .imp()
                .projects
                .property::<Option<String>>("selected")
        });
        let projects = collect_projects(window);
        let labels: Vec<String> = projects.iter().map(ProjectChoice::label).collect();
        let mut strs: Vec<&str> = vec!["Any project"];
        strs.extend(labels.iter().map(String::as_str));
        let pos = previous
            .as_ref()
            .and_then(|prev| projects.iter().position(|p| &p.uproject == prev))
            .map_or(0, |i| i + 1);
        self_.projects.replace(projects);
        self_
            .project_dropdown
            .set_model(Some(&gtk4::StringList::new(&strs)));
        self_.project_dropdown.set_selected(pos as u32);
        self.update_engine_check();
    }

    fn selected_project(&self) -> Option<ProjectChoice> {
        let self_ = self.imp();
        let idx = self_.project_dropdown.selected();
        if idx == gtk4::INVALID_LIST_POSITION || idx == 0 {
            return None;
        }
        self_.projects.borrow().get(idx as usize - 1).cloned()
    }

    fn update_engine_check(&self) {
        let self_ = self.imp();
        let usable = self.selected_project().is_some_and(|p| p.engine.is_some());
        self_.engine_check.set_sensitive(usable);
        if !usable && self_.engine_check.is_active() {
            self_.engine_check.set_active(false);
        }
    }

    // ---------------------------------------------------------------- search

    fn new_search(&self) {
        self.imp().want.set(PAGE);
        self.run_search();
    }

    fn run_search(&self) {
        let self_ = self.imp();
        let Some(catalog) = self_.catalog.borrow().clone() else {
            return;
        };
        let serial = self_.search_serial.get() + 1;
        self_.search_serial.set(serial);
        let kind = match self_.type_dropdown.selected() {
            0 | gtk4::INVALID_LIST_POSITION => None,
            i => tags::TYPE_NAMES
                .get(i as usize - 1)
                .map(|k| (*k).to_string()),
        };
        let source = match self_.source_dropdown.selected() {
            1 => SourceFilter::Packs,
            2 => SourceFilter::Generated,
            _ => SourceFilter::All,
        };
        let family = match self_.pack_dropdown.selected() {
            0 | gtk4::INVALID_LIST_POSITION => None,
            i => self_.families.borrow().get(i as usize - 1).copied(),
        };
        let pack = family.map(|f| catalog.families[f].slug.clone());
        let engine = if self_.engine_check.is_active() {
            self.selected_project().and_then(|p| p.engine)
        } else {
            None
        };
        let exclude: Vec<String> = self_
            .hidden
            .borrow()
            .iter()
            .map(|h| h.slug.clone())
            .collect();
        let request = SearchRequest {
            exclude,
            text: self_.search_entry.text().trim().to_string(),
            kind,
            pack,
            family,
            engine,
            source,
            want: self_.want.get(),
        };
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let result = run_search(&catalog, &request);
            let _ = sender.send_blocking(Msg::Results(serial, result));
        });
    }

    fn show_results(&self, result: SearchResult) {
        let self_ = self.imp();
        let SearchResult {
            ids,
            total,
            more,
            note,
        } = result;
        {
            let mut shown = self_.shown.borrow_mut();
            let appended = ids.len() >= shown.len() && ids[..shown.len()] == shown[..];
            if appended && !shown.is_empty() {
                let tail: Vec<String> = ids[shown.len()..].iter().map(usize::to_string).collect();
                let strs: Vec<&str> = tail.iter().map(String::as_str).collect();
                self_.model.splice(self_.model.n_items(), 0, &strs);
            } else {
                let all: Vec<String> = ids.iter().map(usize::to_string).collect();
                let strs: Vec<&str> = all.iter().map(String::as_str).collect();
                let old = self_.model.n_items();
                self_.model.splice(0, old, &strs);
            }
            *shown = ids;
        }
        self_.more_button.set_visible(more);
        let shown = self_.shown.borrow().len();
        let catalog_total = self_
            .catalog
            .borrow()
            .as_ref()
            .map_or(0, |c| c.assets.len());
        let counts = if more {
            format!("Showing the first {shown} of {total} matches")
        } else {
            format!("{shown} matches")
        };
        let text = format!("{counts} ({catalog_total} assets in the library)");
        match note {
            Some(n) if shown == 0 => self_.count_label.set_label(&format!("{text} - {n}")),
            _ => self_.count_label.set_label(&text),
        }
        self.update_detail();
    }

    // ---------------------------------------------------------------- cards

    fn refresh_bound(&self) {
        let items: Vec<gtk4::ListItem> = self
            .imp()
            .bound
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect();
        for item in items {
            self.refresh_card(&item);
        }
        self.update_detail_image();
    }

    fn refresh_card(&self, item: &gtk4::ListItem) {
        let self_ = self.imp();
        let (Some(index), Some((check, image, name, sub)), Some(catalog)) = (
            item_asset(item),
            card_parts(item),
            self_.catalog.borrow().clone(),
        ) else {
            return;
        };
        let Some(asset) = catalog.assets.get(index) else {
            return;
        };
        name.set_label(&asset.name);
        name.set_tooltip_text(Some(&asset.game_path));
        let family = &catalog.families[asset.family];
        let detail = match asset.class.as_deref() {
            Some(class) => format!("{} | {class}", family.name),
            None => format!("{} | {}", family.name, asset.kind),
        };
        sub.set_label(&detail);
        self_.refreshing.set(true);
        check.set_active(self_.selected.borrow().contains(&index));
        self_.refreshing.set(false);
        self.set_thumbnail(&catalog, asset, &image);
    }

    /// Cached texture, else the kind icon while the thumbnail loads (or when there is none).
    fn set_thumbnail(&self, catalog: &Catalog, asset: &Asset, image: &gtk4::Image) {
        let Some(thumbs) = self.imp().thumbs.get() else {
            return;
        };
        match thumbs.texture(&asset.id) {
            Some(Some(texture)) => image.set_paintable(Some(&texture)),
            Some(None) => image.set_icon_name(Some(kind_icon(asset.kind))),
            None => {
                image.set_icon_name(Some(kind_icon(asset.kind)));
                if !asset.has_thumbnail || asset.source != SourceKind::Pack {
                    return;
                }
                let Some(release) = asset.releases.first().copied() else {
                    return;
                };
                let (Some(package), Some(pack)) =
                    (catalog.package(release), catalog.packs.get(release.pack))
                else {
                    return;
                };
                let entry = pack.entry.clone();
                let game_path = package.game_path.clone();
                thumbs.want(&asset.id, move || {
                    ue_asset_library::read_thumbnail(&entry, &game_path)
                        .map_err(|e| log::debug!("No thumbnail for {game_path}: {e}"))
                        .ok()
                        .flatten()
                });
            }
        }
    }

    // ---------------------------------------------------------------- hidden packs

    /// Right-click / long press on a card: offer to filter its pack out of the results.
    fn show_card_menu(&self, card: &gtk4::Box, asset: usize, x: f64, y: f64) {
        let Some(catalog) = self.imp().catalog.borrow().clone() else {
            return;
        };
        let Some(a) = catalog.assets.get(asset) else {
            return;
        };
        let family = a.family;
        let slug = catalog.families[family].slug.clone();
        let name = catalog.families[family].name.clone();
        let popover = gtk4::Popover::new();
        popover.set_parent(card);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let button = gtk4::Button::with_label(&format!("Filter out {}", truncate(&name, 40)));
        button.add_css_class("flat");
        button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                page.hide_pack(&slug, &name, family);
            }
        ));
        popover.set_child(Some(&button));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn hide_pack(&self, slug: &str, name: &str, family: usize) {
        let self_ = self.imp();
        {
            let mut hidden = self_.hidden.borrow_mut();
            if hidden.iter().any(|h| h.slug == slug) {
                return;
            }
            hidden.push(HiddenPack {
                slug: slug.to_string(),
                name: name.to_string(),
            });
            hidden_packs::save(&hidden);
        }
        // Assets of a hidden pack leave the selection too
        let catalog = self_.catalog.borrow().clone();
        if let Some(catalog) = catalog {
            let before = self_.selected.borrow().len();
            self_
                .selected
                .borrow_mut()
                .retain(|&i| catalog.assets.get(i).is_none_or(|a| a.family != family));
            if self_.selected.borrow().len() != before {
                self.after_selection_change();
            }
        }
        self.rebuild_hidden();
        self.new_search();
    }

    fn restore_packs(&self, slug: Option<&str>) {
        let self_ = self.imp();
        {
            let mut hidden = self_.hidden.borrow_mut();
            match slug {
                Some(s) => hidden.retain(|h| h.slug != s),
                None => hidden.clear(),
            }
            hidden_packs::save(&hidden);
        }
        self.rebuild_hidden();
        self.new_search();
    }

    /// The "N packs hidden" button and its popover (hidden entirely when nothing is hidden).
    fn rebuild_hidden(&self) {
        let self_ = self.imp();
        let hidden = self_.hidden.borrow().clone();
        while let Some(child) = self_.hidden_box.first_child() {
            self_.hidden_box.remove(&child);
        }
        self_.hidden_button.set_visible(!hidden.is_empty());
        if hidden.is_empty() {
            self_.hidden_button.popdown();
            return;
        }
        self_.hidden_button.set_label(&if hidden.len() == 1 {
            "1 pack hidden".to_string()
        } else {
            format!("{} packs hidden", hidden.len())
        });
        for pack in &hidden {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let label = gtk4::Label::new(Some(&pack.name));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            label.set_max_width_chars(36);
            let restore = gtk4::Button::from_icon_name("list-remove-symbolic");
            restore.add_css_class("flat");
            restore.set_tooltip_text(Some("Show this pack again"));
            let slug = pack.slug.clone();
            restore.connect_clicked(clone!(
                #[weak(rename_to=page)]
                self,
                move |_| page.restore_packs(Some(&slug))
            ));
            row.append(&label);
            row.append(&restore);
            self_.hidden_box.append(&row);
        }
        let all = gtk4::Button::with_label("Show all");
        all.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.restore_packs(None)
        ));
        self_.hidden_box.append(&all);
    }

    // ---------------------------------------------------------------- selection

    fn on_toggled(&self, asset: usize, active: bool) {
        {
            let mut selected = self.imp().selected.borrow_mut();
            if active {
                selected.insert(asset);
            } else {
                selected.remove(&asset);
            }
        }
        self.after_selection_change();
    }

    /// Update the footer at once and compute "required" and the size on a worker thread.
    fn after_selection_change(&self) {
        let self_ = self.imp();
        self_.summary.replace(None);
        let serial = self_.summary_serial.get() + 1;
        self_.summary_serial.set(serial);
        self.update_footer();
        let Some(catalog) = self_.catalog.borrow().clone() else {
            return;
        };
        let selected: Vec<usize> = self_.selected.borrow().iter().copied().collect();
        if selected.is_empty() {
            return;
        }
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let summary = compute_summary(&catalog, &selected);
            let _ = sender.send_blocking(Msg::Summary(serial, summary));
        });
    }

    fn update_footer(&self) {
        let self_ = self.imp();
        let Some(catalog) = self_.catalog.borrow().clone() else {
            self_.summary_label.set_label("Nothing selected");
            return;
        };
        let selected = self_.selected.borrow();
        let n = selected.len();
        self_.clear_button.set_sensitive(n > 0);
        let generated: Vec<&Asset> = selected
            .iter()
            .filter_map(|&i| catalog.assets.get(i))
            .filter(|a| a.source == SourceKind::Generated)
            .collect();
        self_
            .add_button
            .set_sensitive(n > 0 && generated.is_empty());
        let label = if n == 0 {
            "Nothing selected".to_string()
        } else {
            match self_.summary.borrow().as_ref() {
                Some(s) => format!(
                    "{} selected + {} required, {}",
                    s.selected,
                    s.required,
                    human_size(s.bytes)
                ),
                None => format!("{n} selected"),
            }
        };
        self_.summary_label.set_label(&label);
        match generated.first() {
            Some(a) => {
                let place = a
                    .generated
                    .as_ref()
                    .and_then(|g| {
                        g.unreal_object_path
                            .clone()
                            .or_else(|| g.output_path.clone())
                    })
                    .unwrap_or_default();
                self_.caption_label.set_label(&format!(
                    "Generated assets cannot be added to a project from here; they already live at {place}. Deselect them to add the rest."
                ));
                self_.caption_label.set_visible(true);
            }
            None if n > 0
                && self.selected_project().is_none()
                && self_.projects.borrow().is_empty() =>
            {
                self_
                    .caption_label
                    .set_label("No projects found. Add a projects directory in the preferences.");
                self_.caption_label.set_visible(true);
            }
            None => self_.caption_label.set_visible(false),
        }
    }

    fn open_add_dialog(&self) {
        let self_ = self.imp();
        let (Some(catalog), Some(window)) = (self_.catalog.borrow().clone(), self_.window.get())
        else {
            return;
        };
        let assets: Vec<usize> = self_.selected.borrow().iter().copied().collect();
        if assets.is_empty() {
            return;
        }
        // Fresh list: projects may have been added since the page was last shown.
        let projects = collect_projects(window);
        let default = self
            .selected_project()
            .map(|p| p.uproject)
            .or_else(|| {
                window
                    .imp()
                    .logged_in_stack
                    .imp()
                    .projects
                    .property::<Option<String>>("selected")
            })
            .and_then(|path| projects.iter().position(|p| p.uproject == path));
        add_dialog::EpicAssetsAddDialog::present_for(window, catalog, assets, projects, default);
    }

    // ---------------------------------------------------------------- detail pane

    fn focused_asset(&self) -> Option<usize> {
        self.imp()
            .selection
            .selected_item()
            .and_downcast::<gtk4::StringObject>()
            .and_then(|s| s.string().parse().ok())
    }

    fn update_detail(&self) {
        let self_ = self.imp();
        let (Some(index), Some(catalog)) = (self.focused_asset(), self_.catalog.borrow().clone())
        else {
            self_
                .detail_name
                .set_label("Select an asset to see its details");
            self_.detail_path.set_label("");
            self_.detail_text.set_label("");
            self_
                .detail_image
                .set_icon_name(Some("package-x-generic-symbolic"));
            return;
        };
        let Some(asset) = catalog.assets.get(index) else {
            return;
        };
        self_.detail_name.set_label(&asset.name);
        self_.detail_path.set_label(&asset.game_path);
        self_.detail_text.set_label(&detail_text(&catalog, asset));
        self.update_detail_image();
    }

    fn update_detail_image(&self) {
        let self_ = self.imp();
        let (Some(index), Some(catalog), Some(thumbs)) = (
            self.focused_asset(),
            self_.catalog.borrow().clone(),
            self_.thumbs.get(),
        ) else {
            return;
        };
        let Some(asset) = catalog.assets.get(index) else {
            return;
        };
        match thumbs.texture(&asset.id) {
            Some(Some(texture)) => self_.detail_image.set_paintable(Some(&texture)),
            _ => self_
                .detail_image
                .set_icon_name(Some(kind_icon(asset.kind))),
        }
    }
}

fn names(paths: &[String], max: usize) -> String {
    let mut out: Vec<&str> = paths
        .iter()
        .take(max)
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect();
    if paths.len() > max {
        out.push("...");
    }
    out.join(", ")
}

/// The text of the detail pane for one asset.
fn detail_text(catalog: &Catalog, asset: &Asset) -> String {
    let mut text = String::new();
    let class = asset.class.as_deref().unwrap_or("unknown class");
    text.push_str(&format!("Type: {} ({class})\n", type_label(asset.kind)));
    if asset.source == SourceKind::Generated {
        text.push_str("Source: generated (not importable here)\n");
        if let Some(g) = &asset.generated {
            if let Some(p) = &g.unreal_object_path {
                text.push_str(&format!("Object path: {p}\n"));
            }
            if let Some(p) = &g.output_path {
                text.push_str(&format!("Output: {p}\n"));
            }
            if let Some(p) = &g.target_project {
                text.push_str(&format!("Project: {p}\n"));
            }
            if let Some(p) = &g.completed_at {
                text.push_str(&format!("Completed: {p}\n"));
            }
        }
    } else {
        text.push_str(&format!("Size: {}\n", human_size(asset.size)));
        text.push_str(&format!("Pack: {}\n", catalog.families[asset.family].name));
        if !asset.releases.is_empty() {
            text.push_str("Releases:\n");
            for r in &asset.releases {
                let pack = &catalog.packs[r.pack];
                text.push_str(&format!(
                    "  {} (UE {})\n",
                    pack.display_name,
                    pack.engine_label().unwrap_or_else(|| "?".into())
                ));
            }
        }
    }
    if let Some(d) = &asset.description {
        text.push_str(&format!("\nDescription: {d}\n"));
    }
    let all_tags = catalog.all_tags(asset);
    if !all_tags.is_empty() {
        let shown: Vec<&str> = all_tags
            .iter()
            .take(MAX_LISTED_TAGS)
            .map(String::as_str)
            .collect();
        text.push_str(&format!(
            "\nTags: {}{}\n",
            shown.join(", "),
            if all_tags.len() > MAX_LISTED_TAGS {
                ", ..."
            } else {
                ""
            }
        ));
    }
    if let Some(release) = asset.releases.first() {
        if let (Some(package), Some(index)) = (
            catalog.package(*release),
            catalog.packs[release.pack].index.as_ref(),
        ) {
            let required =
                ue_asset_library::closure(index, std::slice::from_ref(&package.game_path))
                    .len()
                    .saturating_sub(1);
            text.push_str(&format!("\nRequired packages: {required}\n"));
            if !package.deps.is_empty() {
                text.push_str(&format!(
                    "Needs in this pack ({}): {}\n",
                    package.deps.len(),
                    names(&package.deps, MAX_LISTED_NAMES)
                ));
            }
            if !package.external.is_empty() {
                text.push_str(&format!(
                    "References outside the pack ({}): {}\n",
                    package.external.len(),
                    names(&package.external, MAX_LISTED_NAMES)
                ));
            }
            if package.header_unreadable {
                text.push_str("Its header could not be read; what it needs is unknown.\n");
            }
        }
    }
    text
}
