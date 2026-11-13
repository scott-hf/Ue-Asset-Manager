//! One dialog shared by the Epic library, Fab downloads and local library imports.
//!
//! It asks which project the asset should be added to (defaulting to the project
//! selected on the Projects page, or "vault only") and which version to use, then
//! hands the request to the download manager (Fab releases) or imports the local
//! library zip into the project.
//!
//! The version list offers every source of the asset: Fab releases and local zips,
//! each labeled with its source. It is sorted newest engine version first; at the same
//! version Fab comes before Local and is the default. The project's engine version then
//! picks the entry (exact match, else the highest not above it, else the lowest).

use crate::models::asset_data::{AssetData, AssetType};
use crate::models::install_log::{self, InstallRecord};
use crate::models::library_merge::{self as merge, MatchKeys, Source};
use crate::models::project_data::ProjectData;
use crate::models::project_registry::{self as registry, VersionSource};
use crate::ui::widgets::download_manager::asset::Asset;
use crate::ui::widgets::download_manager::{EpicDownloadManager, PostDownloadAction};
use crate::window::EpicAssetManagerWindow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use egs_api::api::types::asset_info::AssetInfo;
use egs_api::api::types::fab_library::FabAsset;
use gtk4::glib::clone;
use gtk4::{self, gio, glib, CompositeTemplate, StringList};
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Mutex;
use ue_asset_library::{ImportPolicy, Kind, LibraryEntry};

/// Last project choice made in this session (`Some(None)` = vault only).
static LAST_PROJECT: Mutex<Option<Option<String>>> = Mutex::new(None);

/// (display name, .uproject path, resolved engine version and where it came from)
type ProjectEntry = (String, String, Option<(String, VersionSource)>);

/// Everything that is known about the asset the dialog is opened for: the Epic catalog items
/// (a legacy item and its Fab copy), the Fab library asset and the local library zips.
#[derive(Debug, Clone, Default)]
pub struct Subject {
    pub title: String,
    /// Epic catalog items, the one with the thumbnail first.
    pub epic: Vec<AssetInfo>,
    pub fab: Option<FabAsset>,
    /// Local library zips (never of kind `Project`).
    pub local: Vec<LibraryEntry>,
    /// Plugins go to `<project>/Plugins/<PluginName>/` only.
    pub is_plugin: bool,
    pub show_projects: bool,
    /// Opened from the Fab page: Fab library releases are listed before the Epic ones.
    pub fab_first: bool,
}

impl Subject {
    fn new(
        epic: Vec<AssetInfo>,
        fab: Option<FabAsset>,
        local: Vec<LibraryEntry>,
        fab_first: bool,
    ) -> Self {
        let local: Vec<LibraryEntry> = local
            .into_iter()
            .filter(|e| e.sidecar.kind != Kind::Project)
            .collect();
        let title = epic
            .first()
            .and_then(|a| a.title.clone())
            .or_else(|| fab.as_ref().map(|f| f.title.clone()))
            .or_else(|| local.first().map(|l| l.sidecar.display_name.clone()))
            .unwrap_or_default();
        let kinds: Vec<Option<AssetType>> = epic.iter().map(AssetData::decide_kind).collect();
        let is_plugin = kinds.iter().any(|k| matches!(k, Some(AssetType::Plugin)));
        let show_projects = fab.is_some()
            || !local.is_empty()
            || kinds.iter().any(|k| {
                matches!(
                    k,
                    Some(AssetType::Asset | AssetType::Project | AssetType::Plugin)
                )
            });
        Self {
            title,
            epic,
            fab,
            local,
            is_plugin,
            show_projects,
            fab_first,
        }
    }

    /// Ids and cleaned names under which installs of this asset are logged.
    fn log_keys(&self) -> (Vec<String>, Vec<String>) {
        let mut ids: Vec<String> = self.epic.iter().map(|a| a.id.clone()).collect();
        ids.extend(self.fab.iter().map(|f| f.asset_id.clone()));
        ids.extend(self.local.iter().map(|l| l.sidecar.sha256.clone()));
        let mut names: Vec<&str> = vec![self.title.as_str()];
        names.extend(self.local.iter().map(|l| l.sidecar.display_name.as_str()));
        (ids, MatchKeys::new(Vec::new(), &names).name_keys)
    }
}

/// Where the content of a version entry comes from.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Origin {
    /// A release of an Epic catalog item (downloaded from the Epic marketplace).
    Epic(AssetInfo),
    /// An artifact of a Fab library asset.
    Fab(FabAsset),
    /// A zip of the local library.
    Local(LibraryEntry),
}

/// One selectable version: release id (Epic), artifact id (Fab) or sha256 (local zip).
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Dropdown text without the project-dependent suffix, starting with the source tag
    pub label: String,
    pub id: String,
    /// Normalized engine versions ("5.3")
    pub engines: Vec<String>,
    platforms: Vec<String>,
    pub source: Source,
    pub origin: Origin,
}

impl VersionEntry {
    /// (major, minor) of every engine version this release supports.
    pub fn tuples(&self) -> Vec<(u32, u32)> {
        merge::engine_tuples(self.engines.iter().map(String::as_str))
    }
}

/// Reduce "UE_5.3", "5.3.2" or "{GUID}" to "5.3" (or `None` when it is not a version).
fn normalize_engine(raw: &str) -> Option<String> {
    registry::normalize_version(raw)
}

#[cfg(target_os = "windows")]
const DEFAULT_PLATFORM: &str = "Windows";
#[cfg(target_os = "linux")]
const DEFAULT_PLATFORM: &str = "Linux";
#[cfg(target_os = "macos")]
const DEFAULT_PLATFORM: &str = "Mac";
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
const DEFAULT_PLATFORM: &str = "Windows";

pub mod imp {
    use super::*;
    use adw::subclass::dialog::AdwDialogImpl;
    use once_cell::sync::OnceCell;

    #[derive(Debug, CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/download_dialog.ui")]
    pub struct EpicDownloadDialog {
        pub subject: RefCell<Option<Subject>>,
        pub window: OnceCell<EpicAssetManagerWindow>,
        pub download_manager: OnceCell<EpicDownloadManager>,
        /// Index 0 is always "vault only" (`None`); the rest are `.uproject` paths.
        pub project_paths: RefCell<Vec<Option<String>>>,
        /// Display name per project entry (empty for "vault only").
        pub project_names: RefCell<Vec<String>>,
        /// Normalized engine association per project entry.
        pub project_engines: RefCell<Vec<Option<(String, VersionSource)>>>,
        pub versions: RefCell<Vec<VersionEntry>>,
        pub vault_dirs: RefCell<Vec<String>>,
        /// The install log as it was when the dialog opened.
        pub install_records: RefCell<Vec<InstallRecord>>,
        #[template_child]
        pub asset_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub installed_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub project_box: TemplateChild<gtk4::Box>,
        #[template_child]
        pub project_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub version_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub platform_box: TemplateChild<gtk4::Box>,
        #[template_child]
        pub platform_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub vault_box: TemplateChild<gtk4::Box>,
        #[template_child]
        pub vault_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub overwrite_box: TemplateChild<gtk4::Box>,
        #[template_child]
        pub overwrite_switch: TemplateChild<gtk4::Switch>,
        #[template_child]
        pub hint_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub cancel_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub download_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicDownloadDialog {
        const NAME: &'static str = "EpicDownloadDialog";
        type Type = super::EpicDownloadDialog;
        type ParentType = adw::Dialog;

        fn new() -> Self {
            Self {
                subject: RefCell::new(None),
                window: OnceCell::new(),
                download_manager: OnceCell::new(),
                project_paths: RefCell::new(Vec::new()),
                project_names: RefCell::new(Vec::new()),
                project_engines: RefCell::new(Vec::new()),
                versions: RefCell::new(Vec::new()),
                vault_dirs: RefCell::new(Vec::new()),
                install_records: RefCell::new(Vec::new()),
                asset_label: TemplateChild::default(),
                installed_label: TemplateChild::default(),
                project_box: TemplateChild::default(),
                project_dropdown: TemplateChild::default(),
                version_dropdown: TemplateChild::default(),
                platform_box: TemplateChild::default(),
                platform_dropdown: TemplateChild::default(),
                vault_box: TemplateChild::default(),
                vault_dropdown: TemplateChild::default(),
                overwrite_box: TemplateChild::default(),
                overwrite_switch: TemplateChild::default(),
                hint_label: TemplateChild::default(),
                cancel_button: TemplateChild::default(),
                download_button: TemplateChild::default(),
            }
        }

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for EpicDownloadDialog {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_events();
        }
    }

    impl WidgetImpl for EpicDownloadDialog {}
    impl AdwDialogImpl for EpicDownloadDialog {}
}

glib::wrapper! {
    pub struct EpicDownloadDialog(ObjectSubclass<imp::EpicDownloadDialog>)
        @extends gtk4::Widget, adw::Dialog,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::ShortcutManager;
}

impl EpicDownloadDialog {
    /// Ask where to put an Epic library asset and which release (or local zip) to use.
    pub fn present_epic(
        window: &EpicAssetManagerWindow,
        dm: &EpicDownloadManager,
        asset: &AssetInfo,
    ) -> Self {
        let keys = MatchKeys::new(
            merge::epic_listing_id(asset).into_iter().collect(),
            &[asset.title.as_deref().unwrap_or_default()],
        );
        let logged_in = window.imp().logged_in_stack.clone();
        let (mut epic, local) = logged_in
            .imp()
            .library
            .dialog_parts(std::slice::from_ref(&asset.id), &keys);
        if !epic.iter().any(|a| a.id == asset.id) {
            epic.insert(0, asset.clone());
        }
        let dialog = Self::build(window, dm, Subject::new(epic, None, local, false));
        dialog.present_on(window);
        dialog
    }

    /// Ask where to put a Fab asset and which version / platform to download.
    pub fn present_fab(
        window: &EpicAssetManagerWindow,
        dm: &EpicDownloadManager,
        asset: &FabAsset,
    ) -> Self {
        let keys = MatchKeys::new(
            merge::fab_listing_id(asset).into_iter().collect(),
            &[asset.title.as_str()],
        );
        let logged_in = window.imp().logged_in_stack.clone();
        let (epic, local) = logged_in.imp().library.dialog_parts(&[], &keys);
        let dialog = Self::build(
            window,
            dm,
            Subject::new(epic, Some(asset.clone()), local, true),
        );
        dialog.present_on(window);
        dialog
    }

    /// Ask for a library entry as shown on the library page (marketplace items and/or zips).
    pub fn present_entry(
        window: &EpicAssetManagerWindow,
        dm: &EpicDownloadManager,
        data: &AssetData,
    ) -> Self {
        let local = data.locals().into_iter().map(|l| l.entry).collect();
        let dialog = Self::build(
            window,
            dm,
            Subject::new(data.epic_items(), None, local, false),
        );
        dialog.present_on(window);
        dialog
    }

    fn present_on(&self, window: &EpicAssetManagerWindow) {
        adw::prelude::AdwDialogExt::present(self, Some(window));
    }

    fn build(window: &EpicAssetManagerWindow, dm: &EpicDownloadManager, subject: Subject) -> Self {
        let dialog: Self = glib::Object::new();
        let self_ = dialog.imp();
        let _ = self_.window.set(window.clone());
        let _ = self_.download_manager.set(dm.clone());

        let show_projects = subject.show_projects;
        self_.asset_label.set_label(&subject.title);
        self_.subject.replace(Some(subject));
        self_
            .install_records
            .replace(install_log::load(&install_log::default_path()));

        dialog.setup_versions();
        dialog.setup_vaults();
        dialog.setup_projects(show_projects);
        dialog
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
        self_.download_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                dialog.confirm();
            }
        ));
        self_.project_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                dialog.on_project_changed();
            }
        ));
        self_.version_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                dialog.update_platforms();
                dialog.update_action_state();
            }
        ));
    }

    /// The version entries of the subject: Fab releases, then local zips, sorted newest engine
    /// first with Fab before Local at the same version.
    fn collect_versions(subject: &Subject) -> Vec<VersionEntry> {
        let mut entries: Vec<VersionEntry> = Vec::new();
        // Release ids are shared by the legacy item, its Fab copy and the Fab library asset
        let mut seen: HashSet<String> = HashSet::new();

        let push_epic = |entries: &mut Vec<VersionEntry>, seen: &mut HashSet<String>| {
            for asset in &subject.epic {
                let Some(releases) = asset.sorted_releases() else {
                    continue;
                };
                for release in releases {
                    let Some(id) = release.app_id.clone() else {
                        continue;
                    };
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    let raw_engines: Vec<String> = release
                        .compatible_apps
                        .clone()
                        .unwrap_or_default()
                        .iter()
                        .map(|e| e.trim_start_matches("UE_").to_string())
                        .collect();
                    let mut label = release
                        .version_title
                        .clone()
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| id.clone());
                    if !raw_engines.is_empty() {
                        label = format!("{} (UE {})", label, raw_engines.join(", "));
                    }
                    entries.push(VersionEntry {
                        label: format!("[{}] {label}", Source::Fab.label()),
                        id,
                        engines: raw_engines
                            .iter()
                            .filter_map(|e| normalize_engine(e))
                            .collect(),
                        platforms: Vec::new(),
                        source: Source::Fab,
                        origin: Origin::Epic(asset.clone()),
                    });
                }
            }
        };
        let push_fab = |entries: &mut Vec<VersionEntry>, seen: &mut HashSet<String>| {
            let Some(asset) = &subject.fab else {
                return;
            };
            for pv in &asset.project_versions {
                if !seen.insert(pv.artifact_id.clone()) {
                    continue;
                }
                let raw_engines: Vec<String> = pv
                    .engine_versions
                    .iter()
                    .map(|e| e.trim_start_matches("UE_").to_string())
                    .collect();
                let label = if raw_engines.is_empty() {
                    pv.artifact_id.clone()
                } else {
                    format!("UE {}", raw_engines.join(", "))
                };
                entries.push(VersionEntry {
                    label: format!("[{}] {label}", Source::Fab.label()),
                    id: pv.artifact_id.clone(),
                    engines: raw_engines
                        .iter()
                        .filter_map(|e| normalize_engine(e))
                        .collect(),
                    platforms: pv.target_platforms.clone(),
                    source: Source::Fab,
                    origin: Origin::Fab(asset.clone()),
                });
            }
        };
        if subject.fab_first {
            push_fab(&mut entries, &mut seen);
            push_epic(&mut entries, &mut seen);
        } else {
            push_epic(&mut entries, &mut seen);
            push_fab(&mut entries, &mut seen);
        }

        for entry in &subject.local {
            let sc = &entry.sidecar;
            let engines: Vec<String> = sc
                .engine_version_guess
                .iter()
                .filter_map(|e| normalize_engine(e))
                .collect();
            let label = match sc.engine_version_guess.as_deref() {
                Some(engine) => format!(
                    "[{}] {} (UE {engine})",
                    Source::Local.label(),
                    sc.display_name
                ),
                None => format!("[{}] {}", Source::Local.label(), sc.display_name),
            };
            entries.push(VersionEntry {
                label,
                id: sc.sha256.clone(),
                engines,
                platforms: Vec::new(),
                source: Source::Local,
                origin: Origin::Local(entry.clone()),
            });
        }

        merge::sort_versions(&mut entries, |e| (e.tuples().into_iter().max(), e.source));
        entries
    }

    fn setup_versions(&self) {
        let self_ = self.imp();
        let entries = self
            .imp()
            .subject
            .borrow()
            .as_ref()
            .map(Self::collect_versions)
            .unwrap_or_default();

        let has_versions = !entries.is_empty();
        self_.versions.replace(entries);
        self.refresh_version_labels(None, None);
        if has_versions {
            self_.version_dropdown.set_selected(0);
        } else {
            self_.download_button.set_sensitive(false);
            self_
                .hint_label
                .set_label("No downloadable version is available.");
        }
        self.update_platforms();
    }

    /// Rebuild the version dropdown text. With a project engine, releases newer than it are marked
    /// and the recommended one is flagged; without, the first entry is the latest.
    fn refresh_version_labels(&self, project: Option<(u32, u32)>, recommended: Option<usize>) {
        let self_ = self.imp();
        let previous = self_.version_dropdown.selected();
        let labels: Vec<String> = self_
            .versions
            .borrow()
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut label = v.label.clone();
                match project {
                    None if i == 0 => label.push_str(" - latest"),
                    None => {}
                    Some(p) => {
                        if registry::release_is_newer(p, &v.tuples()) {
                            label.push_str(&format!(
                                " - newer than the project (UE {}.{})",
                                p.0, p.1
                            ));
                        }
                        if recommended == Some(i) {
                            label.push_str(" - recommended");
                        }
                    }
                }
                label
            })
            .collect();
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self_
            .version_dropdown
            .set_model(Some(&StringList::new(&strs)));
        if previous != gtk4::INVALID_LIST_POSITION && (previous as usize) < labels.len() {
            self_.version_dropdown.set_selected(previous);
        }
    }

    fn selected_version(&self) -> Option<VersionEntry> {
        let self_ = self.imp();
        let idx = self_.version_dropdown.selected();
        if idx == gtk4::INVALID_LIST_POSITION {
            return None;
        }
        self_.versions.borrow().get(idx as usize).cloned()
    }

    /// The platform list only applies to Fab library releases.
    fn update_platforms(&self) {
        let self_ = self.imp();
        let platforms = self
            .selected_version()
            .filter(|v| matches!(v.origin, Origin::Fab(_)))
            .map(|v| v.platforms)
            .unwrap_or_default();
        self_.platform_box.set_visible(!platforms.is_empty());
        let strs: Vec<&str> = platforms.iter().map(String::as_str).collect();
        self_
            .platform_dropdown
            .set_model(Some(&StringList::new(&strs)));
        if let Some(pos) = platforms.iter().position(|p| p == DEFAULT_PLATFORM) {
            self_.platform_dropdown.set_selected(pos as u32);
        }
    }

    /// A local zip can only be imported into a project; everything else can go to the vault.
    fn update_action_state(&self) {
        let self_ = self.imp();
        let Some(version) = self.selected_version() else {
            return;
        };
        let has_project = self.selected_project_path().is_some();
        if has_project {
            self_.download_button.set_sensitive(true);
            return;
        }
        if matches!(version.origin, Origin::Local(_)) {
            self_.download_button.set_sensitive(false);
            self_
                .hint_label
                .set_label("A local library zip can only be added to a project: choose a project.");
        } else {
            self_.download_button.set_sensitive(true);
            self_
                .hint_label
                .set_label("The asset is downloaded to the vault only.");
        }
    }

    /// "Installed on MM/DD/YYYY [in <project>]": the selected project's most recent install when
    /// there is one, else the most recent install anywhere.
    fn update_installed_label(&self) {
        let self_ = self.imp();
        let Some((ids, names)) = self.imp().subject.borrow().as_ref().map(Subject::log_keys) else {
            return;
        };
        let project = self.selected_project_path();
        let records = self_.install_records.borrow();
        match install_log::latest_for(&records, &ids, &names, project.as_deref()) {
            Some(record) => {
                self_
                    .installed_label
                    .set_label(&install_log::installed_line(record, &chrono::Local));
                self_.installed_label.set_visible(true);
            }
            None => self_.installed_label.set_visible(false),
        }
    }

    fn setup_vaults(&self) {
        let self_ = self.imp();
        let settings = gio::Settings::new(crate::config::APP_ID);
        let dirs: Vec<String> = settings
            .strv("unreal-vault-directories")
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let labels: Vec<String> = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                if i == 0 {
                    format!("{d} (default)")
                } else {
                    d.clone()
                }
            })
            .collect();
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self_
            .vault_dropdown
            .set_model(Some(&StringList::new(&strs)));
        if !dirs.is_empty() {
            self_.vault_dropdown.set_selected(0);
        }
        self_.vault_dirs.replace(dirs);
    }

    fn setup_projects(&self, show_projects: bool) {
        let self_ = self.imp();
        let mut projects: Vec<ProjectEntry> = Vec::new();
        if show_projects {
            if let Some(window) = self_.window.get() {
                let settings = gio::Settings::new(crate::config::APP_ID);
                let logged_in = window.imp().logged_in_stack.clone();
                let engines_box = logged_in.imp().engines.clone();
                let projects_widget = logged_in.imp().projects.imp();
                let model = &projects_widget.grid_model;
                for i in 0..model.n_items() {
                    let Some(data) = model
                        .item(i)
                        .and_then(|item| item.downcast::<ProjectData>().ok())
                    else {
                        continue;
                    };
                    let Some(path) = data.path() else {
                        continue;
                    };
                    // Override > numeric EngineAssociation > installed engine (by GUID)
                    let association = data
                        .uproject()
                        .map(|u| u.engine_association)
                        .unwrap_or_default();
                    let engine =
                        registry::resolve_engine_version(&settings, &path, &association, || {
                            engines_box
                                .engine_from_assoociation(&association)
                                .and_then(|e| {
                                    e.version.valid().then(|| {
                                        format!(
                                            "{}.{}",
                                            e.version.major_version, e.version.minor_version
                                        )
                                    })
                                })
                        });
                    projects.push((data.name().unwrap_or_else(|| path.clone()), path, engine));
                }
            }
        }
        projects.sort_by_key(|(name, _, _)| name.to_lowercase());

        let selected_path: Option<String> = if show_projects {
            self_.window.get().and_then(|window| {
                let logged_in = window.imp().logged_in_stack.clone();
                let projects_widget = logged_in.imp().projects.clone();
                projects_widget.property::<Option<String>>("selected")
            })
        } else {
            None
        };

        let mut labels = vec!["Don't add to a project (vault only)".to_string()];
        let mut names = vec![String::new()];
        let mut paths: Vec<Option<String>> = vec![None];
        let mut engines: Vec<Option<(String, VersionSource)>> = vec![None];
        for (name, path, engine) in projects {
            labels.push(name.clone());
            names.push(name);
            paths.push(Some(path));
            engines.push(engine);
        }

        // Selected project first, then the last choice in this session, then the first project.
        let last = LAST_PROJECT.lock().ok().and_then(|l| l.clone());
        let default_idx = selected_path
            .as_ref()
            .and_then(|sel| paths.iter().position(|p| p.as_ref() == Some(sel)))
            .or_else(|| match &last {
                Some(choice) => paths.iter().position(|p| p == choice),
                None => None,
            })
            .or(if paths.len() > 1 { Some(1) } else { None })
            .unwrap_or(0);
        debug!(
            "Download dialog project default: selected={:?} last={:?} -> index {}",
            selected_path, last, default_idx
        );

        self_.project_paths.replace(paths);
        self_.project_names.replace(names);
        self_.project_engines.replace(engines);
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self_
            .project_dropdown
            .set_model(Some(&StringList::new(&strs)));
        self_.project_dropdown.set_selected(default_idx as u32);
        self_.project_box.set_visible(show_projects);
        // The notify signal does not fire when the index stays 0.
        self.on_project_changed();
    }

    fn selected_project_path(&self) -> Option<String> {
        let self_ = self.imp();
        let idx = self_.project_dropdown.selected() as usize;
        self_.project_paths.borrow().get(idx).cloned().flatten()
    }

    fn selected_project_name(&self) -> String {
        let self_ = self.imp();
        let idx = self_.project_dropdown.selected() as usize;
        self_
            .project_names
            .borrow()
            .get(idx)
            .cloned()
            .unwrap_or_default()
    }

    fn on_project_changed(&self) {
        self.apply_project_choice();
        self.update_installed_label();
        self.update_action_state();
    }

    fn apply_project_choice(&self) {
        let self_ = self.imp();
        let idx = self_.project_dropdown.selected() as usize;
        let has_project = self.selected_project_path().is_some();
        self_.overwrite_box.set_visible(has_project);
        self_
            .vault_box
            .set_visible(!has_project && self_.vault_dirs.borrow().len() > 1);
        self_.download_button.set_label(if has_project {
            "Add to Project"
        } else {
            "Download"
        });

        let engine = self_.project_engines.borrow().get(idx).cloned().flatten();
        let project_version = engine
            .as_ref()
            .and_then(|(e, _)| registry::version_tuple(e));
        let (Some((engine, source)), Some(project_version)) = (engine, project_version) else {
            self.refresh_version_labels(None, None);
            self_.hint_label.set_label(if has_project {
                "The project's engine version is unknown; set an override in the project details."
            } else {
                "The asset is downloaded to the vault only."
            });
            return;
        };

        // Assets can go up in versions, never down: the highest release not above the project.
        // The list is sorted newest first with Fab before Local, so a tie goes to Fab.
        let releases: Vec<Vec<(u32, u32)>> = self_
            .versions
            .borrow()
            .iter()
            .map(VersionEntry::tuples)
            .collect();
        let choice = registry::choose_release(project_version, &releases);
        self.refresh_version_labels(Some(project_version), choice.map(|c| c.index()));
        let Some(choice) = choice else {
            self_.hint_label.set_label(&format!(
                "No version with a known engine is listed (project: UE {engine}, {}).",
                source.label()
            ));
            return;
        };
        let pos = choice.index();
        self_.version_dropdown.set_selected(pos as u32);
        let chosen = releases
            .get(pos)
            .and_then(|v| match choice {
                registry::ReleaseChoice::AllNewer(_) => v.iter().min(),
                _ => v.iter().filter(|x| **x <= project_version).max(),
            })
            .map(|(a, b)| format!("{a}.{b}"))
            .unwrap_or_default();
        debug!("Download dialog: {choice:?} for project engine UE {engine} -> UE {chosen}");
        self_.hint_label.set_label(&match choice {
            registry::ReleaseChoice::Exact(_) => format!(
                "Version matches the project's engine (UE {engine}, {}).",
                source.label()
            ),
            registry::ReleaseChoice::Older(_) => format!(
                "No release for UE {engine} ({}); using the newest older one (UE {chosen}).",
                source.label()
            ),
            registry::ReleaseChoice::AllNewer(_) => format!(
                "Warning: every release is newer than the project's engine (UE {engine}, {}). \
                 The lowest (UE {chosen}) is selected and may not work with this project.",
                source.label()
            ),
        });
    }

    /// Start the download (or the local import) with the current choices and close the dialog.
    pub fn confirm(&self) {
        let self_ = self.imp();
        let (Some(window), Some(dm)) = (self_.window.get(), self_.download_manager.get()) else {
            return;
        };
        let Some(version) = self.selected_version() else {
            warn!("Download dialog confirmed without a version");
            return;
        };
        let project_path = self.selected_project_path();
        let project_name = self.selected_project_name();
        if let Ok(mut last) = LAST_PROJECT.lock() {
            *last = Some(project_path.clone());
        }
        let engine_text = version
            .tuples()
            .into_iter()
            .max()
            .map(|(a, b)| format!("{a}.{b}"))
            .unwrap_or_default();

        if let Origin::Local(entry) = &version.origin {
            // A local zip goes straight into the chosen project
            let Some(uproject) = project_path else {
                return;
            };
            self.import_local(entry.clone(), uproject, project_name, engine_text);
            return;
        }

        let (target, actions) = match &project_path {
            Some(uproject) => {
                let dir = std::path::Path::new(uproject)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string());
                match dir {
                    Some(dir) => {
                        let mut actions = vec![PostDownloadAction::Copy(
                            dir,
                            self_.overwrite_switch.is_active(),
                        )];
                        // Plugins go to <project>/Plugins/<PluginName>/ only
                        let is_plugin = matches!(version.origin, Origin::Epic(_))
                            && self_.subject.borrow().as_ref().is_some_and(|s| s.is_plugin);
                        if is_plugin {
                            actions.push(PostDownloadAction::PluginLayout(Vec::new()));
                        }
                        (None, Some(actions))
                    }
                    None => (None, None),
                }
            }
            None => {
                let vault_idx = self_.vault_dropdown.selected();
                let vault = if vault_idx == gtk4::INVALID_LIST_POSITION {
                    None
                } else {
                    self_.vault_dirs.borrow().get(vault_idx as usize).cloned()
                };
                (vault, None)
            }
        };

        // (download manager key, asset id) of what is queued, for the install log
        let queued: Option<(String, String, String)> = match version.origin.clone() {
            Origin::Epic(asset) => {
                debug!(
                    "Download dialog: epic asset {:?} release {} project {:?}",
                    asset.title, version.id, project_path
                );
                let title = asset.title.clone().unwrap_or_default();
                let asset_id = asset.id.clone();
                dm.add_asset_download(version.id.clone(), asset, &target, actions);
                Some((version.id.clone(), asset_id, title))
            }
            Origin::Fab(asset) => {
                let platform_idx = self_.platform_dropdown.selected() as usize;
                let platform = version
                    .platforms
                    .get(platform_idx)
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_PLATFORM.to_string());
                debug!(
                    "Download dialog: fab asset {} artifact {} platform {} project {:?}",
                    asset.title, version.id, platform, project_path
                );
                let asset_id = asset.asset_id.clone();
                let title = asset.title.clone();
                dm.add_fab_asset_download(asset, version.id.clone(), platform, &target, actions);
                Some((asset_id.clone(), asset_id, title))
            }
            Origin::Local(_) => None,
        };
        if let (Some((key, asset_id, title)), Some(uproject)) = (queued, &project_path) {
            // Written to the install log once the files are in the project
            dm.set_install_info(
                &key,
                InstallRecord::new(
                    &asset_id,
                    &title,
                    Source::Fab,
                    &engine_text,
                    uproject,
                    &project_name,
                ),
            );
        }
        window.show_download_manager();
        self.force_close();
    }

    /// Import a local library zip into a project (off the main thread) and log the install.
    fn import_local(
        &self,
        entry: LibraryEntry,
        uproject: String,
        project_name: String,
        engine_text: String,
    ) {
        let self_ = self.imp();
        let Some(window) = self_.window.get().cloned() else {
            return;
        };
        let Some(root) = std::path::Path::new(&uproject)
            .parent()
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        let policy = if self_.overwrite_switch.is_active() {
            ImportPolicy::OverwriteWithBackup
        } else {
            ImportPolicy::SkipExisting
        };
        let record = InstallRecord::new(
            &entry.sidecar.sha256,
            &entry.sidecar.display_name,
            Source::Local,
            &engine_text,
            &uproject,
            &project_name,
        );
        debug!(
            "Download dialog: local zip {} into {}",
            entry.sidecar.display_name, uproject
        );
        window.add_notification(
            "local-import",
            &format!("Importing \"{}\"...", entry.sidecar.display_name),
            gtk4::MessageType::Info,
        );
        let handle = gio::spawn_blocking(move || {
            let result = ue_asset_library::safe_import(&entry, &root, policy)
                .map(|r| {
                    let mut text = format!(
                        "Imported \"{}\": {} files written, {} skipped, {} backed up.",
                        entry.sidecar.display_name,
                        r.written.len(),
                        r.skipped.len(),
                        r.backed_up.len()
                    );
                    for w in &r.warnings {
                        text.push_str(&format!(" Warning: {w}"));
                    }
                    text
                })
                .map_err(|e| e.to_string());
            if result.is_ok() {
                install_log::record_install(record);
            }
            result
        });
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(text)) => {
                    info!("{text}");
                    window.add_notification("local-import", &text, gtk4::MessageType::Info);
                }
                Ok(Err(e)) => {
                    error!("Local library import failed: {e}");
                    window.add_notification(
                        "local-import",
                        &format!("Import failed: {e}"),
                        gtk4::MessageType::Error,
                    );
                }
                Err(_) => warn!("The local library import panicked"),
            }
        });
        self.force_close();
    }
}
