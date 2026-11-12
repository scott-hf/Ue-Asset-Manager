//! Archive hardening shared by the sanitizer and the importer.

use crate::error::NeedsReview;
use std::collections::HashMap;
use std::io::{Read, Seek};
use zip::ZipArchive;

pub const MAX_ENTRIES: usize = 200_000;
pub const MAX_UNCOMPRESSED: u64 = 50 * 1024 * 1024 * 1024;
pub const MAX_PATH_LEN: usize = 400;

/// One validated archive entry with a normalised, `/`-separated relative path.
#[derive(Clone, Debug)]
pub struct EntryInfo {
    pub index: usize,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn unsafe_(code: &str, detail: impl Into<String>) -> NeedsReview {
    NeedsReview::unsafe_entry(code, detail)
}

/// Normalise a raw entry name (backslashes to `/`) and reject unsafe names.
pub fn normalize_name(raw: &str) -> Result<String, NeedsReview> {
    let n = raw.replace('\\', "/");
    if n.starts_with('/') {
        return Err(unsafe_("ABSOLUTE", raw));
    }
    if n.contains(':') {
        return Err(unsafe_("DRIVE_OR_STREAM", raw));
    }
    if n.chars().any(|c| {
        (c as u32) < 0x20 || c == '\u{7f}' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return Err(unsafe_("INVALID_CHAR", raw));
    }
    let mut comps: Vec<&str> = Vec::new();
    for c in n.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            return Err(unsafe_("SLIP", raw));
        }
        if c.ends_with('.') || c.ends_with(' ') {
            return Err(unsafe_("TRAILING_DOT_SPACE", raw));
        }
        let stem = c.split('.').next().unwrap_or("").to_ascii_lowercase();
        if RESERVED.contains(&stem.as_str()) {
            return Err(unsafe_("RESERVED_NAME", raw));
        }
        comps.push(c);
    }
    if comps.is_empty() {
        return Err(unsafe_("EMPTY_NAME", raw));
    }
    Ok(comps.join("/"))
}

/// Check path length, case collisions, duplicates and file/dir conflicts.
pub fn check_paths(entries: &[EntryInfo]) -> Result<(), NeedsReview> {
    let mut files: HashMap<String, &str> = HashMap::new();
    let mut dirs: HashMap<String, String> = HashMap::new();
    for e in entries {
        if e.path.chars().count() > MAX_PATH_LEN {
            return Err(unsafe_("PATH_TOO_LONG", e.path.clone()));
        }
        let comps: Vec<&str> = e.path.split('/').collect();
        let proper = if e.is_dir {
            comps.len()
        } else {
            comps.len() - 1
        };
        for k in 1..=proper {
            let orig = comps[..k].join("/");
            let low = orig.to_lowercase();
            match dirs.get(&low) {
                Some(prev) if *prev != orig => {
                    return Err(unsafe_("CASE_COLLISION", format!("{prev} vs {orig}")))
                }
                Some(_) => {}
                None => {
                    dirs.insert(low, orig);
                }
            }
        }
        if !e.is_dir {
            let low = e.path.to_lowercase();
            if let Some(prev) = files.insert(low, e.path.as_str()) {
                if prev == e.path {
                    return Err(unsafe_("DUPLICATE", e.path.clone()));
                }
                return Err(unsafe_("CASE_COLLISION", format!("{prev} vs {}", e.path)));
            }
        }
    }
    for low in files.keys() {
        if dirs.contains_key(low) {
            return Err(unsafe_("FILE_DIR_CONFLICT", low.clone()));
        }
    }
    Ok(())
}

/// Read and validate every entry header of the archive (no decompression).
pub fn scan_entries<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<Vec<EntryInfo>, NeedsReview> {
    if zip.len() > MAX_ENTRIES {
        return Err(unsafe_(
            "TOO_MANY_ENTRIES",
            format!("{} entries", zip.len()),
        ));
    }
    let mut out = Vec::with_capacity(zip.len());
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let f = zip
            .by_index_raw(i)
            .map_err(|e| unsafe_("UNREADABLE_ENTRY", format!("entry {i}: {e}")))?;
        let raw = f.name().to_string();
        if f.encrypted() {
            return Err(unsafe_("ENCRYPTED", raw));
        }
        if f.is_symlink() {
            return Err(unsafe_("SYMLINK", raw));
        }
        let is_dir = raw.ends_with('/') || raw.ends_with('\\');
        let path = normalize_name(&raw)?;
        let size = f.size();
        total = total.saturating_add(size);
        if total > MAX_UNCOMPRESSED {
            return Err(unsafe_(
                "TOO_BIG",
                format!("more than {MAX_UNCOMPRESSED} bytes uncompressed"),
            ));
        }
        out.push(EntryInfo {
            index: i,
            path,
            is_dir,
            size,
        });
    }
    check_paths(&out)?;
    Ok(out)
}

/// True when `path` is exactly `root` or lies under it (case-sensitive, `/` separated).
pub fn under_root(path: &str, root: &str) -> bool {
    path.len() > root.len() && path.starts_with(root) && path.as_bytes()[root.len()] == b'/'
}
