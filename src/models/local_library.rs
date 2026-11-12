//! GObject wrapper for an entry of the local (offline) asset library.
//!
//! The library itself (zips + sidecar manifests) is produced by the
//! `ue_asset_sanitizer` tool of the `ue_asset_library` crate; this model only
//! exposes what the UI needs and keeps the full [`LibraryEntry`] for importing.

use crate::models::fab_overlay::{self, FabOverlay};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::path::{Path, PathBuf};
use ue_asset_library::{Kind, LibraryEntry};

mod imp {
    use super::*;
    use glib::Properties;
    use std::cell::RefCell;

    #[derive(Debug, Default, Properties)]
    #[properties(wrapper_type = super::LocalAssetData)]
    pub struct LocalAssetData {
        #[property(get, set)]
        id: RefCell<String>,
        #[property(get, set)]
        name: RefCell<String>,
        #[property(get, set)]
        kind: RefCell<String>,
        #[property(get, set, name = "engine-version")]
        engine_version: RefCell<String>,
        #[property(get, set, name = "library-dir")]
        library_dir: RefCell<String>,
        pub entry: RefCell<Option<LibraryEntry>>,
        pub overlay: RefCell<Option<FabOverlay>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LocalAssetData {
        const NAME: &'static str = "LocalAssetData";
        type Type = super::LocalAssetData;
        type ParentType = glib::Object;
    }

    #[glib::derived_properties]
    impl ObjectImpl for LocalAssetData {}
}

glib::wrapper! {
    pub struct LocalAssetData(ObjectSubclass<imp::LocalAssetData>);
}

/// Human readable label for a library entry kind.
pub fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Content => "Content",
        Kind::Plugin => "Plugin",
        Kind::Project => "Project",
    }
}

impl LocalAssetData {
    pub fn new(entry: &LibraryEntry, library_dir: &Path) -> Self {
        let data: Self = glib::Object::builder()
            .property("id", entry.sidecar.sha256.as_str())
            .property("name", entry.sidecar.display_name.as_str())
            .property("kind", kind_label(entry.sidecar.kind))
            .property(
                "engine-version",
                entry
                    .sidecar
                    .engine_version_guess
                    .clone()
                    .unwrap_or_default(),
            )
            .property("library-dir", library_dir.to_string_lossy().as_ref())
            .build();
        data.imp().entry.replace(Some(entry.clone()));
        data
    }

    pub fn overlay(&self) -> Option<FabOverlay> {
        self.imp().overlay.borrow().clone()
    }

    pub fn set_overlay(&self, overlay: Option<FabOverlay>) {
        self.imp().overlay.replace(overlay);
    }

    pub fn entry(&self) -> Option<LibraryEntry> {
        self.imp().entry.borrow().clone()
    }

    pub fn entry_kind(&self) -> Option<Kind> {
        self.imp().entry.borrow().as_ref().map(|e| e.sidecar.kind)
    }
}

/// Result of scanning one configured library directory.
#[derive(Debug)]
pub struct ScanResult {
    pub generation: u64,
    pub dir: PathBuf,
    /// `None` when the directory could not be reached.
    pub entries: Option<Vec<(LibraryEntry, Option<FabOverlay>)>>,
}

/// Scan one directory (blocking; call from a worker thread). Never panics on
/// an unreachable directory, it reports `entries: None` instead.
pub fn scan_directory(generation: u64, dir: PathBuf) -> ScanResult {
    if !dir.is_dir() {
        log::warn!(
            "Local library directory {} is not reachable, skipping",
            dir.display()
        );
        return ScanResult {
            generation,
            dir,
            entries: None,
        };
    }
    // The Fab overlay is read here (worker thread) so the UI never touches the share.
    let entries = ue_asset_library::scan_library(&dir)
        .into_iter()
        .map(|e| {
            let overlay = fab_overlay::load(&e.zip_path);
            (e, overlay)
        })
        .collect();
    ScanResult {
        generation,
        dir,
        entries: Some(entries),
    }
}
