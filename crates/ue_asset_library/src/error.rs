//! Error and diagnostic types.

use crate::sidecar::Kind;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Why a zip could not be sanitized automatically and needs a human look.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum NeedsReview {
    /// No asset had a resolvable `/Game/...` own path.
    NoGameMatch,
    /// The rewritten layout has several content roots.
    MultiRoot(Vec<String>),
    /// Sampled assets implied different rewrite rules.
    RuleDisagree(Vec<String>),
    Ambiguous(String),
    TooManyUnresolved {
        unresolved: usize,
        total: usize,
    },
    /// No content would be left after filtering.
    NoContent,
    /// Hardening rejection; `code` is one of SLIP, ABSOLUTE, CASE_COLLISION, ...
    Unsafe {
        code: String,
        detail: String,
    },
}

impl NeedsReview {
    pub fn unsafe_entry(code: &str, detail: impl Into<String>) -> Self {
        NeedsReview::Unsafe {
            code: code.to_string(),
            detail: detail.into(),
        }
    }

    /// Stable diagnostic code.
    pub fn code(&self) -> String {
        match self {
            NeedsReview::NoGameMatch => "NO_GAME_MATCH".into(),
            NeedsReview::MultiRoot(_) => "MULTI_ROOT".into(),
            NeedsReview::RuleDisagree(_) => "RULE_DISAGREE".into(),
            NeedsReview::Ambiguous(_) => "AMBIGUOUS".into(),
            NeedsReview::TooManyUnresolved { .. } => "TOO_MANY_UNRESOLVED".into(),
            NeedsReview::NoContent => "NO_CONTENT".into(),
            NeedsReview::Unsafe { code, .. } => code.clone(),
        }
    }
}

impl fmt::Display for NeedsReview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NeedsReview::NoGameMatch => {
                write!(f, "NO_GAME_MATCH: no asset has a resolvable /Game/ path")
            }
            NeedsReview::MultiRoot(r) => {
                write!(f, "MULTI_ROOT: several content roots: {}", r.join(", "))
            }
            NeedsReview::RuleDisagree(r) => write!(f, "RULE_DISAGREE: {}", r.join("; ")),
            NeedsReview::Ambiguous(s) => write!(f, "AMBIGUOUS: {s}"),
            NeedsReview::TooManyUnresolved { unresolved, total } => {
                write!(
                    f,
                    "TOO_MANY_UNRESOLVED: {unresolved} of {total} assets unresolved"
                )
            }
            NeedsReview::NoContent => write!(f, "NO_CONTENT: nothing left after filtering"),
            NeedsReview::Unsafe { code, detail } => write!(f, "{code}: {detail}"),
        }
    }
}

impl std::error::Error for NeedsReview {}

#[derive(Debug)]
pub enum SanitizeError {
    /// Needs a human decision; the zip goes to `needs-review/`.
    NeedsReview(NeedsReview),
    /// The archive looks incomplete (no central directory): retry later.
    Truncated(String),
    /// Transient I/O problem (share hiccup, unreadable file): retried, counted as an attempt.
    Io(String),
    /// The file is in use by another process (sharing violation): retried, never counted.
    Busy(String),
    /// The archive is unreadable for good.
    Corrupt(String),
}

impl fmt::Display for SanitizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SanitizeError::NeedsReview(n) => write!(f, "needs review: {n}"),
            SanitizeError::Truncated(s) => write!(f, "truncated or incomplete zip: {s}"),
            SanitizeError::Io(s) => write!(f, "i/o error: {s}"),
            SanitizeError::Busy(s) => write!(f, "file busy: {s}"),
            SanitizeError::Corrupt(s) => write!(f, "corrupt zip: {s}"),
        }
    }
}

impl std::error::Error for SanitizeError {}

impl From<std::io::Error> for SanitizeError {
    fn from(e: std::io::Error) -> Self {
        if crate::util::is_busy_error(&e) {
            return SanitizeError::Busy(e.to_string());
        }
        SanitizeError::Io(e.to_string())
    }
}

#[derive(Debug)]
pub enum ImportError {
    Io(String),
    Zip(String),
    WrongKind(Kind),
    NotAProject(PathBuf),
    /// Hardening rejected the library zip.
    Unsafe(NeedsReview),
    OutsideAllowedRoots(String),
    SidecarMismatch(String),
    SizeMismatch(String),
    InvalidName(String),
    TargetExists(PathBuf),
    PathConflict(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Io(s) => write!(f, "i/o error: {s}"),
            ImportError::Zip(s) => write!(f, "zip error: {s}"),
            ImportError::WrongKind(k) => write!(f, "operation not valid for {k:?} assets"),
            ImportError::NotAProject(p) => {
                write!(f, "not an Unreal project (no .uproject): {}", p.display())
            }
            ImportError::Unsafe(n) => write!(f, "unsafe archive: {n}"),
            ImportError::OutsideAllowedRoots(s) => write!(f, "entry outside allowed roots: {s}"),
            ImportError::SidecarMismatch(s) => write!(f, "zip does not match its sidecar: {s}"),
            ImportError::SizeMismatch(s) => write!(f, "size mismatch while extracting: {s}"),
            ImportError::InvalidName(s) => write!(f, "invalid project name: {s}"),
            ImportError::TargetExists(p) => write!(f, "target already exists: {}", p.display()),
            ImportError::PathConflict(s) => write!(f, "file/folder type conflict: {s}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        ImportError::Io(e.to_string())
    }
}

impl From<zip::result::ZipError> for ImportError {
    fn from(e: zip::result::ZipError) -> Self {
        ImportError::Zip(e.to_string())
    }
}

#[cfg(test)]
mod assert_traits {
    fn is_send_sync<T: Send + Sync + 'static>() {}
    #[test]
    fn errors_are_send_sync() {
        is_send_sync::<super::NeedsReview>();
        is_send_sync::<super::SanitizeError>();
        is_send_sync::<super::ImportError>();
    }
}
