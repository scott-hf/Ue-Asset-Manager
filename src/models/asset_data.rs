use crate::models::fab_overlay::FabOverlay;
use crate::models::library_merge::{self, MatchKeys};
use crate::tools::asset_info::Search;
use chrono::{DateTime, Utc};
use diesel::dsl::exists;
use diesel::{select, ExpressionMethods, QueryDsl, RunQueryDsl};
use egs_api::api::types::asset_info::AssetInfo;
use gtk4::gdk::Texture;
use gtk4::prelude::ObjectExt;
use gtk4::prelude::SettingsExtManual;
use gtk4::{glib, subclass::prelude::*};
use log::trace;
use std::path::PathBuf;
use ue_asset_library::{Kind, LibraryEntry};

pub enum AssetType {
    Asset,
    Project,
    Game,
    Engine,
    Plugin,
}

/// A local library zip attached to a library entry (with its optional Fab overlay).
#[derive(Debug, Clone)]
pub struct LocalRef {
    pub entry: LibraryEntry,
    pub overlay: Option<FabOverlay>,
}

impl LocalRef {
    pub fn new(entry: LibraryEntry, overlay: Option<FabOverlay>) -> Self {
        Self { entry, overlay }
    }

    /// Identity keys used to match this zip against Epic/Fab library items.
    pub fn keys(&self) -> MatchKeys {
        let sc = &self.entry.sidecar;
        let mut names: Vec<&str> = vec![sc.display_name.as_str()];
        if let Some(friendly) = sc.friendly_name.as_deref() {
            names.push(friendly);
        }
        if let Some(overlay) = &self.overlay {
            names.push(overlay.title.as_str());
        }
        MatchKeys::new(
            self.overlay.iter().map(|o| o.listing_uid.clone()).collect(),
            &names,
        )
    }
}

/// Library category path of a local zip, so category filters and the sidebar also work for
/// entries that exist only locally.
fn local_category(kind: Kind) -> &'static str {
    match kind {
        Kind::Content => "assets",
        Kind::Plugin => "plugins",
        Kind::Project => "projects",
    }
}

fn local_kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::Content => "asset",
        Kind::Plugin => "plugins",
        Kind::Project => "projects",
    }
}

// Implementation sub-module of the GObject
mod imp {
    use super::*;
    use gtk4::gdk::Texture;
    use gtk4::glib::ParamSpecObject;
    use gtk4::prelude::ToValue;
    use std::cell::RefCell;

    // The actual data structure that stores our values. This is not accessible
    // directly from the outside.
    #[derive(Debug)]
    pub struct AssetData {
        id: RefCell<Option<String>>,
        name: RefCell<Option<String>>,
        favorite: RefCell<bool>,
        downloaded: RefCell<bool>,
        downloading: RefCell<bool>,
        download_progress: RefCell<f64>,
        download_speed: RefCell<String>,
        pub kind: RefCell<Option<String>>,
        pub asset: RefCell<Option<AssetInfo>>,
        /// The other Epic catalog items of the same asset (the Fab copy of a legacy item).
        pub twins: RefCell<Vec<AssetInfo>>,
        /// Local library zips of this asset.
        pub locals: RefCell<Vec<LocalRef>>,
        /// Cached identity keys of everything attached to this entry.
        pub keys: RefCell<MatchKeys>,
        thumbnail: RefCell<Option<Texture>>,
        pub settings: gtk4::gio::Settings,
    }

    // Basic declaration of our type for the GObject type system
    #[glib::object_subclass]
    impl ObjectSubclass for AssetData {
        const NAME: &'static str = "AssetData";
        type Type = super::AssetData;
        type ParentType = glib::Object;

        fn new() -> Self {
            Self {
                id: RefCell::new(None),
                name: RefCell::new(None),
                favorite: RefCell::new(false),
                downloaded: RefCell::new(false),
                downloading: RefCell::new(false),
                download_progress: RefCell::new(0.0),
                download_speed: RefCell::new(String::new()),
                kind: RefCell::new(None),
                asset: RefCell::new(None),
                twins: RefCell::new(Vec::new()),
                locals: RefCell::new(Vec::new()),
                keys: RefCell::new(MatchKeys::default()),
                thumbnail: RefCell::new(None),
                settings: gtk4::gio::Settings::new(crate::config::APP_ID),
            }
        }
    }

    // The ObjectImpl trait provides the setters/getters for GObject properties.
    // Here we need to provide the values that are internally stored back to the
    // caller, or store whatever new value the caller is providing.
    //
    // This maps between the GObject properties and our internal storage of the
    // corresponding values of the properties.
    impl ObjectImpl for AssetData {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: once_cell::sync::Lazy<Vec<glib::subclass::Signal>> =
                once_cell::sync::Lazy::new(|| {
                    vec![glib::subclass::Signal::builder("refreshed")
                        .flags(glib::SignalFlags::ACTION)
                        .build()]
                });
            SIGNALS.as_ref()
        }

        fn properties() -> &'static [glib::ParamSpec] {
            use once_cell::sync::Lazy;
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![
                    glib::ParamSpecString::builder("name").build(),
                    glib::ParamSpecString::builder("id").build(),
                    ParamSpecObject::builder::<Texture>("thumbnail").build(),
                    glib::ParamSpecBoolean::builder("favorite").build(),
                    glib::ParamSpecBoolean::builder("downloaded").build(),
                    glib::ParamSpecBoolean::builder("downloading").build(),
                    glib::ParamSpecDouble::builder("download-progress")
                        .minimum(0.0)
                        .maximum(1.0)
                        .default_value(0.0)
                        .build(),
                    glib::ParamSpecString::builder("download-speed").build(),
                ]
            });

            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "name" => {
                    let name = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.name.replace(name);
                }
                "id" => {
                    let id = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.id.replace(id);
                }
                "favorite" => {
                    let favorite = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.favorite.replace(favorite);
                }
                "downloaded" => {
                    let downloaded = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.downloaded.replace(downloaded);
                }
                "downloading" => {
                    let downloading = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.downloading.replace(downloading);
                }
                "download-progress" => {
                    let progress = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.download_progress.replace(progress);
                }
                "download-speed" => {
                    let speed: Option<String> = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.download_speed.replace(speed.unwrap_or_default());
                }
                "thumbnail" => {
                    let thumbnail = value
                        .get()
                        .expect("type conformity checked by `Object::set_property`");
                    self.thumbnail.replace(thumbnail);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "name" => self.name.borrow().to_value(),
                "id" => self.id.borrow().to_value(),
                "favorite" => self.favorite.borrow().to_value(),
                "downloaded" => self.downloaded.borrow().to_value(),
                "downloading" => self.downloading.borrow().to_value(),
                "download-progress" => self.download_progress.borrow().to_value(),
                "download-speed" => self.download_speed.borrow().to_value(),
                "thumbnail" => self.thumbnail.borrow().to_value(),
                _ => unimplemented!(),
            }
        }
    }
}

// Public part of the AssetData type. This behaves like a normal gtk-rs-style GObject
// binding
glib::wrapper! {
    pub struct AssetData(ObjectSubclass<imp::AssetData>);
}

// Constructor for new instances. This simply calls glib::Object::new() with
// initial values for our two properties and then returns the new instance
impl AssetData {
    pub fn new(asset: &AssetInfo, image: Option<Texture>) -> AssetData {
        let data: Self = glib::Object::new::<Self>();
        let self_ = data.imp();

        data.set_property("id", &asset.id);
        data.check_favorite();
        data.set_property("name", &asset.title);
        self_.asset.replace(Some(asset.clone()));
        data.check_downloaded();

        data.configure_kind(asset);
        data.refresh_keys();

        if let Some(tex) = image {
            data.set_property("thumbnail", tex);
        };
        data
    }

    /// An entry that exists only in the local library. `group_key` identifies the group of zips
    /// (cleaned name and kind); the id is `local:<group_key>`.
    pub fn new_local(group_key: &str, locals: Vec<LocalRef>) -> AssetData {
        let data: Self = glib::Object::new::<Self>();
        let self_ = data.imp();
        data.set_property("id", format!("local:{group_key}"));
        let first = locals.first().map(|l| l.entry.sidecar.clone());
        if let Some(sc) = &first {
            let name = locals
                .iter()
                .find_map(|l| l.overlay.as_ref().map(|o| o.title.clone()))
                .unwrap_or_else(|| sc.display_name.clone());
            data.set_property("name", name);
            self_
                .kind
                .replace(Some(local_kind_text(sc.kind).to_string()));
        }
        self_.locals.replace(locals);
        data.refresh_keys();
        data.check_favorite();
        data.check_downloaded();
        data
    }

    /// Whether this entry has Fab (Epic marketplace) content.
    pub fn has_fab(&self) -> bool {
        self.imp().asset.borrow().is_some()
    }

    /// Whether this entry has a local library zip.
    pub fn has_local(&self) -> bool {
        !self.imp().locals.borrow().is_empty()
    }

    /// The Epic catalog items of this entry, the primary one (with the thumbnail) first.
    pub fn epic_items(&self) -> Vec<AssetInfo> {
        let self_ = self.imp();
        let mut items: Vec<AssetInfo> = self_.asset.borrow().iter().cloned().collect();
        items.extend(self_.twins.borrow().iter().cloned());
        items
    }

    /// Ids of every Epic catalog item of this entry.
    pub fn all_ids(&self) -> Vec<String> {
        let self_ = self.imp();
        let mut ids: Vec<String> = self_.asset.borrow().iter().map(|a| a.id.clone()).collect();
        ids.extend(self_.twins.borrow().iter().map(|a| a.id.clone()));
        if ids.is_empty() {
            ids.push(self.id());
        }
        ids
    }

    pub fn locals(&self) -> Vec<LocalRef> {
        self.imp().locals.borrow().clone()
    }

    /// Replace the attached local zips. Returns whether anything changed.
    pub fn set_locals(&self, locals: Vec<LocalRef>) -> bool {
        let self_ = self.imp();
        let same = {
            let old = self_.locals.borrow();
            old.len() == locals.len()
                && old
                    .iter()
                    .zip(&locals)
                    .all(|(a, b)| a.entry.zip_path == b.entry.zip_path && a.overlay == b.overlay)
        };
        if same {
            return false;
        }
        self_.locals.replace(locals);
        self.refresh_keys();
        self.emit_by_name::<()>("refreshed", &[]);
        true
    }

    /// Identity keys of everything attached to this entry.
    pub fn match_keys(&self) -> MatchKeys {
        self.imp().keys.borrow().clone()
    }

    fn refresh_keys(&self) {
        let self_ = self.imp();
        let mut listing_ids = Vec::new();
        let mut titles: Vec<String> = Vec::new();
        for item in self.epic_items() {
            listing_ids.extend(library_merge::epic_listing_id(&item));
            titles.extend(item.title.clone());
        }
        // The keys of an entry with an Epic/Fab item come from that item only, so the zips that
        // are attached to it do not influence which zips match it. An entry that exists only in
        // the local library is identified by its zips.
        if self_.asset.borrow().is_none() {
            for local in self_.locals.borrow().iter() {
                listing_ids.extend(local.keys().listing_ids);
                titles.push(local.entry.sidecar.display_name.clone());
            }
        }
        let names: Vec<&str> = titles.iter().map(String::as_str).collect();
        self_.keys.replace(MatchKeys::new(listing_ids, &names));
    }

    /// Add the second catalog item of the same asset (the Fab copy of a legacy item, or the other
    /// way round). The item that has a thumbnail becomes the primary one. `image` is the
    /// already loaded thumbnail of `twin`, if it has one.
    pub fn add_twin(&self, twin: &AssetInfo, image: Option<Texture>) {
        let self_ = self.imp();
        let primary_has_image = self_
            .asset
            .borrow()
            .as_ref()
            .is_some_and(|a| a.thumbnail().is_some());
        if twin.thumbnail().is_some() && !primary_has_image {
            let old = self_.asset.replace(Some(twin.clone()));
            if let Some(old) = old {
                self_.twins.borrow_mut().push(old);
            }
            self.set_property("id", &twin.id);
            self.set_property("name", &twin.title);
            self.configure_kind(twin);
        } else {
            self_.twins.borrow_mut().push(twin.clone());
            if self_.kind.borrow().is_none() {
                self.configure_kind(twin);
            }
        }
        if self.image().is_none() {
            if let Some(tex) = image {
                self.set_property("thumbnail", tex);
            }
        }
        self.refresh_keys();
        self.check_downloaded();
        self.check_favorite();
        self.emit_by_name::<()>("refreshed", &[]);
    }

    /// Use `texture` as the thumbnail when there is none yet (the Fab library knows the picture of
    /// items whose Epic catalog entry has none). Returns whether it was applied.
    pub fn adopt_thumbnail(&self, texture: &Texture) -> bool {
        if self.image().is_some() {
            return false;
        }
        self.set_property("thumbnail", texture.clone());
        self.emit_by_name::<()>("refreshed", &[]);
        true
    }

    pub fn decide_kind(asset: &AssetInfo) -> Option<AssetType> {
        if let Some(cat) = &asset.categories {
            for c in cat {
                match c.path.as_str() {
                    "assets" => {
                        return Some(AssetType::Asset);
                    }
                    "games" => {
                        return Some(AssetType::Game);
                    }
                    "plugins" => {
                        return Some(AssetType::Plugin);
                    }
                    "projects" => {
                        return Some(AssetType::Project);
                    }
                    "engines" => {
                        return Some(AssetType::Engine);
                    }
                    _ => {}
                };
            }
        };
        None
    }

    fn configure_kind(&self, asset: &AssetInfo) {
        let self_ = self.imp();
        Self::decide_kind(asset).map_or_else(
            || {
                self_.kind.replace(None);
            },
            |kind| match kind {
                AssetType::Asset => {
                    self_.kind.replace(Some("asset".to_string()));
                }
                AssetType::Project => {
                    self_.kind.replace(Some("projects".to_string()));
                }
                AssetType::Game => {
                    self_.kind.replace(Some("games".to_string()));
                }
                AssetType::Engine => {
                    self_.kind.replace(Some("engines".to_string()));
                }
                AssetType::Plugin => {
                    self_.kind.replace(Some("plugins".to_string()));
                }
            },
        );
    }

    pub fn id(&self) -> String {
        self.property("id")
    }

    pub fn name(&self) -> String {
        self.property("name")
    }

    pub fn favorite(&self) -> bool {
        self.property("favorite")
    }

    pub fn downloaded(&self) -> bool {
        self.property("downloaded")
    }

    fn local_date(&self) -> Option<DateTime<Utc>> {
        self.imp()
            .locals
            .borrow()
            .iter()
            .filter_map(|l| DateTime::parse_from_rfc3339(&l.entry.sidecar.sanitized_at).ok())
            .map(|d| d.with_timezone(&Utc))
            .max()
    }

    pub fn release(&self) -> Option<DateTime<Utc>> {
        let self_ = self.imp();
        (*self_.asset.borrow())
            .as_ref()
            .and_then(|a| match a.latest_release() {
                None => a.last_modified_date,
                Some(ri) => ri.date_added,
            })
            .or_else(|| self.local_date())
    }

    pub fn kind(&self) -> Option<AssetType> {
        let self_ = self.imp();
        (*self_.kind.borrow())
            .as_ref()
            .and_then(|a| match a.as_str() {
                "asset" => Some(AssetType::Asset),
                "games" => Some(AssetType::Game),
                "plugins" => Some(AssetType::Plugin),
                "projects" => Some(AssetType::Project),
                "engines" => Some(AssetType::Engine),
                _ => None,
            })
    }

    pub fn last_modified(&self) -> Option<DateTime<Utc>> {
        let self_ = self.imp();
        (*self_.asset.borrow())
            .as_ref()
            .and_then(|a| a.last_modified_date)
            .or_else(|| self.local_date())
    }

    pub fn image(&self) -> Option<Texture> {
        self.property("thumbnail")
    }

    fn has_category(&self, cat: &str) -> bool {
        if cat.eq("favorites") {
            self.favorite()
        } else if cat.eq("downloaded") {
            self.downloaded()
        } else {
            let self_ = self.imp();
            let cat = cat.to_ascii_lowercase();
            let matches = |info: &AssetInfo| {
                info.categories.as_ref().is_some_and(|categories| {
                    categories
                        .iter()
                        .any(|c| c.path.to_ascii_lowercase().contains(&cat))
                })
            };
            if let Some(b) = self_.asset.borrow().as_ref() {
                if matches(b) {
                    return true;
                }
            }
            for twin in self_.twins.borrow().iter() {
                if matches(twin) {
                    return true;
                }
            }
            // Entries that exist only locally are categorized by the kind of their zip.
            if self_.asset.borrow().is_none() {
                let locals = self_.locals.borrow();
                return locals.iter().any(|l| {
                    local_category(l.entry.sidecar.kind)
                        .to_ascii_lowercase()
                        .contains(&cat)
                });
            }
            false
        }
    }

    pub fn check_category(&self, cat: &str) -> bool {
        crate::tools::category_filter::eval_category_expr(cat, &|c| self.has_category(c))
    }

    pub fn check_downloaded(&self) {
        let self_ = self.imp();
        let vaults = self_.settings.strv("unreal-vault-directories");
        for ass in self.epic_items() {
            if let Some(ris) = &ass.release_info {
                for ri in ris {
                    if let Some(app) = &ri.app_id {
                        if !Self::downloaded_locations(&vaults, app).is_empty() {
                            self.set_property("downloaded", true);
                            return;
                        }
                    }
                }
            }
        }
        // A local zip is ready to be added to a project, like a downloaded vault item.
        self.set_property(
            "downloaded",
            self_.asset.borrow().is_none() && !self_.locals.borrow().is_empty(),
        );
    }

    pub fn downloaded_locations(directories: &glib::StrV, asset_id: &str) -> Vec<PathBuf> {
        let mut result: Vec<PathBuf> = Vec::new();
        for directory in directories {
            let mut path = std::path::PathBuf::from(directory.as_str());
            path.push(asset_id);
            path.push("data");
            if path.exists() {
                result.push(path);
            }
        }
        result
    }

    pub fn check_favorite(&self) {
        let db = crate::models::database::connection();
        if let Ok(mut conn) = db.get() {
            let ex: Result<bool, diesel::result::Error> = select(exists(
                crate::schema::favorite_asset::table
                    .filter(crate::schema::favorite_asset::asset.eq_any(self.all_ids())),
            ))
            .get_result(&mut conn);
            if let Ok(fav) = ex {
                self.set_property("favorite", fav);
                return;
            }
        }
        self.set_property("favorite", false);
    }

    pub fn refresh(&self) {
        self.check_favorite();
        self.check_downloaded();
        self.emit_by_name::<()>("refreshed", &[]);
    }

    pub fn downloading(&self) -> bool {
        self.property("downloading")
    }

    pub fn set_downloading(&self, downloading: bool) {
        trace!(
            "AssetData.set_downloading: id={}, downloading={}",
            self.id(),
            downloading
        );
        self.set_property("downloading", downloading);
        trace!("AssetData.set_downloading: emitting refreshed signal");
        self.emit_by_name::<()>("refreshed", &[]);
    }

    pub fn download_progress(&self) -> f64 {
        self.property("download-progress")
    }

    pub fn set_download_progress(&self, progress: f64) {
        self.set_property("download-progress", progress);
        self.emit_by_name::<()>("refreshed", &[]);
    }

    pub fn download_speed(&self) -> String {
        self.property("download-speed")
    }

    pub fn set_download_speed(&self, speed: &str) {
        self.set_property("download-speed", speed);
    }

    /// Set both progress and speed together, emitting only one refresh signal
    pub fn set_download_info(&self, progress: f64, speed: &str) {
        self.set_property("download-progress", progress);
        self.set_property("download-speed", speed);
        self.emit_by_name::<()>("refreshed", &[]);
    }
}
