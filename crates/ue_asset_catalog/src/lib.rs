//! Cross-pack asset catalog, offline auto-tagger and stdio MCP server over the local Unreal
//! asset library (`ue_asset_library`). No GTK, no async runtime, no HTTP dependency tree.
//!
//! The catalog is derived data: the library zips, their sidecars and the `.assets.json` overlays
//! stay the source of truth. The GTK app can reuse this crate for a global "Assets" page:
//!
//! ```no_run
//! use std::path::PathBuf;
//! use ue_asset_catalog::{Catalog, LoadOptions, SearchQuery};
//! use ue_asset_catalog::import::{self, ProjectTarget};
//!
//! // Load (cache + overlays make this fast; missing indexes are built from the zips).
//! let dirs = [PathBuf::from("//server/ue_assets_library/library")];
//! let (catalog, report) = Catalog::load(&dirs, &LoadOptions::default());
//! println!("{} assets from {} packs", catalog.assets.len(), report.entries);
//!
//! // Search.
//! let hits = catalog.search(&SearchQuery { text: "grass".into(), ..Default::default() });
//! let asset = &catalog.assets[hits.hits[0].asset];
//!
//! // Plan (dry run), then apply with the library's safe import.
//! let project = ProjectTarget::from_path(&PathBuf::from("E:/Projects/My/My.uproject")).unwrap();
//! let ids = import::resolve_assets(&catalog, &[asset.id.clone()], 500).unwrap();
//! let plan = import::plan_import(&catalog, &project, &ids, true).unwrap();
//! let outcomes = import::apply_plan(&catalog, &plan, false);
//! ```
//!
//! # Public API surface
//!
//! * [`catalog`]: [`Catalog`] (`load`, `from_entries`, `search`, `find_asset`, `pack_by_id`,
//!   `match_packs`, `choose_pack`, `stats`, `needs_reload`, `add_generated`), [`catalog::Pack`],
//!   [`catalog::Family`] (releases of one product), [`catalog::Asset`], [`LoadOptions`],
//!   [`LoadReport`].
//! * [`search`]: [`SearchQuery`], [`search::SearchOutput`] (ranked, filtered, paged).
//! * [`tags`]: the deterministic auto-tagger (`tag_asset`, `tag_pack`, `tokenize`, kinds).
//! * [`release`]: the no-downgrade rule (`choose_release`, `ReleaseChoice`, `parse_version`).
//! * [`import`]: [`import::ProjectTarget`], [`import::plan_import`] (dry run),
//!   [`import::apply_plan`] (wraps `safe_import_subset`), [`import::log_install`].
//! * [`generated`] / [`generation`]: the read-only generated-assets source and the opt-in
//!   `request_generation` client.
//! * [`vision`] / [`enrich`]: optional vision-model descriptions (`<zip>.vision.json`).
//! * [`mcp`] / [`tools`] / [`config`]: the JSON-RPC server, its tools and command line.
//! * [`http_server`]: the read-only HTTP transport (`--http`), std::net only.
//! * [`bundles`]: the hosted bundle store (`prepare_import`, `GET /bundles/<id>`): a verified zip
//!   plus manifest the client applies to its own project.

pub mod b64;
pub mod bundles;
pub mod catalog;
pub mod config;
pub mod enrich;
pub mod generated;
pub mod generation;
pub mod http;
pub mod http_server;
pub mod import;
pub mod install_log;
pub mod mcp;
pub mod release;
pub mod search;
pub mod selftest;
pub mod tags;
pub mod tools;
pub mod vision;

#[cfg(test)]
mod tests_generation;
#[cfg(test)]
pub(crate) mod testutil;

pub use catalog::{Catalog, LoadOptions, LoadReport};
pub use mcp::Server;
pub use search::SearchQuery;
pub use tools::{App, AppConfig};
