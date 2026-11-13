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

/// The directory that holds the `*.zip.json` sidecars. The sanitizer share has the layout
/// `<share>/{import,library,needs-review,...}`; when the configured directory is the share root
/// (no sidecars of its own, but a `library` folder) the `library` folder is used.
fn library_dir_of(dir: PathBuf) -> PathBuf {
    let has_sidecars = std::fs::read_dir(&dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".zip.json")
        })
    });
    let nested = dir.join("library");
    if !has_sidecars && nested.is_dir() {
        log::info!(
            "Local library {} is a share root, scanning {}",
            dir.display(),
            nested.display()
        );
        return nested;
    }
    dir
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
    let dir = library_dir_of(dir);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eam-locallib-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_directory_is_reported_as_unreachable() {
        let result = scan_directory(1, PathBuf::from("Z:/definitely/not/here"));
        assert!(result.entries.is_none());
    }

    #[test]
    fn share_root_falls_back_to_its_library_folder() {
        let root = temp_dir("root");
        std::fs::create_dir_all(root.join("library")).unwrap();
        std::fs::create_dir_all(root.join("import")).unwrap();
        assert_eq!(library_dir_of(root.clone()), root.join("library"));
        // a directory with sidecars of its own is used as it is
        std::fs::write(root.join("a.zip.json"), "{}").unwrap();
        assert_eq!(library_dir_of(root.clone()), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reads the real share: `cargo test -- --ignored real_share --nocapture`. Override the
    /// directory with `UE_LIBRARY_TEST_DIR`. Checks the forward-slash, backslash and share-root
    /// spellings of the path.
    #[test]
    #[ignore = "needs the scott-server share"]
    fn real_share_is_scanned_in_every_path_spelling() {
        let base = std::env::var("UE_LIBRARY_TEST_DIR")
            .unwrap_or_else(|_| "//scott-server/ue_assets_library/library".to_string());
        let forward = base.replace('\\', "/");
        let back = forward.replace('/', "\\");
        let root = forward
            .trim_end_matches('/')
            .trim_end_matches("/library")
            .to_string();
        for spelling in [forward.clone(), back, root] {
            let result = scan_directory(1, PathBuf::from(&spelling));
            let entries = result
                .entries
                .unwrap_or_else(|| panic!("{spelling} not reachable"));
            let names: Vec<String> = entries
                .iter()
                .map(|(e, _)| {
                    e.zip_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            println!("{spelling}: {} entries", names.len());
            for wanted in [
                "advanced-crafting-system",
                "camping-animations",
                "operating-room-pack",
            ] {
                assert!(
                    names.iter().any(|n| n.starts_with(wanted)),
                    "{wanted} missing in {spelling}: {names:?}"
                );
            }
        }
    }
}
