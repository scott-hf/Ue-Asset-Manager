//! "Add to project" for the assets picked on the Assets page.
//!
//! Choose a project, see the dry-run plan (releases chosen by the no-downgrade rule, files,
//! conflicts, references outside the packs), then apply it on a worker thread. The app only
//! reports the files it wrote; staging them anywhere is a separate step.

use super::ProjectChoice;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib::clone;
use gtk4::{self, glib, CompositeTemplate};
use log::{error, info};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::Arc;
use ue_asset_catalog::import::{self, ImportPlan, ProjectTarget};
use ue_asset_catalog::release::format_version;
use ue_asset_catalog::Catalog;

const MAX_LISTED: usize = 40;
const MAX_LISTED_WRITTEN: usize = 200;

pub enum Msg {
    Plan(u64, Box<Result<ImportPlan, String>>),
    Done(Result<Applied, String>),
}

/// Outcome of applying a plan.
pub struct Applied {
    pub text: String,
    pub notification: String,
}

pub mod imp {
    use super::*;
    use adw::subclass::dialog::AdwDialogImpl;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/assets_add_dialog.ui")]
    pub struct EpicAssetsAddDialog {
        pub window: RefCell<Option<crate::window::EpicAssetManagerWindow>>,
        pub catalog: RefCell<Option<Arc<Catalog>>>,
        pub assets: RefCell<Vec<usize>>,
        pub projects: RefCell<Vec<ProjectChoice>>,
        pub plan: RefCell<Option<Arc<ImportPlan>>>,
        pub serial: Cell<u64>,
        pub busy: Cell<bool>,
        pub sender: async_channel::Sender<Msg>,
        pub receiver: RefCell<Option<async_channel::Receiver<Msg>>>,
        #[template_child]
        pub title_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub project_section: TemplateChild<gtk4::Box>,
        #[template_child]
        pub project_dropdown: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub overwrite_switch: TemplateChild<gtk4::Switch>,
        #[template_child]
        pub preview_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub verdict_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub spinner: TemplateChild<gtk4::Spinner>,
        #[template_child]
        pub cancel_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub add_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicAssetsAddDialog {
        const NAME: &'static str = "EpicAssetsAddDialog";
        type Type = super::EpicAssetsAddDialog;
        type ParentType = adw::Dialog;

        fn new() -> Self {
            let (sender, receiver) = async_channel::unbounded();
            Self {
                window: RefCell::new(None),
                catalog: RefCell::new(None),
                assets: RefCell::new(Vec::new()),
                projects: RefCell::new(Vec::new()),
                plan: RefCell::new(None),
                serial: Cell::new(0),
                busy: Cell::new(false),
                sender,
                receiver: RefCell::new(Some(receiver)),
                title_label: TemplateChild::default(),
                project_section: TemplateChild::default(),
                project_dropdown: TemplateChild::default(),
                overwrite_switch: TemplateChild::default(),
                preview_label: TemplateChild::default(),
                verdict_banner: TemplateChild::default(),
                spinner: TemplateChild::default(),
                cancel_button: TemplateChild::default(),
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

    impl ObjectImpl for EpicAssetsAddDialog {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_events();
            obj.setup_receiver();
        }
    }

    impl WidgetImpl for EpicAssetsAddDialog {}
    impl AdwDialogImpl for EpicAssetsAddDialog {}
}

glib::wrapper! {
    pub struct EpicAssetsAddDialog(ObjectSubclass<imp::EpicAssetsAddDialog>)
        @extends gtk4::Widget, adw::Dialog,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::ShortcutManager;
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

fn verdict_text(verdict: &str, pack_engine: Option<&str>, project: Option<(u32, u32)>) -> String {
    let pack = pack_engine.unwrap_or("?");
    let project = project.map(format_version).unwrap_or_else(|| "?".into());
    match verdict {
        "exact" => format!("UE {pack}: matches the project's engine (UE {project})"),
        "older" => format!(
            "UE {pack}: older than the project (UE {project}); Unreal upgrades it when it is resaved"
        ),
        "newer" => format!(
            "WARNING UE {pack}: newer than the project (UE {project}); no older release exists, it may not open"
        ),
        _ => format!("UE {pack}: the project's engine version is unknown, the newest release is used"),
    }
}

impl EpicAssetsAddDialog {
    /// `default_project` indexes `projects`.
    pub fn present_for(
        window: &crate::window::EpicAssetManagerWindow,
        catalog: Arc<Catalog>,
        assets: Vec<usize>,
        projects: Vec<ProjectChoice>,
        default_project: Option<usize>,
    ) -> Self {
        let dialog: Self = glib::Object::new();
        let self_ = dialog.imp();
        self_.title_label.set_label(&format!(
            "Add {} selected assets to a project",
            assets.len()
        ));
        let labels: Vec<String> = projects.iter().map(ProjectChoice::label).collect();
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self_
            .project_dropdown
            .set_model(Some(&gtk4::StringList::new(&strs)));
        self_.window.replace(Some(window.clone()));
        self_.catalog.replace(Some(catalog));
        self_.assets.replace(assets);
        let has_projects = !projects.is_empty();
        self_.projects.replace(projects);
        if has_projects {
            self_
                .project_dropdown
                .set_selected(default_project.unwrap_or(0) as u32);
            dialog.refresh_preview();
        } else {
            self_
                .preview_label
                .set_label("No projects found. Add a projects directory in the preferences.");
        }
        adw::prelude::AdwDialogExt::present(&dialog, Some(window));
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
        self_.add_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.apply()
        ));
        self_.project_dropdown.connect_selected_notify(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.refresh_preview()
        ));
        self_.overwrite_switch.connect_active_notify(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.refresh_preview()
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

    fn selected_project(&self) -> Option<ProjectChoice> {
        let self_ = self.imp();
        let idx = self_.project_dropdown.selected();
        if idx == gtk4::INVALID_LIST_POSITION {
            return None;
        }
        self_.projects.borrow().get(idx as usize).cloned()
    }

    /// Dry run on a worker thread (reads the project's Content folder for clashes).
    fn refresh_preview(&self) {
        let self_ = self.imp();
        if self_.busy.get() {
            return;
        }
        let (Some(project), Some(catalog)) =
            (self.selected_project(), self_.catalog.borrow().clone())
        else {
            return;
        };
        let serial = self_.serial.get() + 1;
        self_.serial.set(serial);
        self_.plan.replace(None);
        self_.add_button.set_sensitive(false);
        self_.verdict_banner.set_revealed(false);
        self_.preview_label.set_label("Checking the project...");
        self_.spinner.start();
        let assets: Vec<usize> = self_.assets.borrow().clone();
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<ImportPlan, String> {
                let target = ProjectTarget::from_path(&PathBuf::from(&project.uproject))?;
                let ids: Vec<String> = assets
                    .iter()
                    .map(|&a| catalog.assets[a].id.clone())
                    .collect();
                let resolved = import::resolve_assets(&catalog, &ids, 5000)?;
                import::plan_import(&catalog, &target, &resolved, true)
            })();
            let _ = sender.send_blocking(Msg::Plan(serial, Box::new(result)));
        });
    }

    fn apply(&self) {
        let self_ = self.imp();
        let (Some(plan), Some(catalog)) =
            (self_.plan.borrow().clone(), self_.catalog.borrow().clone())
        else {
            return;
        };
        let overwrite = self_.overwrite_switch.is_active();
        self_.busy.set(true);
        self_.add_button.set_sensitive(false);
        self_.project_section.set_sensitive(false);
        self_.spinner.start();
        self_.preview_label.set_label("Adding the assets...");
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let outcomes = import::apply_plan(&catalog, &plan, overwrite);
            let log_path = ue_asset_catalog::install_log::default_path();
            let mut text = String::new();
            let mut written_total = 0usize;
            let mut failed = 0usize;
            let mut written_paths: Vec<String> = Vec::new();
            for outcome in &outcomes {
                match &outcome.result {
                    Ok(report) => {
                        written_total += report.written.len();
                        text.push_str(&format!(
                            "{}: {} files written, {} skipped, {} backed up.\n",
                            outcome.pack_name,
                            report.written.len(),
                            report.skipped.len(),
                            report.backed_up.len()
                        ));
                        for w in &report.warnings {
                            text.push_str(&format!("  Warning: {w}\n"));
                        }
                        if !report.external_missing.is_empty() {
                            let listed: Vec<&str> = report
                                .external_missing
                                .iter()
                                .take(MAX_LISTED)
                                .map(String::as_str)
                                .collect();
                            text.push_str(&format!(
                                "  Warning: {} referenced packages are not part of this pack: {}{}\n",
                                report.external_missing.len(),
                                listed.join(", "),
                                if report.external_missing.len() > MAX_LISTED {
                                    ", ..."
                                } else {
                                    ""
                                }
                            ));
                        }
                        written_paths.extend(
                            report
                                .written
                                .iter()
                                .map(|p| import::relative_to(&plan.project, p)),
                        );
                        if let Some(path) = &log_path {
                            if let Err(e) =
                                import::log_install(path, &catalog, &outcome.pack_id, &plan.project)
                            {
                                text.push_str(&format!("  Install log not updated: {e}\n"));
                            }
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        text.push_str(&format!("{}: failed: {e}\n", outcome.pack_name));
                    }
                }
            }
            if !written_paths.is_empty() {
                text.push_str("\nFiles written (relative to the project):\n");
                for p in written_paths.iter().take(MAX_LISTED_WRITTEN) {
                    text.push_str(&format!("  {p}\n"));
                }
                if written_paths.len() > MAX_LISTED_WRITTEN {
                    text.push_str(&format!(
                        "  ... and {} more\n",
                        written_paths.len() - MAX_LISTED_WRITTEN
                    ));
                }
            }
            let notification = if failed == 0 {
                format!(
                    "Added assets to \"{}\": {written_total} files written.",
                    plan.project.name
                )
            } else {
                format!(
                    "Added assets to \"{}\": {written_total} files written, {failed} packs failed.",
                    plan.project.name
                )
            };
            let _ = sender.send_blocking(Msg::Done(Ok(Applied { text, notification })));
        });
    }

    fn handle_msg(&self, msg: Msg) {
        let self_ = self.imp();
        match msg {
            Msg::Plan(serial, result) => {
                if serial != self_.serial.get() {
                    return;
                }
                self_.spinner.stop();
                match *result {
                    Ok(plan) => {
                        let (text, newer) = self.describe(&plan);
                        self_.preview_label.set_label(&text);
                        self_.verdict_banner.set_title(
                            "Some packs are newer than the project's engine and may not open.",
                        );
                        self_.verdict_banner.set_revealed(newer);
                        self_.add_button.set_sensitive(!plan.packs.is_empty());
                        self_.plan.replace(Some(Arc::new(plan)));
                    }
                    Err(e) => {
                        self_
                            .preview_label
                            .set_label(&format!("Unable to plan the import: {e}"));
                    }
                }
            }
            Msg::Done(result) => {
                self_.spinner.stop();
                self_.busy.set(false);
                let window = self_.window.borrow().clone();
                match result {
                    Ok(applied) => {
                        info!("{}", applied.notification);
                        self_.preview_label.set_label(&applied.text);
                        self_.cancel_button.set_label("Close");
                        self_.add_button.set_visible(false);
                        if let Some(window) = window {
                            window.add_notification(
                                "assets-add",
                                &applied.notification,
                                gtk4::MessageType::Info,
                            );
                        }
                    }
                    Err(e) => {
                        error!("Adding assets failed: {e}");
                        self_.project_section.set_sensitive(true);
                        self_
                            .preview_label
                            .set_label(&format!("Adding the assets failed: {e}"));
                        if let Some(window) = window {
                            window.add_notification(
                                "assets-add",
                                &format!("Adding assets failed: {e}"),
                                gtk4::MessageType::Error,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Text of a dry-run plan, and whether any pack is newer than the project.
    fn describe(&self, plan: &ImportPlan) -> (String, bool) {
        let overwrite = self.imp().overwrite_switch.is_active();
        let totals = plan.totals();
        let mut text = format!(
            "{} assets ({} files, {}) from {} packs into {}.\n",
            totals.assets,
            totals.files,
            human_size(totals.size_bytes),
            plan.packs.len(),
            plan.project.name
        );
        let mut newer = false;
        for p in &plan.packs {
            newer |= p.verdict == "newer";
            text.push_str(&format!(
                "\n{}\n  {}\n  {} selected + {} required, {}\n",
                p.pack_name,
                verdict_text(p.verdict, p.pack_engine.as_deref(), plan.project.engine),
                p.selected.len(),
                p.dependencies.len(),
                human_size(p.size_bytes)
            ));
            if p.unreadable > 0 {
                text.push_str(&format!(
                    "  {} packages could not be analysed; what they need may be missing.\n",
                    p.unreadable
                ));
            }
        }
        if totals.existing_files > 0 {
            let action = if overwrite {
                "will be backed up to Saved/EAM_Backups and replaced"
            } else {
                "already exist and will be skipped"
            };
            text.push_str(&format!("\n{} files {action}:\n", totals.existing_files));
            let mut listed = 0usize;
            'outer: for p in &plan.packs {
                for f in &p.existing_files {
                    if listed >= MAX_LISTED {
                        text.push_str("  ...\n");
                        break 'outer;
                    }
                    text.push_str(&format!("  {f}\n"));
                    listed += 1;
                }
            }
        } else {
            text.push_str("\nNo existing files will be affected.\n");
        }
        if totals.external > 0 {
            text.push_str(&format!(
                "\n{} referenced packages are not part of these packs and will not be added.\n",
                totals.external
            ));
        }
        for w in &plan.warnings {
            text.push_str(&format!("\nWarning: {w}\n"));
        }
        (text, newer)
    }
}
