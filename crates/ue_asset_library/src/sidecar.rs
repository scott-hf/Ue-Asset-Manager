//! Versioned sidecar manifest, one per library zip: `<slug>.<sha12>.zip.json`.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Content,
    Plugin,
    Project,
}

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sidecar {
    pub schema: u32,
    pub kind: Kind,
    /// `clean_name(zip stem)`.
    pub display_name: String,
    /// Original file name of the dropped zip.
    pub source_zip: String,
    /// SHA-256 of the dropped (source) zip; its first 12 hex chars are in the library file name.
    pub sha256: String,
    /// "5.5"; real UE versions only; the `.uplugin` EngineVersion wins.
    #[serde(default)]
    pub engine_version_guess: Option<String>,
    #[serde(default)]
    pub content_folder: Option<String>,
    #[serde(default)]
    pub plugin_name: Option<String>,
    /// The rewrite rule applied (audit).
    #[serde(default)]
    pub zip_prefix: String,
    #[serde(default)]
    pub game_prefix: String,
    /// `["Content/OperatingRoom"]` or `["Plugins/Name"]`; empty for projects.
    #[serde(default)]
    pub allowed_roots: Vec<String>,
    pub entry_count: u64,
    pub total_uncompressed: u64,
    /// `/Game/` references outside the pack (dependency warning).
    #[serde(default)]
    pub external_refs: Vec<String>,
    /// RFC3339 UTC.
    #[serde(default)]
    pub sanitized_at: String,
    #[serde(default)]
    pub marketplace_url: Option<String>,
    #[serde(default)]
    pub friendly_name: Option<String>,
    /// Unknown fields are preserved.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
