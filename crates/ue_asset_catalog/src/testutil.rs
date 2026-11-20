//! Test fixtures shared by unit tests: synthetic library entries and indexes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use ue_asset_library::{AssetIndex, Kind, LibraryEntry, PackageInfo, Sidecar, INDEX_SCHEMA};

/// A library entry that does not exist on disk (paths are fake).
pub fn entry(name: &str, sha: &str, engine: Option<&str>, kind: Kind) -> LibraryEntry {
    let slug = crate::tags::slugify(name);
    let sc = Sidecar {
        schema: 1,
        kind,
        display_name: name.to_string(),
        source_zip: format!("{name}.zip"),
        sha256: sha.to_string(),
        engine_version_guess: engine.map(str::to_string),
        content_folder: Some("Pack".to_string()),
        plugin_name: None,
        zip_prefix: String::new(),
        game_prefix: String::new(),
        allowed_roots: vec!["Content/Pack".to_string()],
        entry_count: 1,
        total_uncompressed: 1,
        external_refs: Vec::new(),
        sanitized_at: String::new(),
        marketplace_url: None,
        friendly_name: None,
        extra: serde_json::Map::new(),
    };
    LibraryEntry {
        sidecar: sc,
        zip_path: PathBuf::from(format!("/lib/{slug}.{}.zip", &sha[..12])),
        sidecar_path: PathBuf::from(format!("/lib/{slug}.{}.zip.json", &sha[..12])),
    }
}

pub fn pkg(path: &str, class: &str, deps: &[&str]) -> PackageInfo {
    PackageInfo {
        game_path: path.to_string(),
        zip_path: format!("Content{}.uasset", path.trim_start_matches("/Game")),
        companions: Vec::new(),
        class: Some(class.to_string()),
        size: 100,
        deps: deps.iter().map(|s| (*s).to_string()).collect(),
        external: Vec::new(),
        has_thumbnail: false,
        header_unreadable: false,
        unreadable_reason: None,
    }
}

pub fn index(sha: &str, pkgs: Vec<PackageInfo>) -> AssetIndex {
    AssetIndex {
        schema: INDEX_SCHEMA,
        sidecar_sha256: sha.to_string(),
        entry_count: 1,
        packages: pkgs,
        other_files: Vec::new(),
        engine_version_min: None,
        engine_version_max: None,
    }
}

static N: AtomicU64 = AtomicU64::new(0);

/// A temp directory removed on drop.
pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new() -> Tmp {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let p = std::env::temp_dir().join(format!(
            "ue_asset_catalog_tests/{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        Tmp(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    #[allow(dead_code)]
    pub fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.join(name);
        std::fs::create_dir_all(&p).expect("create dir");
        p
    }
}

impl Default for Tmp {
    fn default() -> Self {
        Tmp::new()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
