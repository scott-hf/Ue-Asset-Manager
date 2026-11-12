//! Sanitizer and importer for locally stored Unreal Engine asset zips.
//!
//! * [`sanitize_zip`] turns a dropped zip into a hardened, normalised library zip plus a
//!   versioned sidecar manifest (written last as the commit marker).
//! * [`scan_library`] lists library entries (GUI side, no GTK here).
//! * [`preview_import`], [`safe_import`] and [`safe_import_new_project`] put an entry into
//!   an Unreal project with whitelist + hardening and backups instead of `.bak` files.

pub mod archive;
pub mod config;
pub mod error;
pub mod harden;
pub mod import;
pub mod layout;
pub mod library;
pub mod name;
pub mod pipeline;
pub mod sanitize;
pub mod sidecar;
mod util;

pub use archive::{group_volumes, sanitize_archive, ArchiveKind, VolumeSet};
pub use error::{ImportError, NeedsReview, SanitizeError};
pub use import::{
    preview_import, safe_import, safe_import_new_project, ImportPolicy, ImportReport,
};
pub use layout::{plan_layout, EntryRename, Layout};
pub use library::{scan_library, LibraryEntry};
pub use name::{clean_name, clean_name_ranged};
pub use sanitize::{sanitize_zip, SanitizeOutcome};
pub use sidecar::{Kind, Sidecar};
