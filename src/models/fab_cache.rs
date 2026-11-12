//! On-disk cache of the Fab library listing (per-asset release/version details included).
//!
//! Layout below `<cache dir>/fab/`: `index.json` (schema version + asset ids) and one
//! `<asset id>/fab_asset.json` per asset. Everything is written atomically (temp file + rename);
//! unreadable or foreign-schema data is ignored, never fatal.

use egs_api::api::types::fab_library::FabAsset;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bump when the cached data can no longer be read by older/newer code.
pub const SCHEMA_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const ASSET_FILE: &str = "fab_asset.json";

#[derive(Debug, Serialize, Deserialize)]
struct CacheIndex {
    schema: u32,
    ids: Vec<String>,
}

pub fn cache_dir(base: &str) -> PathBuf {
    let mut path = PathBuf::from(base);
    path.push("fab");
    path
}

/// Write `bytes` to `path` through a temp file in the same directory, then rename over it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = parent.join(tmp_name);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `false` when the index exists but is corrupt or from another schema (then the cache is ignored).
/// A missing index (cache written before the index existed) is accepted.
pub fn is_usable(dir: &Path) -> bool {
    match std::fs::read(dir.join(INDEX_FILE)) {
        Ok(bytes) => serde_json::from_slice::<CacheIndex>(&bytes)
            .map(|i| i.schema == SCHEMA_VERSION)
            .unwrap_or(false),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

pub fn write_index(dir: &Path, ids: &[String]) -> std::io::Result<()> {
    let index = CacheIndex {
        schema: SCHEMA_VERSION,
        ids: ids.to_vec(),
    };
    let bytes = serde_json::to_vec(&index).map_err(std::io::Error::other)?;
    write_atomic(&dir.join(INDEX_FILE), &bytes)
}

pub fn write_asset(dir: &Path, asset: &FabAsset) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(asset).map_err(std::io::Error::other)?;
    write_atomic(&dir.join(&asset.asset_id).join(ASSET_FILE), &bytes)
}

/// Load every readable cached asset; corrupt entries are skipped.
pub fn load_all(dir: &Path) -> HashMap<String, FabAsset> {
    let mut result = HashMap::new();
    if !is_usable(dir) {
        return result;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let file = entry.path().join(ASSET_FILE);
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        match serde_json::from_slice::<FabAsset>(&bytes) {
            Ok(asset) => {
                result.insert(asset.asset_id.clone(), asset);
            }
            Err(e) => log::warn!("Ignoring corrupt FAB cache entry {:?}: {}", file, e),
        }
    }
    result
}

/// Delete cached assets that are no longer in the library.
pub fn remove_assets(dir: &Path, ids: &[String]) {
    for id in ids {
        if id.is_empty() || id.contains(['/', '\\']) {
            continue;
        }
        let _ = std::fs::remove_dir_all(dir.join(id));
    }
}

/// Names of the asset folders on disk (ids), whether or not their content is readable.
pub fn ids_on_disk(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Difference between the cached listing and a freshly fetched one.
#[derive(Debug, Default, PartialEq)]
pub struct Delta {
    /// In the fetched listing only.
    pub added: Vec<FabAsset>,
    /// In both, but the listing entry (versions, images, text...) differs.
    pub changed: Vec<FabAsset>,
    /// In the cache only (no longer in the library).
    pub removed: Vec<String>,
    /// In both and identical.
    pub unchanged: usize,
}

/// Compare the fetched listing against the cache (by asset id). Order of `added`/`changed` follows
/// `fetched`; `removed` is sorted.
pub fn merge_delta(cached: &HashMap<String, FabAsset>, fetched: Vec<FabAsset>) -> Delta {
    let mut delta = Delta::default();
    let mut seen: HashSet<String> = HashSet::new();
    for asset in fetched {
        if !seen.insert(asset.asset_id.clone()) {
            continue; // duplicate in the listing
        }
        match cached.get(&asset.asset_id) {
            None => delta.added.push(asset),
            Some(old) if *old != asset => delta.changed.push(asset),
            Some(_) => delta.unchanged += 1,
        }
    }
    let mut removed: Vec<String> = cached
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    removed.sort();
    delta.removed = removed;
    delta
}

#[cfg(test)]
mod tests {
    use super::*;
    use egs_api::api::types::fab_library::ProjectVersion;

    fn asset(id: &str, engines: &[&str]) -> FabAsset {
        FabAsset {
            asset_id: id.to_string(),
            title: format!("Asset {id}"),
            project_versions: vec![ProjectVersion {
                artifact_id: format!("{id}_art"),
                engine_versions: engines.iter().map(|e| (*e).to_string()).collect(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn map(list: &[FabAsset]) -> HashMap<String, FabAsset> {
        list.iter()
            .map(|a| (a.asset_id.clone(), a.clone()))
            .collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eam-fab-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn delta_splits_added_changed_removed_unchanged() {
        let cached = map(&[
            asset("keep", &["UE_5.4"]),
            asset("upd", &["UE_5.4"]),
            asset("gone", &["UE_5.0"]),
        ]);
        let fetched = vec![
            asset("keep", &["UE_5.4"]),
            asset("upd", &["UE_5.4", "UE_5.5"]), // a new engine version was added
            asset("new", &["UE_5.8"]),
            asset("new", &["UE_5.8"]), // duplicate entry is ignored
        ];
        let delta = merge_delta(&cached, fetched);
        assert_eq!(delta.unchanged, 1);
        assert_eq!(
            delta
                .added
                .iter()
                .map(|a| a.asset_id.as_str())
                .collect::<Vec<_>>(),
            ["new"]
        );
        assert_eq!(
            delta
                .changed
                .iter()
                .map(|a| a.asset_id.as_str())
                .collect::<Vec<_>>(),
            ["upd"]
        );
        assert_eq!(delta.removed, ["gone"]);
    }

    #[test]
    fn empty_cache_means_everything_is_new() {
        let delta = merge_delta(&HashMap::new(), vec![asset("a", &[]), asset("b", &[])]);
        assert_eq!(delta.added.len(), 2);
        assert!(delta.changed.is_empty() && delta.removed.is_empty());
    }

    #[test]
    fn identical_listing_is_all_unchanged() {
        let cached = map(&[asset("a", &["UE_5.1"]), asset("b", &["UE_5.2"])]);
        let delta = merge_delta(
            &cached,
            vec![asset("b", &["UE_5.2"]), asset("a", &["UE_5.1"])],
        );
        assert_eq!(delta.unchanged, 2);
        assert!(delta.added.is_empty() && delta.changed.is_empty() && delta.removed.is_empty());
    }

    #[test]
    fn cache_round_trip_and_corruption_is_tolerated() {
        let dir = temp_dir("roundtrip");
        write_asset(&dir, &asset("a", &["UE_5.1"])).unwrap();
        write_asset(&dir, &asset("b", &["UE_5.2"])).unwrap();
        write_index(&dir, &["a".to_string(), "b".to_string()]).unwrap();
        assert!(is_usable(&dir));
        assert_eq!(load_all(&dir).len(), 2);

        // a corrupt asset file is skipped, the rest still loads
        std::fs::write(dir.join("b").join(ASSET_FILE), b"{ not json").unwrap();
        let loaded = load_all(&dir);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("a"));

        // a corrupt index or another schema makes the whole cache ignored
        std::fs::write(dir.join(INDEX_FILE), b"garbage").unwrap();
        assert!(!is_usable(&dir));
        assert!(load_all(&dir).is_empty());
        std::fs::write(dir.join(INDEX_FILE), br#"{"schema":99,"ids":[]}"#).unwrap();
        assert!(!is_usable(&dir));

        // a missing index (legacy cache) is accepted
        std::fs::remove_file(dir.join(INDEX_FILE)).unwrap();
        assert!(is_usable(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_replaces_without_leftovers() {
        let dir = temp_dir("atomic");
        let file = dir.join("x.json");
        write_atomic(&file, b"one").unwrap();
        write_atomic(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["x.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_assets_ignores_path_tricks() {
        let dir = temp_dir("remove");
        write_asset(&dir, &asset("a", &[])).unwrap();
        remove_assets(&dir, &["../evil".to_string(), "a".to_string()]);
        assert!(ids_on_disk(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
