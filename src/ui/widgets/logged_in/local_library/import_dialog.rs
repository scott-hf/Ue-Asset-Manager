//! Dialog used to import a local library entry into an existing project, or to
//! create a new project from a library entry of kind `Project`.
//!
//! Flow: pick project -> `preview_import` (worker thread) lists conflicting
//! files -> `safe_import` (worker thread) -> notification with the result.

use adw::subclass::prelude::AdwWindowImpl;
use gtk4::glib::clone;
use gtk4::subclass::prelude::*;
use gtk4::{self, gio, prelude::*};
use gtk4::{glib, CompositeTemplate};
use log::{error, info};
use std::path::PathBuf;
use ue_asset_library::{ImportPolicy, LibraryEntry};

const MAX_LISTED_FILES: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    ImportToProject,
    NewProject,
}

/// Messages from the worker threads back to the UI.
pub enum Msg {
    Preview(PathBuf, Result<Vec<PathBuf>, String>),
    Done(Result<String, String>),
}

pub mod imp {
    use super::*;
    use crate::window::EpicAssetManagerWindow;
    use once_cell::sync::OnceCell;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/local_import_dialog.ui")]
    pub struct EpicLocalImportDialog {
        pub window: OnceCell<EpicAssetManagerWindow>,
        pub entry: RefCell<Option<LibraryEntry>>,
        pub mode: Cell<Option<Mode>>,
        pub selected_project: RefCell<Option<PathBuf>>,
        /// The `.uproject` file and name of the selected project (for the install log).
        pub selected_uproject: RefCell<Option<(String, String)>>,
        pub parent_dir: RefCell<Option<PathBuf>>,
        pub preview_serial: Cell<u64>,
        pub busy: Cell<bool>,
        pub project_model: gio::ListStore,
        pub sender: async_channel::Sender<(u64, Msg)>,
        pub receiver: RefCell<Option<async_channel::Receiver<(u64, Msg)>>>,
        #[template_child]
        pub title_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub project_section: TemplateChild<gtk4::Box>,
        #[template_child]
        pub new_project_section: TemplateChild<gtk4::Box>,
        #[template_child]
        pub projects_list: TemplateChild<gtk4::ListView>,
        #[template_child]
        pub no_projects_bar: TemplateChild<adw::Banner>,
        #[template_child]
        pub project_name_entry: TemplateChild<gtk4::Entry>,
        #[template_child]
        pub parent_dir_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub choose_parent_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub overwrite_section: TemplateChild<gtk4::Box>,
        #[template_child]
        pub overwrite_switch: TemplateChild<gtk4::Switch>,
        #[template_child]
        pub status_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub spinner: TemplateChild<gtk4::Spinner>,
        #[template_child]
        pub import_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicLocalImportDialog {
        const NAME: &'static str = "EpicLocalImportDialog";
        type Type = super::EpicLocalImportDialog;
        type ParentType = adw::Window;

        fn new() -> Self {
            let (sender, receiver) = async_channel::unbounded();
            Self {
                window: OnceCell::new(),
                entry: RefCell::new(None),
                mode: Cell::new(None),
                selected_project: RefCell::new(None),
                selected_uproject: RefCell::new(None),
                parent_dir: RefCell::new(None),
                preview_serial: Cell::new(0),
                busy: Cell::new(false),
                project_model: gio::ListStore::new::<crate::models::project_data::ProjectData>(),
                sender,
                receiver: RefCell::new(Some(receiver)),
                title_label: TemplateChild::default(),
                project_section: TemplateChild::default(),
                new_project_section: TemplateChild::default(),
                projects_list: TemplateChild::default(),
                no_projects_bar: TemplateChild::default(),
                project_name_entry: TemplateChild::default(),
                parent_dir_label: TemplateChild::default(),
                choose_parent_button: TemplateChild::default(),
                overwrite_section: TemplateChild::default(),
                overwrite_switch: TemplateChild::default(),
                status_label: TemplateChild::default(),
                spinner: TemplateChild::default(),
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

    impl ObjectImpl for EpicLocalImportDialog {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_list();
            obj.setup_actions();
            obj.setup_receiver();
        }
    }

    impl WidgetImpl for EpicLocalImportDialog {}
    impl WindowImpl for EpicLocalImportDialog {}
    impl AdwWindowImpl for EpicLocalImportDialog {}
}

glib::wrapper! {
    pub struct EpicLocalImportDialog(ObjectSubclass<imp::EpicLocalImportDialog>)
        @extends gtk4::Widget, gtk4::Window, adw::Window,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl Default for EpicLocalImportDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl EpicLocalImportDialog {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_window(&self, window: &crate::window::EpicAssetManagerWindow) {
        let self_ = self.imp();
        if self_.window.get().is_none() {
            self_.window.set(window.clone()).unwrap();
        }
    }

    pub fn set_transient_for(&self, parent: Option<&crate::window::EpicAssetManagerWindow>) {
        gtk4::prelude::GtkWindowExt::set_transient_for(
            self,
            parent.map(|w| w.upcast_ref::<gtk4::Window>()),
        );
    }

    pub fn set_entry(&self, entry: LibraryEntry, mode: Mode) {
        let self_ = self.imp();
        let name = entry.sidecar.display_name.clone();
        self_.mode.set(Some(mode));
        match mode {
            Mode::ImportToProject => {
                self_
                    .title_label
                    .set_label(&format!("Import \"{name}\" into project:"));
                self_.project_section.set_visible(true);
                self_.new_project_section.set_visible(false);
                self_.overwrite_section.set_visible(true);
                self_.import_button.set_label("Import");
            }
            Mode::NewProject => {
                self_
                    .title_label
                    .set_label(&format!("Create new project from \"{name}\""));
                self_.project_section.set_visible(false);
                self_.new_project_section.set_visible(true);
                self_.overwrite_section.set_visible(false);
                self_.import_button.set_label("Create project");
                let suggested: String = name
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                self_.project_name_entry.set_text(&suggested);
                // Default to the first configured projects directory.
                if let Some(dir) = self_
                    .window
                    .get()
                    .map(|_| gio::Settings::new(crate::config::APP_ID))
                    .and_then(|s| s.strv("unreal-projects-directories").first().cloned())
                {
                    self.set_parent_dir(PathBuf::from(dir.as_str()));
                }
            }
        }
        self_.entry.replace(Some(entry));
        self.update_button();
    }

    fn setup_list(&self) {
        let self_ = self.imp();
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            texts.set_margin_top(6);
            texts.set_margin_bottom(6);
            texts.set_margin_start(8);
            texts.set_margin_end(8);
            let name = gtk4::Label::new(None);
            name.set_halign(gtk4::Align::Start);
            name.add_css_class("heading");
            let path = gtk4::Label::new(None);
            path.set_halign(gtk4::Align::Start);
            path.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
            path.add_css_class("caption");
            path.add_css_class("dim-label");
            texts.append(&name);
            texts.append(&path);
            item.set_child(Some(&texts));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let Some(project) = item
                .item()
                .and_downcast::<crate::models::project_data::ProjectData>()
            else {
                return;
            };
            let Some(texts) = item.child().and_downcast::<gtk4::Box>() else {
                return;
            };
            if let Some(name) = texts.first_child().and_downcast::<gtk4::Label>() {
                name.set_label(&project.name().unwrap_or_default());
            }
            if let Some(path) = texts.last_child().and_downcast::<gtk4::Label>() {
                path.set_label(&project.path().unwrap_or_default());
            }
        });

        let selection = gtk4::SingleSelection::new(Some(self_.project_model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        selection.connect_selection_changed(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |model, _, _| dialog.on_project_selected(model)
        ));
        self_.projects_list.set_model(Some(&selection));
        self_.projects_list.set_factory(Some(&factory));
    }

    fn setup_actions(&self) {
        let self_ = self.imp();

        self_.import_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.start_import()
        ));

        // Re-run the preview when the overwrite policy changes.
        self_.overwrite_switch.connect_active_notify(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.refresh_preview()
        ));

        self_.project_name_entry.connect_changed(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.update_button()
        ));

        self_.choose_parent_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| {
                let chooser = gtk4::FileDialog::builder()
                    .title("Create project in")
                    .modal(true)
                    .build();
                chooser.select_folder(
                    Some(&dialog),
                    None::<&gio::Cancellable>,
                    clone!(
                        #[weak]
                        dialog,
                        move |result| {
                            if let Some(path) = result.ok().and_then(|f| f.path()) {
                                dialog.set_parent_dir(path);
                            }
                        }
                    ),
                );
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
                while let Ok((serial, msg)) = receiver.recv().await {
                    dialog.handle_msg(serial, msg);
                }
            }
        ));
    }

    fn set_parent_dir(&self, dir: PathBuf) {
        let self_ = self.imp();
        self_.parent_dir_label.set_label(&dir.display().to_string());
        self_.parent_dir.replace(Some(dir));
        self.update_button();
    }

    fn update_button(&self) {
        let self_ = self.imp();
        let ready = !self_.busy.get()
            && match self_.mode.get() {
                Some(Mode::ImportToProject) => self_.selected_project.borrow().is_some(),
                Some(Mode::NewProject) => {
                    self_.parent_dir.borrow().is_some()
                        && !self_.project_name_entry.text().trim().is_empty()
                }
                None => false,
            };
        self_.import_button.set_sensitive(ready);
    }

    fn on_project_selected(&self, model: &gtk4::SingleSelection) {
        let self_ = self.imp();
        let project = model
            .selected_item()
            .and_downcast::<crate::models::project_data::ProjectData>();
        let uproject = project.as_ref().and_then(|p| p.path());
        let root = uproject
            .as_ref()
            .and_then(|p| PathBuf::from(p).parent().map(std::path::Path::to_path_buf));
        self_.selected_uproject.replace(uproject.map(|path| {
            let name = project
                .as_ref()
                .and_then(|p| p.name())
                .unwrap_or_else(|| path.clone());
            (path, name)
        }));
        self_.selected_project.replace(root);
        self.update_button();
        self.refresh_preview();
    }

    fn refresh_preview(&self) {
        let self_ = self.imp();
        if self_.mode.get() != Some(Mode::ImportToProject) {
            return;
        }
        let (Some(entry), Some(root)) = (
            self_.entry.borrow().clone(),
            self_.selected_project.borrow().clone(),
        ) else {
            self_.status_label.set_label("");
            return;
        };
        let serial = self_.preview_serial.get() + 1;
        self_.preview_serial.set(serial);
        self_
            .status_label
            .set_label("Checking for existing files...");
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let result = ue_asset_library::preview_import(&entry, &root).map_err(|e| e.to_string());
            let _ = sender.send_blocking((serial, Msg::Preview(root, result)));
        });
    }

    fn start_import(&self) {
        let self_ = self.imp();
        let Some(entry) = self_.entry.borrow().clone() else {
            return;
        };
        let mode = self_.mode.get();
        let sender = self_.sender.clone();
        let serial = self_.preview_serial.get();
        match mode {
            Some(Mode::ImportToProject) => {
                let Some(root) = self_.selected_project.borrow().clone() else {
                    return;
                };
                let policy = if self_.overwrite_switch.is_active() {
                    ImportPolicy::OverwriteWithBackup
                } else {
                    ImportPolicy::SkipExisting
                };
                let uproject = self_.selected_uproject.borrow().clone();
                self.set_busy("Importing...");
                std::thread::spawn(move || {
                    let result = ue_asset_library::safe_import(&entry, &root, policy)
                        .map(|r| {
                            // Remember the install (asset, source, engine version, project, time)
                            if let Some((uproject, project_name)) = &uproject {
                                crate::models::install_log::record_install(
                                    crate::models::install_log::InstallRecord::new(
                                        &entry.sidecar.sha256,
                                        &entry.sidecar.display_name,
                                        crate::models::library_merge::Source::Local,
                                        entry.sidecar.engine_version_guess.as_deref().unwrap_or(""),
                                        uproject,
                                        project_name,
                                    ),
                                );
                            }
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
                    let _ = sender.send_blocking((serial, Msg::Done(result)));
                });
            }
            Some(Mode::NewProject) => {
                let (Some(parent), name) = (
                    self_.parent_dir.borrow().clone(),
                    self_.project_name_entry.text().trim().to_string(),
                ) else {
                    return;
                };
                self.set_busy("Creating project...");
                std::thread::spawn(move || {
                    let result = ue_asset_library::safe_import_new_project(&entry, &parent, &name)
                        .map(|dir| format!("Created project in {}", dir.display()))
                        .map_err(|e| e.to_string());
                    let _ = sender.send_blocking((serial, Msg::Done(result)));
                });
            }
            None => {}
        }
    }

    fn set_busy(&self, text: &str) {
        let self_ = self.imp();
        self_.busy.set(true);
        self_.spinner.start();
        self_.status_label.set_label(text);
        self_.import_button.set_sensitive(false);
        self_.project_section.set_sensitive(false);
        self_.overwrite_section.set_sensitive(false);
        self_.new_project_section.set_sensitive(false);
    }

    fn handle_msg(&self, serial: u64, msg: Msg) {
        let self_ = self.imp();
        match msg {
            Msg::Preview(root, result) => {
                // Ignore stale previews (project or policy changed since).
                if serial != self_.preview_serial.get()
                    || self_.selected_project.borrow().as_ref() != Some(&root)
                {
                    return;
                }
                let overwrite = self_.overwrite_switch.is_active();
                let text = match result {
                    Err(e) => format!("Unable to check the project: {e}"),
                    Ok(conflicts) if conflicts.is_empty() => {
                        "No existing files will be affected.".to_string()
                    }
                    Ok(conflicts) => {
                        let action = if overwrite {
                            "will be backed up to Saved/EAM_Backups and replaced"
                        } else {
                            "already exist and will be skipped"
                        };
                        let mut text = format!("{} files {action}:\n", conflicts.len());
                        for c in conflicts.iter().take(MAX_LISTED_FILES) {
                            text.push_str(&format!("  {}\n", c.display()));
                        }
                        if conflicts.len() > MAX_LISTED_FILES {
                            text.push_str(&format!(
                                "  ... and {} more\n",
                                conflicts.len() - MAX_LISTED_FILES
                            ));
                        }
                        text
                    }
                };
                self_.status_label.set_label(&text);
            }
            Msg::Done(result) => {
                self_.spinner.stop();
                self_.busy.set(false);
                let Some(window) = self_.window.get() else {
                    return;
                };
                match result {
                    Ok(text) => {
                        info!("{text}");
                        window.add_notification("local-import", &text, gtk4::MessageType::Info);
                        if self_.mode.get() == Some(Mode::NewProject) {
                            window.imp().logged_in_stack.refresh_projects();
                        }
                        self.close();
                    }
                    Err(e) => {
                        error!("Local library import failed: {e}");
                        window.add_notification(
                            "local-import",
                            &format!("Import failed: {e}"),
                            gtk4::MessageType::Error,
                        );
                        self_.project_section.set_sensitive(true);
                        self_.overwrite_section.set_sensitive(true);
                        self_.new_project_section.set_sensitive(true);
                        self_.status_label.set_label(&format!("Import failed: {e}"));
                        self.update_button();
                    }
                }
            }
        }
    }

    fn load_projects(&self) {
        let self_ = self.imp();
        self_.project_model.remove_all();
        let Some(window) = self_.window.get() else {
            return;
        };
        let logged_in = window.imp().logged_in_stack.clone();
        let projects = &logged_in.imp().projects.imp().grid_model;
        let n_items = projects.n_items();
        self_.no_projects_bar.set_revealed(n_items == 0);
        for i in 0..n_items {
            if let Some(project) = projects
                .item(i)
                .and_downcast::<crate::models::project_data::ProjectData>()
            {
                self_.project_model.append(&project);
            }
        }
    }

    pub fn present(&self) {
        if self.imp().mode.get() == Some(Mode::ImportToProject) {
            self.load_projects();
        }
        gtk4::prelude::GtkWindowExt::present(self);
    }
}
