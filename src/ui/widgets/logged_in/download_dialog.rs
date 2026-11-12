//! One dialog shared by the Epic library and Fab downloads.
//!
//! It asks which project the asset should be added to (defaulting to the project
//! selected on the Projects page, or "vault only") and which version to download,
//! then hands the request to the download manager.

use crate::models::asset_data::{AssetData, AssetType};
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
use log::{debug, warn};
use std::cell::RefCell;
use std::sync::Mutex;

/// Last project choice made in this session (`Some(None)` = vault only).
static LAST_PROJECT: Mutex<Option<Option<String>>> = Mutex::new(None);

/// (display name, .uproject path, resolved engine version and where it came from)
type ProjectEntry = (String, String, Option<(String, VersionSource)>);

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Subject {
    Epic(AssetInfo),
    Fab(FabAsset),
}

/// One selectable version: release id for Epic assets, artifact id for Fab assets.
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Dropdown text without the project-dependent suffix
    pub label: String,
    pub id: String,
    /// Normalized engine versions ("5.3")
    pub engines: Vec<String>,
    platforms: Vec<String>,
}

impl VersionEntry {
    /// (major, minor) of every engine version this release supports.
    pub fn tuples(&self) -> Vec<(u32, u32)> {
        self.engines
            .iter()
            .filter_map(|e| registry::version_tuple(e))
            .collect()
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
        /// Normalized engine association per project entry.
        pub project_engines: RefCell<Vec<Option<(String, VersionSource)>>>,
        pub versions: RefCell<Vec<VersionEntry>>,
        pub vault_dirs: RefCell<Vec<String>>,
        #[template_child]
        pub asset_label: TemplateChild<gtk4::Label>,
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
                project_engines: RefCell::new(Vec::new()),
                versions: RefCell::new(Vec::new()),
                vault_dirs: RefCell::new(Vec::new()),
                asset_label: TemplateChild::default(),
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
    /// Ask where to put an Epic library asset and which release to download.
    pub fn present_epic(
        window: &EpicAssetManagerWindow,
        dm: &EpicDownloadManager,
        asset: &AssetInfo,
    ) -> Self {
        let dialog = Self::build(window, dm, Subject::Epic(asset.clone()));
        dialog.present_on(window);
        dialog
    }

    /// Ask where to put a Fab asset and which version / platform to download.
    pub fn present_fab(
        window: &EpicAssetManagerWindow,
        dm: &EpicDownloadManager,
        asset: &FabAsset,
    ) -> Self {
        let dialog = Self::build(window, dm, Subject::Fab(asset.clone()));
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

        let show_projects = match &subject {
            Subject::Epic(asset) => matches!(
                AssetData::decide_kind(asset),
                Some(AssetType::Asset | AssetType::Project | AssetType::Plugin)
            ),
            Subject::Fab(_) => true,
        };
        let title = match &subject {
            Subject::Epic(asset) => asset.title.clone().unwrap_or_default(),
            Subject::Fab(asset) => asset.title.clone(),
        };
        self_.asset_label.set_label(&title);
        self_.subject.replace(Some(subject));

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
            }
        ));
    }

    fn setup_versions(&self) {
        let self_ = self.imp();
        let mut entries: Vec<VersionEntry> = Vec::new();
        match &*self_.subject.borrow() {
            Some(Subject::Epic(asset)) => {
                if let Some(releases) = asset.sorted_releases() {
                    for release in releases {
                        let Some(id) = release.app_id.clone() else {
                            continue;
                        };
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
                            label,
                            id,
                            engines: raw_engines
                                .iter()
                                .filter_map(|e| normalize_engine(e))
                                .collect(),
                            platforms: Vec::new(),
                        });
                    }
                }
            }
            Some(Subject::Fab(asset)) => {
                // Fab lists versions oldest first; show the newest engine first.
                let engine_key = |pv: &egs_api::api::types::fab_library::ProjectVersion| {
                    pv.engine_versions
                        .iter()
                        .filter_map(|e| normalize_engine(e))
                        .filter_map(|e| {
                            let mut it = e.split('.');
                            Some((
                                it.next()?.parse::<u32>().ok()?,
                                it.next()?.parse::<u32>().ok()?,
                            ))
                        })
                        .max()
                };
                let mut sorted: Vec<_> = asset.project_versions.iter().collect();
                sorted.sort_by_key(|pv| std::cmp::Reverse(engine_key(pv)));
                for pv in sorted {
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
                        label,
                        id: pv.artifact_id.clone(),
                        engines: raw_engines
                            .iter()
                            .filter_map(|e| normalize_engine(e))
                            .collect(),
                        platforms: pv.target_platforms.clone(),
                    });
                }
            }
            None => {}
        }

        let is_fab = matches!(&*self_.subject.borrow(), Some(Subject::Fab(_)));
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
        self_.platform_box.set_visible(is_fab);
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

    fn update_platforms(&self) {
        let self_ = self.imp();
        let idx = self_.version_dropdown.selected() as usize;
        let platforms = self_
            .versions
            .borrow()
            .get(idx)
            .map(|v| v.platforms.clone())
            .unwrap_or_default();
        let strs: Vec<&str> = platforms.iter().map(String::as_str).collect();
        self_
            .platform_dropdown
            .set_model(Some(&StringList::new(&strs)));
        if let Some(pos) = platforms.iter().position(|p| p == DEFAULT_PLATFORM) {
            self_.platform_dropdown.set_selected(pos as u32);
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
        let mut paths: Vec<Option<String>> = vec![None];
        let mut engines: Vec<Option<(String, VersionSource)>> = vec![None];
        for (name, path, engine) in projects {
            labels.push(name);
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

    fn on_project_changed(&self) {
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

        // Assets can go up in versions, never down: the highest release not above the project
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

    /// Start the download with the current choices and close the dialog.
    pub fn confirm(&self) {
        let self_ = self.imp();
        let (Some(window), Some(dm)) = (self_.window.get(), self_.download_manager.get()) else {
            return;
        };
        let version_idx = self_.version_dropdown.selected() as usize;
        let Some(version) = self_.versions.borrow().get(version_idx).cloned() else {
            warn!("Download dialog confirmed without a version");
            return;
        };
        let project_path = self.selected_project_path();
        if let Ok(mut last) = LAST_PROJECT.lock() {
            *last = Some(project_path.clone());
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
                        let is_plugin = matches!(
                            &*self_.subject.borrow(),
                            Some(Subject::Epic(asset))
                                if matches!(AssetData::decide_kind(asset), Some(AssetType::Plugin))
                        );
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

        let subject = self_.subject.borrow().clone();
        match subject {
            Some(Subject::Epic(asset)) => {
                debug!(
                    "Download dialog: epic asset {:?} release {} project {:?}",
                    asset.title, version.id, project_path
                );
                dm.add_asset_download(version.id, asset, &target, actions);
            }
            Some(Subject::Fab(asset)) => {
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
                dm.add_fab_asset_download(asset, version.id, platform, &target, actions);
            }
            None => return,
        }
        window.show_download_manager();
        self.force_close();
    }
}
