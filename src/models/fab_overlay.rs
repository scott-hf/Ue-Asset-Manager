//! App-owned overlay linking a local library zip to a Fab listing.
//!
//! Stored as `<library zip file name>.fab.json` next to the library zip. The
//! sanitizer's own sidecar (`<zip>.json`) is never touched.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const OVERLAY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FabOverlay {
    pub schema: u32,
    pub listing_uid: String,
    pub title: String,
    #[serde(default)]
    pub seller: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub matched_at: String,
}

impl FabOverlay {
    pub fn new(
        listing_uid: String,
        title: String,
        seller: Option<String>,
        thumbnail_url: Option<String>,
    ) -> Self {
        Self {
            schema: OVERLAY_SCHEMA,
            listing_uid,
            title,
            seller,
            thumbnail_url,
            description: None,
            matched_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub fn listing_url(&self) -> String {
        format!("https://www.fab.com/listings/{}", self.listing_uid)
    }
}

/// Overlay file path for a library zip: `<zip file name>.fab.json`.
pub fn overlay_path(zip_path: &Path) -> PathBuf {
    let mut name = zip_path.file_name().unwrap_or_default().to_os_string();
    name.push(".fab.json");
    zip_path.with_file_name(name)
}

/// Load the overlay of a library zip. Missing file, unreadable file, bad json
/// or an unknown schema all yield `None`.
pub fn load(zip_path: &Path) -> Option<FabOverlay> {
    let path = overlay_path(zip_path);
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<FabOverlay>(&text) {
        Ok(o) if o.schema == OVERLAY_SCHEMA && !o.listing_uid.is_empty() => Some(o),
        Ok(_) => {
            log::warn!(
                "Ignoring Fab overlay {} (unsupported content)",
                path.display()
            );
            None
        }
        Err(e) => {
            log::warn!("Ignoring bad Fab overlay {}: {e}", path.display());
            None
        }
    }
}

/// Write the overlay through a temp file in the same directory + rename.
pub fn save(zip_path: &Path, overlay: &FabOverlay) -> std::io::Result<()> {
    let path = overlay_path(zip_path);
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let json = serde_json::to_vec_pretty(overlay)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let result = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Delete only the overlay (never the zip or the sidecar).
pub fn clear(zip_path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(overlay_path(zip_path)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Percent-encode a search query for use in a URL query string.
pub fn encode_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
