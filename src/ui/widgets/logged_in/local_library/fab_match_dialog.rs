//! "Match on Fab" dialog: searches the Fab marketplace for a local library
//! entry, lets the user pick one listing and stores it as an app-owned overlay
//! (`<zip>.fab.json`) next to the library zip. Nothing is matched automatically.

use crate::models::fab_overlay::{self, FabOverlay};
use crate::models::fab_search_data::FabSearchData;
use adw::subclass::prelude::AdwWindowImpl;
use egs_api::api::types::fab_search::{FabSearchListing, FabSearchParams};
use gtk4::glib::clone;
use gtk4::subclass::prelude::*;
use gtk4::{self, gio, prelude::*};
use gtk4::{glib, CompositeTemplate};
use log::{error, warn};
use std::path::PathBuf;

const MAX_CANDIDATES: usize = 8;

/// Messages from the worker threads back to the UI.
pub enum Msg {
    /// A candidate with its (optional) thumbnail bytes.
    Candidate(Box<FabSearchListing>, Option<Vec<u8>>),
    SearchDone(Result<usize, String>),
    Saved(Result<FabOverlay, String>),
}

type MatchedCallback = Box<dyn Fn(FabOverlay)>;

pub mod imp {
    use super::*;
    use crate::window::EpicAssetManagerWindow;
    use once_cell::sync::OnceCell;
    use std::cell::{Cell, RefCell};

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/achetagames/epic_asset_manager/fab_match_dialog.ui")]
    pub struct EpicFabMatchDialog {
        pub window: OnceCell<EpicAssetManagerWindow>,
        pub zip_path: RefCell<Option<PathBuf>>,
        pub serial: Cell<u64>,
        pub busy: Cell<bool>,
        pub model: gio::ListStore,
        pub selection: gtk4::SingleSelection,
        pub matched: RefCell<Option<MatchedCallback>>,
        pub sender: async_channel::Sender<(u64, Msg)>,
        pub receiver: RefCell<Option<async_channel::Receiver<(u64, Msg)>>>,
        #[template_child]
        pub title_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub query_entry: TemplateChild<gtk4::SearchEntry>,
        #[template_child]
        pub search_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub candidates_list: TemplateChild<gtk4::ListView>,
        #[template_child]
        pub spinner: TemplateChild<gtk4::Spinner>,
        #[template_child]
        pub status_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub select_button: TemplateChild<gtk4::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EpicFabMatchDialog {
        const NAME: &'static str = "EpicFabMatchDialog";
        type Type = super::EpicFabMatchDialog;
        type ParentType = adw::Window;

        fn new() -> Self {
            let (sender, receiver) = async_channel::unbounded();
            let model = gio::ListStore::new::<FabSearchData>();
            Self {
                window: OnceCell::new(),
                zip_path: RefCell::new(None),
                serial: Cell::new(0),
                busy: Cell::new(false),
                selection: gtk4::SingleSelection::builder()
                    .model(&model)
                    .autoselect(false)
                    .can_unselect(true)
                    .build(),
                model,
                matched: RefCell::new(None),
                sender,
                receiver: RefCell::new(Some(receiver)),
                title_label: TemplateChild::default(),
                query_entry: TemplateChild::default(),
                search_button: TemplateChild::default(),
                candidates_list: TemplateChild::default(),
                spinner: TemplateChild::default(),
                status_label: TemplateChild::default(),
                select_button: TemplateChild::default(),
            }
        }

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for EpicFabMatchDialog {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_list();
            obj.setup_actions();
            obj.setup_receiver();
        }
    }

    impl WidgetImpl for EpicFabMatchDialog {}
    impl WindowImpl for EpicFabMatchDialog {}
    impl AdwWindowImpl for EpicFabMatchDialog {}
}

glib::wrapper! {
    pub struct EpicFabMatchDialog(ObjectSubclass<imp::EpicFabMatchDialog>)
        @extends gtk4::Widget, gtk4::Window, adw::Window,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl Default for EpicFabMatchDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl EpicFabMatchDialog {
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

    /// Called on the main thread after the overlay was written.
    pub fn connect_matched<F: Fn(FabOverlay) + 'static>(&self, f: F) {
        self.imp().matched.replace(Some(Box::new(f)));
    }

    pub fn set_target(&self, zip_path: PathBuf, display_name: &str, query: &str) {
        let self_ = self.imp();
        self_.zip_path.replace(Some(zip_path));
        self_
            .title_label
            .set_label(&format!("Match \"{display_name}\" on Fab"));
        self_.query_entry.set_text(query);
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
            let picture = gtk4::Picture::new();
            picture.set_size_request(96, 54);
            picture.set_content_fit(gtk4::ContentFit::Cover);
            let texts = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            texts.set_valign(gtk4::Align::Center);
            let title = gtk4::Label::new(None);
            title.set_halign(gtk4::Align::Start);
            title.set_xalign(0.0);
            title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            title.add_css_class("heading");
            let sub = gtk4::Label::new(None);
            sub.set_halign(gtk4::Align::Start);
            sub.set_xalign(0.0);
            sub.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            sub.add_css_class("caption");
            sub.add_css_class("dim-label");
            texts.append(&title);
            texts.append(&sub);
            row.append(&picture);
            row.append(&texts);
            item.set_child(Some(&row));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let Some(data) = item.item().and_downcast::<FabSearchData>() else {
                return;
            };
            let Some(row) = item.child().and_downcast::<gtk4::Box>() else {
                return;
            };
            if let Some(picture) = row.first_child().and_downcast::<gtk4::Picture>() {
                picture.set_paintable(data.image().as_ref());
            }
            if let Some(texts) = row.last_child().and_downcast::<gtk4::Box>() {
                if let Some(title) = texts.first_child().and_downcast::<gtk4::Label>() {
                    title.set_label(&data.title());
                }
                if let Some(sub) = texts.last_child().and_downcast::<gtk4::Label>() {
                    let seller = data.seller_name();
                    let kind = data.listing_type();
                    sub.set_label(&match (seller.is_empty(), kind.is_empty()) {
                        (false, false) => format!("{seller} - {kind}"),
                        (false, true) => seller,
                        (true, false) => kind,
                        (true, true) => String::new(),
                    });
                }
            }
        });

        self_.selection.connect_selection_changed(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_, _, _| dialog.update_buttons()
        ));
        self_.candidates_list.set_model(Some(&self_.selection));
        self_.candidates_list.set_factory(Some(&factory));
    }

    fn setup_actions(&self) {
        let self_ = self.imp();
        self_.search_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.start_search()
        ));
        self_.query_entry.connect_activate(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.start_search()
        ));
        self_.select_button.connect_clicked(clone!(
            #[weak(rename_to=dialog)]
            self,
            move |_| dialog.save_selected()
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

    fn update_buttons(&self) {
        let self_ = self.imp();
        self_
            .select_button
            .set_sensitive(!self_.busy.get() && self_.selection.selected_item().is_some());
    }

    fn set_status(&self, busy: bool, text: &str) {
        let self_ = self.imp();
        if busy {
            self_.spinner.start();
        } else {
            self_.spinner.stop();
        }
        self_.status_label.set_label(text);
    }

    fn start_search(&self) {
        let self_ = self.imp();
        let query = self_.query_entry.text().trim().to_string();
        if query.is_empty() {
            return;
        }
        let Some(window) = self_.window.get() else {
            return;
        };
        let serial = self_.serial.get() + 1;
        self_.serial.set(serial);
        self_.model.remove_all();
        self.update_buttons();
        self.set_status(true, "Searching Fab...");

        let eg = window.imp().model.borrow().epic_games.borrow().clone();
        let sender = self_.sender.clone();
        std::thread::spawn(move || {
            let params = FabSearchParams {
                channels: Some("unreal-engine".to_string()),
                count: Some(MAX_CANDIDATES as u32),
                q: Some(fab_overlay::encode_query(&query)),
                ..Default::default()
            };
            match crate::RUNTIME.block_on(eg.try_fab_search(&params)) {
                Ok(results) => {
                    let mut sent = 0;
                    for listing in results.results.into_iter().take(MAX_CANDIDATES) {
                        let thumb = thumbnail_url(&listing).and_then(|url| {
                            crate::HTTP_CLIENT
                                .get(url)
                                .send()
                                .ok()
                                .and_then(|r| r.bytes().ok())
                                .map(|b| b.to_vec())
                        });
                        sent += 1;
                        if sender
                            .send_blocking((serial, Msg::Candidate(Box::new(listing), thumb)))
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = sender.send_blocking((serial, Msg::SearchDone(Ok(sent))));
                }
                Err(e) => {
                    let _ = sender.send_blocking((serial, Msg::SearchDone(Err(e.to_string()))));
                }
            }
        });
    }

    fn save_selected(&self) {
        let self_ = self.imp();
        let (Some(item), Some(zip)) = (
            self_
                .selection
                .selected_item()
                .and_downcast::<FabSearchData>(),
            self_.zip_path.borrow().clone(),
        ) else {
            return;
        };
        let Some(listing) = item.listing() else {
            return;
        };
        let overlay = FabOverlay::new(
            listing.uid.clone(),
            listing.title.clone().unwrap_or_default(),
            listing.user.as_ref().and_then(|u| u.seller_name.clone()),
            thumbnail_url(&listing),
        );
        self_.busy.set(true);
        self.update_buttons();
        self.set_status(true, "Saving match...");
        let sender = self_.sender.clone();
        let serial = self_.serial.get();
        std::thread::spawn(move || {
            let result = fab_overlay::save(&zip, &overlay)
                .map(|()| overlay)
                .map_err(|e| e.to_string());
            let _ = sender.send_blocking((serial, Msg::Saved(result)));
        });
    }

    fn handle_msg(&self, serial: u64, msg: Msg) {
        let self_ = self.imp();
        match msg {
            Msg::Candidate(listing, bytes) => {
                if serial != self_.serial.get() {
                    return;
                }
                let texture = bytes
                    .and_then(|b| gtk4::gdk::Texture::from_bytes(&glib::Bytes::from_owned(b)).ok());
                self_.model.append(&FabSearchData::new(&listing, texture));
            }
            Msg::SearchDone(result) => {
                if serial != self_.serial.get() {
                    return;
                }
                match result {
                    Ok(0) => {
                        self.set_status(false, "No listings found. Try different search text.")
                    }
                    Ok(n) => {
                        self.set_status(false, &format!("{n} candidates, pick the right one."))
                    }
                    Err(e) => {
                        warn!("Fab search failed: {e}");
                        self.set_status(false, &format!("Search failed: {e}"));
                        if let Some(window) = self_.window.get() {
                            window.add_notification(
                                "local-fab-match",
                                &format!("Fab search failed: {e}"),
                                gtk4::MessageType::Error,
                            );
                        }
                    }
                }
            }
            Msg::Saved(result) => {
                self_.busy.set(false);
                match result {
                    Ok(overlay) => {
                        if let Some(cb) = self_.matched.borrow().as_ref() {
                            cb(overlay);
                        }
                        self.close();
                    }
                    Err(e) => {
                        error!("Unable to save Fab match: {e}");
                        self.set_status(false, &format!("Unable to save the match: {e}"));
                        if let Some(window) = self_.window.get() {
                            window.add_notification(
                                "local-fab-match",
                                &format!("Unable to save the Fab match: {e}"),
                                gtk4::MessageType::Error,
                            );
                        }
                        self.update_buttons();
                    }
                }
            }
        }
    }

    pub fn present(&self) {
        gtk4::prelude::GtkWindowExt::present(self);
        self.start_search();
    }
}

fn thumbnail_url(listing: &FabSearchListing) -> Option<String> {
    listing
        .thumbnails
        .as_ref()
        .and_then(|thumbs| thumbs.iter().find_map(|t| t.media_url.clone()))
}
