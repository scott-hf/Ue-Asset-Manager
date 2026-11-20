//! Sanitizer and importer for locally stored Unreal Engine asset zips.
//!
//! * [`sanitize_zip`] turns a dropped zip into a hardened, normalised library zip plus a
//!   versioned sidecar manifest (written last as the commit marker).
//! * [`scan_library`] lists library entries (GUI side, no GTK here).
//! * [`preview_import`], [`safe_import`] and [`safe_import_new_project`] put an entry into
//!   an Unreal project with whitelist + hardening and backups instead of `.bak` files.

pub mod archive;
pub mod asset_index;
pub mod config;
pub mod error;
pub mod harden;
pub mod import;
pub mod layout;
pub mod lease;
pub mod library;
pub mod name;
pub mod nested;
pub mod pipeline;
pub mod rotating_log;
pub mod sanitize;
pub mod sidecar;
pub mod uasset;
mod util;
mod zipfix;

pub use archive::{group_volumes, sanitize_archive, sanitize_archive_with, ArchiveKind, VolumeSet};
pub use asset_index::{
    build_index, closure, index_path, load_or_build_index, read_thumbnail, write_index, AssetIndex,
    OtherFile, PackageInfo, INDEX_SCHEMA,
};
pub use error::{ImportError, NeedsReview, SanitizeError};
pub use import::{
    preview_import, preview_import_subset, safe_import, safe_import_new_project,
    safe_import_subset, ImportPolicy, ImportReport, Selection,
};
pub use layout::{plan_layout, EntryRename, Layout};
pub use library::{scan_library, LibraryEntry};
pub use name::{clean_name, clean_name_ranged};
pub use sanitize::{sanitize_zip, sanitize_zip_with_options, SanitizeOptions, SanitizeOutcome};
pub use sidecar::{Kind, Sidecar};
