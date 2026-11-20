//! Packs the user filtered out of the Assets page, kept in `hidden_packs.json` in the app's user
//! data dir (`{"schema":1,"packs":[{"slug","name"}]}`). A plain file, not a GSettings key: the
//! installed schema can be stale and reading an unknown key panics. A missing, corrupt or
//! foreign-schema file is treated as "nothing hidden".

use crate::models::fab_cache::write_atomic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenPack {
    /// Catalog family slug (what the search excludes).
    pub slug: String,
    /// Display name, for the list of hidden packs.
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct File {
    schema: u32,
    packs: Vec<HiddenPack>,
}

pub fn default_path() -> PathBuf {
    gtk4::glib::user_data_dir()
        .join("epic_asset_manager")
        .join("hidden_packs.json")
}

pub fn load() -> Vec<HiddenPack> {
    load_from(&default_path())
}

pub fn save(packs: &[HiddenPack]) {
    if let Err(e) = save_to(&default_path(), packs) {
        log::warn!("Unable to save the hidden packs: {e}");
    }
}

pub fn load_from(path: &Path) -> Vec<HiddenPack> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<File>(&bytes) {
        Ok(file) if file.schema == SCHEMA => file.packs,
        _ => Vec::new(),
    }
}

pub fn save_to(path: &Path, packs: &[HiddenPack]) -> std::io::Result<()> {
    let file = File {
        schema: SCHEMA,
        packs: packs.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    write_atomic(path, &bytes)
}
