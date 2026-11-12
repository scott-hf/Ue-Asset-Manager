//! "Local Library" page: lists sanitized asset zips stored in user configured
//! directories (see the `ue_asset_library` crate). Works fully offline, no Epic
//! login data is used.

pub mod fab_match_dialog;
pub mod import_dialog;

use crate::models::local_library::{kind_label, scan_directory, LocalAssetData, ScanResult};
use crate::ui::widgets::logged_in::refresh::Refresh;
use gtk4::glib::clone;
use gtk4::subclass::prelude::*;
use gtk4::{self, gio, prelude::*};
use gtk4::{glib, CompositeTemplate};
use log::{info, warn};
use std::path::PathBuf;
use ue_asset_library::Kind;

pub mod imp {
    use super::*;
    use crate::window::EpicAssetManagerWindow;
    use once_cell::sync::OnceCell;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/local_library.ui")]
    pub struct EpicLocalLibraryBox {
        pub window: OnceCell<EpicAssetManagerWindow>,
        pub settings: gio::Settings,
        pub store: gio::ListStore,
        pub filter_model: gtk4::FilterListModel,
        pub selection: gtk4::SingleSelection,
        pub search: RefCell<Option<String>>,
        pub pool: threadpool::ThreadPool,
        pub generation: Cell<u64>,
        pub pending: Cell<usize>,
        pub unreachable: RefCell<Vec<String>>,
        pub sender: async_channel::Sender<ScanResult>,
        pub receiver: RefCell<Option<async_channel::Receiver<ScanResult>>>,
        #[template_child]
        pub content_stack: TemplateChild<gtk4::Stack>,
        #[template_child]
        pub empty_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub list_view: TemplateChild<gtk4::ListView>,
        #[template_child]
        pub search_entry: TemplateChild<gtk4::SearchEntry>,
        #[template_child]
        pub refresh_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub count_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub detail_scroll: TemplateChild<gtk4::ScrolledWindow>,
        #[template_child]
        pub detail_separator: TemplateChild<gtk4::Separator>,
        #[template_child]
        pub detail_name: TemplateChild<gtk4::Label>,
        #[template_child]
        pub detail_kind: TemplateChild<gtk4::Label>,
        #[template_child]
        pub detail_info: TemplateChild<gtk4::Label>,
        #[template_child]
        pub detail_warning: TemplateChild<gtk4::Label>,
        #[template_child]
        pub fab_box: TemplateChild<gtk4::Box>,
        #[template_child]
        pub fab_picture: TemplateChild<gtk4::Picture>,
        #[template_child]
        pub fab_title: TemplateChild<gtk4::Label>,
        #[template_child]
        pub fab_link: TemplateChild<gtk4::LinkButton>,
        #[template_child]
        pub match_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub clear_match_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub import_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub new_project_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicLocalLibraryBox {
        const NAME: &'static str = "EpicLocalLibraryBox";
        type Type = super::EpicLocalLibraryBox;
        type ParentType = gtk4::Box;

        fn new() -> Self {
            let (sender, receiver) = async_channel::unbounded();
            let store = gio::ListStore::new::<LocalAssetData>();
            let filter_model =
                gtk4::FilterListModel::new(Some(store.clone()), None::<gtk4::CustomFilter>);
            Self {
                window: OnceCell::new(),
                settings: gio::Settings::new(crate::config::APP_ID),
                store,
                filter_model,
                selection: gtk4::SingleSelection::builder()
                    .autoselect(false)
                    .can_unselect(true)
                    .build(),
                search: RefCell::new(None),
                pool: threadpool::ThreadPool::new(2),
                generation: Cell::new(0),
                pending: Cell::new(0),
                unreachable: RefCell::new(Vec::new()),
                sender,
                receiver: RefCell::new(Some(receiver)),
                content_stack: TemplateChild::default(),
                empty_page: TemplateChild::default(),
                list_view: TemplateChild::default(),
                search_entry: TemplateChild::default(),
                refresh_button: TemplateChild::default(),
                count_label: TemplateChild::default(),
                detail_scroll: TemplateChild::default(),
                detail_separator: TemplateChild::default(),
                detail_name: TemplateChild::default(),
                detail_kind: TemplateChild::default(),
                detail_info: TemplateChild::default(),
                detail_warning: TemplateChild::default(),
                fab_box: TemplateChild::default(),
                fab_picture: TemplateChild::default(),
                fab_title: TemplateChild::default(),
                fab_link: TemplateChild::default(),
                match_button: TemplateChild::default(),
                clear_match_button: TemplateChild::default(),
                import_button: TemplateChild::default(),
                new_project_button: TemplateChild::default(),
            }
        }

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for EpicLocalLibraryBox {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_widgets();
            obj.setup_list();
            obj.setup_receiver();
        }
    }

    impl WidgetImpl for EpicLocalLibraryBox {}
    impl BoxImpl for EpicLocalLibraryBox {}
}

glib::wrapper! {
    pub struct EpicLocalLibraryBox(ObjectSubclass<imp::EpicLocalLibraryBox>)
        @extends gtk4::Widget, gtk4::Box,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

impl Default for EpicLocalLibraryBox {
    fn default() -> Self {
        Self::new()
    }
}

impl EpicLocalLibraryBox {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_window(&self, window: &crate::window::EpicAssetManagerWindow) {
        let self_ = self.imp();
        if self_.window.get().is_some() {
            return;
        }
        self_.window.set(window.clone()).unwrap();
        self.rescan();
    }

    fn setup_widgets(&self) {
        let self_ = self.imp();

        self_.search_entry.connect_search_changed(clone!(
            #[weak(rename_to=page)]
            self,
            move |entry| {
                let text = entry.text();
                page.imp().search.replace(if text.is_empty() {
                    None
                } else {
                    Some(text.to_lowercase())
                });
                page.update_filter();
            }
        ));

        self_.refresh_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.rescan()
        ));

        self_.match_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.open_match_dialog()
        ));

        self_.clear_match_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.clear_match()
        ));

        // The login state can change while the page is hidden: re-evaluate when shown.
        self.connect_map(|page| page.update_detail());

        self_.import_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.open_import_dialog(import_dialog::Mode::ImportToProject)
        ));

        self_.new_project_button.connect_clicked(clone!(
            #[weak(rename_to=page)]
            self,
            move |_| page.open_import_dialog(import_dialog::Mode::NewProject)
        ));

        // Rescan whenever the configured directories change in the preferences.
        self_.settings.connect_changed(
            Some("local-library-directories"),
            clone!(
                #[weak(rename_to=page)]
                self,
                move |_, _| {
                    if page.imp().window.get().is_some() {
                        page.rescan();
                    }
                }
            ),
        );
    }

    fn setup_list(&self) {
        let self_ = self.imp();

        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(8);
            row.set_margin_end(8);

            let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            texts.set_hexpand(true);
            let name = gtk4::Label::new(None);
            name.set_halign(gtk4::Align::Start);
            name.set_xalign(0.0);
            name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            name.add_css_class("heading");
            let sub = gtk4::Label::new(None);
            sub.set_halign(gtk4::Align::Start);
            sub.set_xalign(0.0);
            sub.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            sub.add_css_class("caption");
            sub.add_css_class("dim-label");
            texts.append(&name);
            texts.append(&sub);

            let badge = gtk4::Label::new(None);
            badge.add_css_class("fab-badge");
            badge.set_valign(gtk4::Align::Center);

            row.append(&texts);
            row.append(&badge);
            item.set_child(Some(&row));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let Some(data) = item.item().and_downcast::<LocalAssetData>() else {
                return;
            };
            let Some(row) = item.child().and_downcast::<gtk4::Box>() else {
                return;
            };
            let Some(texts) = row.first_child().and_downcast::<gtk4::Box>() else {
                return;
            };
            if let Some(name) = texts.first_child().and_downcast::<gtk4::Label>() {
                name.set_label(&data.name());
            }
            if let Some(sub) = texts.last_child().and_downcast::<gtk4::Label>() {
                let engine = data.engine_version();
                if engine.is_empty() {
                    sub.set_label(&data.kind());
                } else {
                    sub.set_label(&format!("{} - UE {}", data.kind(), engine));
                }
            }
            if let Some(badge) = row.last_child().and_downcast::<gtk4::Label>() {
                badge.set_label(&data.kind().to_uppercase());
            }
        });

        let sorter = gtk4::StringSorter::new(Some(gtk4::PropertyExpression::new(
            LocalAssetData::static_type(),
            None::<gtk4::Expression>,
            "name",
        )));
        sorter.set_ignore_case(true);
        let sorted = gtk4::SortListModel::new(Some(self_.filter_model.clone()), Some(sorter));
        self_.selection.set_model(Some(&sorted));

        self_.selection.connect_selection_changed(clone!(
            #[weak(rename_to=page)]
            self,
            move |_, _, _| page.update_detail()
        ));
        self_.selection.connect_items_changed(clone!(
            #[weak(rename_to=page)]
            self,
            move |_, _, _, _| page.update_count()
        ));

        self_.list_view.set_model(Some(&self_.selection));
        self_.list_view.set_factory(Some(&factory));
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
                while let Ok(result) = receiver.recv().await {
                    page.handle_scan_result(result);
                }
            }
        ));
    }

    fn configured_dirs(&self) -> Vec<PathBuf> {
        self.imp()
            .settings
            .strv("local-library-directories")
            .iter()
            .map(|d| PathBuf::from(d.as_str()))
            .collect()
    }

    /// Rescan every configured directory on the thread pool; the UI is never blocked.
    pub fn rescan(&self) {
        let self_ = self.imp();
        let generation = self_.generation.get() + 1;
        self_.generation.set(generation);
        self_.store.remove_all();
        self_.unreachable.borrow_mut().clear();

        let dirs = self.configured_dirs();
        self_.pending.set(dirs.len());
        self.update_detail();
        if dirs.is_empty() {
            self.show_empty(
                "No local library configured",
                "Add a Local Library directory in the preferences to see sanitized asset zips here.",
            );
            return;
        }
        self_.content_stack.set_visible_child_name("list");
        self_.refresh_button.set_sensitive(false);
        self_.count_label.set_label("Scanning...");

        for dir in dirs {
            let sender = self_.sender.clone();
            self_.pool.execute(move || {
                let result = scan_directory(generation, dir);
                if let Err(e) = sender.send_blocking(result) {
                    warn!("Unable to send local library scan result: {e}");
                }
            });
        }
    }

    fn handle_scan_result(&self, result: ScanResult) {
        let self_ = self.imp();
        if result.generation != self_.generation.get() {
            return; // stale result of a previous scan
        }
        match result.entries {
            Some(entries) => {
                info!(
                    "Local library {}: {} entries",
                    result.dir.display(),
                    entries.len()
                );
                for (entry, overlay) in &entries {
                    let data = LocalAssetData::new(entry, &result.dir);
                    data.set_overlay(overlay.clone());
                    self_.store.append(&data);
                }
            }
            None => self_
                .unreachable
                .borrow_mut()
                .push(result.dir.display().to_string()),
        }
        self_.pending.set(self_.pending.get().saturating_sub(1));
        if self_.pending.get() == 0 {
            self_.refresh_button.set_sensitive(true);
            self.update_count();
            if self_.store.n_items() == 0 {
                let unreachable = self_.unreachable.borrow().join(", ");
                if unreachable.is_empty() {
                    self.show_empty(
                        "Local library is empty",
                        "No sanitized assets were found in the configured directories.",
                    );
                } else {
                    self.show_empty(
                        "Local library not reachable",
                        &format!("Could not read: {unreachable}"),
                    );
                }
            } else {
                self_.content_stack.set_visible_child_name("list");
            }
        }
    }

    fn show_empty(&self, title: &str, description: &str) {
        let self_ = self.imp();
        self_.empty_page.set_title(title);
        self_.empty_page.set_description(Some(description));
        self_.content_stack.set_visible_child_name("empty");
        self_.refresh_button.set_sensitive(true);
        self_.count_label.set_label("");
    }

    fn update_count(&self) {
        let self_ = self.imp();
        if self_.pending.get() > 0 {
            return;
        }
        let shown = self_.selection.n_items();
        let total = self_.store.n_items();
        let mut text = if shown == total {
            format!("{total} item{}", if total == 1 { "" } else { "s" })
        } else {
            format!("{shown} of {total} items")
        };
        let unreachable = self_.unreachable.borrow();
        if !unreachable.is_empty() {
            text.push_str(&format!(" ({} directories unreachable)", unreachable.len()));
        }
        self_.count_label.set_label(&text);
    }

    fn update_filter(&self) {
        let self_ = self.imp();
        let search = self_.search.borrow().clone();
        let filter = gtk4::CustomFilter::new(move |obj| {
            let Some(data) = obj.downcast_ref::<LocalAssetData>() else {
                return false;
            };
            search.as_ref().is_none_or(|s| {
                data.name().to_lowercase().contains(s.as_str())
                    || data.kind().to_lowercase().contains(s.as_str())
            })
        });
        self_.filter_model.set_filter(Some(&filter));
    }

    fn selected(&self) -> Option<LocalAssetData> {
        self.imp()
            .selection
            .selected_item()
            .and_downcast::<LocalAssetData>()
    }

    fn update_detail(&self) {
        let self_ = self.imp();
        let Some(data) = self.selected() else {
            self_.detail_scroll.set_visible(false);
            self_.detail_separator.set_visible(false);
            return;
        };
        let Some(entry) = data.entry() else {
            return;
        };
        let sc = &entry.sidecar;
        self_.detail_name.set_label(&sc.display_name);
        self_
            .detail_kind
            .set_label(&kind_label(sc.kind).to_uppercase());

        let mut info = Vec::new();
        if let Some(engine) = &sc.engine_version_guess {
            info.push(format!("Engine version: UE {engine}"));
        }
        match sc.kind {
            Kind::Content => {
                if let Some(folder) = &sc.content_folder {
                    info.push(format!("Content folder: Content/{folder}"));
                }
            }
            Kind::Plugin => {
                if let Some(plugin) = &sc.plugin_name {
                    info.push(format!("Plugin: {plugin}"));
                }
                if let Some(friendly) = &sc.friendly_name {
                    info.push(format!("Name: {friendly}"));
                }
            }
            Kind::Project => {}
        }
        info.push(format!(
            "{} files, {}",
            sc.entry_count,
            byte_unit::Byte::from_u64(sc.total_uncompressed)
                .get_appropriate_unit(byte_unit::UnitType::Binary)
        ));
        info.push(format!("Source: {}", sc.source_zip));
        info.push(format!("Library: {}", entry.zip_path.display()));
        if let Some(url) = &sc.marketplace_url {
            info.push(format!("Marketplace: {url}"));
        }
        self_.detail_info.set_label(&info.join("\n"));

        if sc.external_refs.is_empty() {
            self_.detail_warning.set_visible(false);
        } else {
            let mut shown: Vec<&str> = sc
                .external_refs
                .iter()
                .take(8)
                .map(String::as_str)
                .collect();
            if sc.external_refs.len() > shown.len() {
                shown.push("...");
            }
            self_.detail_warning.set_label(&format!(
                "Warning: this asset references content outside of its own folder that must exist in the target project:\n{}",
                shown.join("\n")
            ));
            self_.detail_warning.set_visible(true);
        }

        let is_project = sc.kind == Kind::Project;
        self_.import_button.set_visible(!is_project);
        self_.new_project_button.set_visible(is_project);
        self_.import_button.set_label(match sc.kind {
            Kind::Plugin => "Install plugin to project",
            _ => "Import to project",
        });
        self_.detail_scroll.set_visible(true);
        self_.detail_separator.set_visible(true);
        self.update_fab_section(&data);
    }

    /// Whether an Epic account is logged in (the Fab matching is hidden offline).
    fn is_logged_in(&self) -> bool {
        self.imp().window.get().is_some_and(|w| {
            w.imp()
                .model
                .borrow()
                .epic_games
                .borrow()
                .user_details()
                .account_id
                .is_some()
        })
    }

    fn update_fab_section(&self, data: &LocalAssetData) {
        let self_ = self.imp();
        let overlay = data.overlay();
        let logged_in = self.is_logged_in();
        self_.match_button.set_visible(logged_in);
        self_.match_button.set_label(if overlay.is_some() {
            "Re-match on Fab"
        } else {
            "Match on Fab"
        });
        self_.clear_match_button.set_visible(overlay.is_some());
        self_
            .fab_picture
            .set_paintable(None::<&gtk4::gdk::Paintable>);
        self_.fab_picture.set_visible(false);

        let Some(overlay) = overlay else {
            self_.fab_box.set_visible(false);
            return;
        };
        self_.fab_box.set_visible(true);
        let title = match &overlay.seller {
            Some(seller) if !seller.is_empty() => format!("{} - {}", overlay.title, seller),
            _ => overlay.title.clone(),
        };
        self_.fab_title.set_label(&title);
        let url = overlay.listing_url();
        self_.fab_link.set_uri(&url);

        // Thumbnail: fetched off the main thread, dropped if the selection moved on.
        if let Some(thumb) = overlay.thumbnail_url.clone() {
            let handle = gio::spawn_blocking(move || {
                crate::HTTP_CLIENT
                    .get(thumb)
                    .send()
                    .ok()
                    .and_then(|r| r.bytes().ok())
                    .map(|b| b.to_vec())
            });
            glib::spawn_future_local(clone!(
                #[weak(rename_to=page)]
                self,
                async move {
                    let Ok(Some(bytes)) = handle.await else {
                        return;
                    };
                    let page_ = page.imp();
                    if page_.fab_link.uri().as_str() != url {
                        return;
                    }
                    if let Ok(texture) =
                        gtk4::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                    {
                        page_.fab_picture.set_paintable(Some(&texture));
                        page_.fab_picture.set_visible(true);
                    }
                }
            ));
        }
    }

    fn open_match_dialog(&self) {
        let self_ = self.imp();
        let (Some(data), Some(window)) = (self.selected(), self_.window.get()) else {
            return;
        };
        let Some(entry) = data.entry() else {
            return;
        };
        let sc = &entry.sidecar;
        let base = sc
            .friendly_name
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .unwrap_or(&sc.display_name);
        let query = ue_asset_library::clean_name(base).0;
        let dialog = fab_match_dialog::EpicFabMatchDialog::new();
        dialog.set_window(window);
        dialog.set_transient_for(Some(window));
        dialog.set_target(entry.zip_path.clone(), &sc.display_name, &query);
        dialog.connect_matched(clone!(
            #[weak(rename_to=page)]
            self,
            #[weak]
            data,
            move |overlay| {
                data.set_overlay(Some(overlay));
                // Only refresh the pane if the same entry is still selected.
                if page.selected().is_some_and(|s| s == data) {
                    page.update_detail();
                }
            }
        ));
        dialog.present();
    }

    /// Delete only the overlay file; the zip and the sanitizer sidecar are untouched.
    fn clear_match(&self) {
        let (Some(data), Some(window)) = (self.selected(), self.imp().window.get()) else {
            return;
        };
        let Some(entry) = data.entry() else {
            return;
        };
        let zip = entry.zip_path;
        let handle = gio::spawn_blocking(move || crate::models::fab_overlay::clear(&zip));
        glib::spawn_future_local(clone!(
            #[weak(rename_to=page)]
            self,
            #[weak]
            window,
            async move {
                match handle.await {
                    Ok(Ok(())) => {
                        data.set_overlay(None);
                        if page.selected().is_some_and(|s| s == data) {
                            page.update_detail();
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("Unable to clear Fab match: {e}");
                        window.add_notification(
                            "local-fab-match",
                            &format!("Unable to clear the Fab match: {e}"),
                            gtk4::MessageType::Error,
                        );
                    }
                    Err(_) => warn!("Clearing the Fab match panicked"),
                }
            }
        ));
    }

    fn open_import_dialog(&self, mode: import_dialog::Mode) {
        let self_ = self.imp();
        let (Some(data), Some(window)) = (self.selected(), self_.window.get()) else {
            return;
        };
        let Some(entry) = data.entry() else {
            return;
        };
        let dialog = import_dialog::EpicLocalImportDialog::new();
        dialog.set_window(window);
        dialog.set_transient_for(Some(window));
        dialog.set_entry(entry, mode);
        dialog.present();
    }
}

impl Refresh for EpicLocalLibraryBox {
    fn run_refresh(&self) {
        self.rescan();
    }

    fn can_be_refreshed(&self) -> bool {
        self.imp().pending.get() == 0
    }

    fn refresh_state_changed(&self) {}
}
