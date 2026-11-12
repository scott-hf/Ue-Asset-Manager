//! Reading the library folder.

use crate::sidecar::Sidecar;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LibraryEntry {
    pub sidecar: Sidecar,
    pub zip_path: PathBuf,
    pub sidecar_path: PathBuf,
}

/// Read every `*.zip.json` whose zip exists. Orphan sidecars, temp files and
/// unparsable json are skipped; this never panics. Sorted by display name.
pub fn scan_library(dir: &Path) -> Vec<LibraryEntry> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(".tmp-") || !name.to_ascii_lowercase().ends_with(".zip.json") {
            continue;
        }
        let sidecar_path = e.path();
        let zip_path = dir.join(&name[..name.len() - ".json".len()]);
        if !zip_path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&sidecar_path) else {
            continue;
        };
        let text = text.trim_start_matches('\u{feff}');
        let Ok(sidecar) = serde_json::from_str::<Sidecar>(text) else {
            continue;
        };
        out.push(LibraryEntry {
            sidecar,
            zip_path,
            sidecar_path,
        });
    }
    out.sort_by(|a, b| {
        a.sidecar
            .display_name
            .to_lowercase()
            .cmp(&b.sidecar.display_name.to_lowercase())
            .then_with(|| a.zip_path.cmp(&b.zip_path))
    });
    out
}
