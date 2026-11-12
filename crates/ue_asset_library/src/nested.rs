//! Nested archives: a zip/rar/7z whose top level holds no assets but one or more
//! archives (typically one per engine version). One inner archive is chosen by the
//! `prefer_engine_versions` list, extracted to a temp file and run through the normal
//! pipeline; the identity stays that of the outer archive.

use crate::archive::{convert_and_sanitize, kind_of_name, source_stem, ArchiveKind};
use crate::error::{NeedsReview, SanitizeError};
use crate::harden::{EntryInfo, MAX_UNCOMPRESSED};
use crate::name::{clean_name_ranged, parse_engine_version};
use crate::sanitize::{SanitizeOptions, SanitizeOutcome, SourceOverride};
use crate::util::unique_token;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::path::Path;
use zip::ZipArchive;

/// Maximum nesting: an archive inside an archive inside an archive.
pub const MAX_NEST_DEPTH: u8 = 2;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub index: usize,
    pub path: String,
    pub kind: ArchiveKind,
    /// Cleaned base name (case-insensitive identity of "the same pack").
    pub base: String,
    pub version: Option<String>,
}

fn ext_of(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => file[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn is_junk(path: &str) -> bool {
    path.split('/').any(|c| c.eq_ignore_ascii_case("__MACOSX"))
        || path.rsplit('/').next().is_some_and(|f| f.starts_with("._"))
}

/// Inner archives of a zip that has no content of its own (no `.uasset`/`.umap`/`.uproject`/
/// `.uplugin` entry anywhere). `None` when the zip is not a nested-archive wrapper.
pub fn detect(entries: &[EntryInfo]) -> Option<Vec<Candidate>> {
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| !e.is_dir && !is_junk(&e.path)) {
        if matches!(
            ext_of(&e.path).as_str(),
            "uasset" | "umap" | "uproject" | "uplugin"
        ) {
            return None;
        }
        let file = e.path.rsplit('/').next().unwrap_or(&e.path);
        let Some(kind) = kind_of_name(file) else {
            continue;
        };
        if !matches!(
            kind,
            ArchiveKind::Zip | ArchiveKind::SevenZ | ArchiveKind::Rar
        ) {
            continue;
        }
        let cn = clean_name_ranged(&source_stem(file));
        out.push(Candidate {
            index: e.index,
            path: e.path.clone(),
            kind,
            base: cn.display.to_lowercase(),
            version: cn.version,
        });
    }
    (!out.is_empty()).then_some(out)
}

fn ver_key(v: &str) -> (u32, u32) {
    let mut it = v.split('.');
    (
        it.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        it.next().and_then(|x| x.parse().ok()).unwrap_or(0),
    )
}

/// Pick one candidate: the first `prefer` version present, else the highest version.
/// Err(detail) when the choice is ambiguous.
pub fn choose<'a>(cands: &'a [Candidate], prefer: &[String]) -> Result<&'a Candidate, String> {
    let bases: BTreeSet<&str> = cands.iter().map(|c| c.base.as_str()).collect();
    if bases.len() > 1 {
        return Err(format!(
            "inner archives are different things: {}",
            cands
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let versioned: Vec<&Candidate> = cands.iter().filter(|c| c.version.is_some()).collect();
    if versioned.is_empty() {
        return if cands.len() == 1 {
            Ok(&cands[0])
        } else {
            Err(format!(
                "several inner archives without a version: {}",
                cands
                    .iter()
                    .map(|c| c.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
    }
    for p in prefer {
        let want = parse_engine_version(p).unwrap_or_else(|| p.trim().to_string());
        if let Some(c) = versioned
            .iter()
            .find(|c| c.version.as_deref() == Some(want.as_str()))
        {
            return Ok(c);
        }
    }
    Ok(versioned
        .into_iter()
        .max_by_key(|c| ver_key(c.version.as_deref().unwrap_or("0.0")))
        .expect("non-empty"))
}

/// Extract the chosen inner archive (size-capped) and sanitize it under the outer identity.
pub(crate) fn run<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    cands: &[Candidate],
    library_dir: &Path,
    base: &SourceOverride,
    opts: &SanitizeOptions,
) -> Result<SanitizeOutcome, SanitizeError> {
    if opts.depth >= MAX_NEST_DEPTH {
        return Err(SanitizeError::NeedsReview(NeedsReview::unsafe_entry(
            "NESTED_TOO_DEEP",
            format!("more than {MAX_NEST_DEPTH} levels of nested archives"),
        )));
    }
    let chosen = choose(cands, &opts.prefer_engine_versions).map_err(|d| {
        SanitizeError::NeedsReview(NeedsReview::unsafe_entry("NESTED_AMBIGUOUS", d))
    })?;
    let skipped: Vec<String> = cands
        .iter()
        .filter(|c| c.index != chosen.index)
        .map(|c| c.path.clone())
        .collect();

    let ext = match chosen.kind {
        ArchiveKind::SevenZ => "7z",
        ArchiveKind::Rar => "rar",
        _ => "zip",
    };
    let tmp = library_dir.join(format!(".tmp-{}.nested.{ext}", unique_token()));
    let extracted = (|| -> Result<(), SanitizeError> {
        let entry = archive
            .by_index(chosen.index)
            .map_err(|e| SanitizeError::Io(format!("{}: {e}", chosen.path)))?;
        if entry.size() > MAX_UNCOMPRESSED {
            return Err(SanitizeError::NeedsReview(NeedsReview::unsafe_entry(
                "TOO_BIG",
                chosen.path.clone(),
            )));
        }
        let mut out = File::create(&tmp)?;
        let n = io::copy(&mut entry.take(MAX_UNCOMPRESSED + 1), &mut out)?;
        if n > MAX_UNCOMPRESSED {
            return Err(SanitizeError::NeedsReview(NeedsReview::unsafe_entry(
                "TOO_BIG",
                chosen.path.clone(),
            )));
        }
        out.sync_all()?;
        Ok(())
    })();
    if let Err(e) = extracted {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    let mut sov = base.clone();
    sov.engine_version = chosen.version.clone();
    sov.extra
        .insert("nested_chosen".into(), chosen.path.clone().into());
    sov.extra
        .insert("nested_skipped".into(), serde_json::json!(skipped));
    let result = convert_and_sanitize(
        chosen.kind,
        std::slice::from_ref(&tmp),
        library_dir,
        &sov,
        &opts.deeper(),
    );
    let _ = fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(i: usize, path: &str) -> Candidate {
        let file = path.rsplit('/').next().unwrap();
        let cn = clean_name_ranged(&source_stem(file));
        Candidate {
            index: i,
            path: path.into(),
            kind: ArchiveKind::Zip,
            base: cn.display.to_lowercase(),
            version: cn.version,
        }
    }

    #[test]
    fn choosing() {
        let c = vec![
            cand(0, "Enhanced Timer Manager 5.7.zip"),
            cand(1, "Enhanced Timer Manager 5.8.zip"),
        ];
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(choose(&c, &p(&["5.8"])).unwrap().index, 1);
        assert_eq!(choose(&c, &p(&["5.7", "5.8"])).unwrap().index, 0);
        // None of the preferred versions present: highest wins.
        assert_eq!(choose(&c, &p(&["5.3"])).unwrap().index, 1);
        assert_eq!(choose(&c, &[]).unwrap().index, 1);
        let mixed = vec![cand(0, "A Pack 5.7.zip"), cand(1, "B Other 5.8.zip")];
        assert!(choose(&mixed, &p(&["5.8"])).is_err());
        let two_unversioned = vec![cand(0, "A.zip"), cand(1, "A.rar")];
        assert!(choose(&two_unversioned, &[]).is_err());
        assert_eq!(choose(&two_unversioned[..1], &[]).unwrap().index, 0);
    }
}
