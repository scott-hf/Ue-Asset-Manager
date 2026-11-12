//! Sanitize one dropped zip into the library (zip first, sidecar last as the commit marker).

use crate::error::{NeedsReview, SanitizeError};
use crate::layout::{plan_layout, Layout};
use crate::name::clean_name;
use crate::sidecar::{Kind, Sidecar, SCHEMA_VERSION};
use crate::util::{is_busy_error, rfc3339_utc, sha256_file, slugify, unique_token, unix_secs};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use zip::result::ZipError;
use zip::{ZipArchive, ZipWriter};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SanitizeOutcome {
    Sanitized {
        library_zip: PathBuf,
        sidecar_path: PathBuf,
        sidecar: Sidecar,
    },
    /// The same source zip (by SHA-256) is already in the library.
    Duplicate { existing: PathBuf },
}

fn map_open_error(e: ZipError) -> SanitizeError {
    match e {
        ZipError::Io(io) => {
            if is_busy_error(&io) {
                SanitizeError::Io(io.to_string())
            } else {
                SanitizeError::Truncated(io.to_string())
            }
        }
        ZipError::InvalidArchive(m) => {
            let l = m.to_lowercase();
            if l.contains("central directory") || l.contains("end of") || l.contains("eocd") {
                SanitizeError::Truncated(m.to_string())
            } else {
                SanitizeError::Corrupt(m.to_string())
            }
        }
        other => SanitizeError::Corrupt(other.to_string()),
    }
}

/// Library zip with the same source hash, if present together with its sidecar.
fn find_duplicate(library_dir: &Path, sha12: &str) -> Option<PathBuf> {
    let suffix = format!(".{sha12}.zip");
    for e in fs::read_dir(library_dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(".tmp-") || !name.ends_with(&suffix) {
            continue;
        }
        let sidecar = library_dir.join(format!("{name}.json"));
        if sidecar.exists() {
            return Some(e.path());
        }
    }
    None
}

fn write_zip(src: &Path, layout: &Layout, tmp: &Path) -> Result<(), SanitizeError> {
    let mut archive = ZipArchive::new(BufReader::with_capacity(1 << 20, File::open(src)?))
        .map_err(map_open_error)?;
    let out = BufWriter::with_capacity(1 << 20, File::create(tmp)?);
    let mut writer = ZipWriter::new(out);
    for r in &layout.renames {
        let f = archive
            .by_index_raw(r.index)
            .map_err(|e| SanitizeError::Corrupt(format!("{}: {e}", r.from)))?;
        writer
            .raw_copy_file_rename(f, &r.to)
            .map_err(|e| SanitizeError::Io(format!("{}: {e}", r.to)))?;
    }
    let mut out = writer
        .finish()
        .map_err(|e| SanitizeError::Io(e.to_string()))?;
    out.flush()?;
    let file = out
        .into_inner()
        .map_err(|e| SanitizeError::Io(e.to_string()))?;
    file.sync_all()?;
    drop(file);
    // Integrity check: the finished zip must reopen with the expected entry count.
    let check = ZipArchive::new(BufReader::new(File::open(tmp)?))
        .map_err(|e| SanitizeError::Io(format!("verify: {e}")))?;
    if check.len() != layout.renames.len() {
        return Err(SanitizeError::Io(format!(
            "verify: wrote {} entries, expected {}",
            check.len(),
            layout.renames.len()
        )));
    }
    Ok(())
}

/// Sanitize `src` into `library_dir`.
///
/// Temp files are created inside `library_dir` (same filesystem), the zip is renamed
/// into place first and the sidecar is renamed in LAST. The original is not moved.
pub fn sanitize_zip(src: &Path, library_dir: &Path) -> Result<SanitizeOutcome, SanitizeError> {
    fs::create_dir_all(library_dir)?;
    let file_name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed.zip".into());
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| file_name.clone());

    // Open first: a truncated zip is reported before any hashing work.
    let mut archive = ZipArchive::new(BufReader::with_capacity(1 << 20, File::open(src)?))
        .map_err(map_open_error)?;
    let sha = sha256_file(src)?;
    let sha12 = &sha[..12];
    if let Some(existing) = find_duplicate(library_dir, sha12) {
        return Ok(SanitizeOutcome::Duplicate { existing });
    }
    let layout = plan_layout(&mut archive).map_err(SanitizeError::NeedsReview)?;
    drop(archive);
    if layout.renames.is_empty() {
        return Err(SanitizeError::NeedsReview(NeedsReview::NoContent));
    }

    let (display_name, name_version) = clean_name(&stem);
    let slug = slugify(&display_name);
    let zip_name = format!("{slug}.{sha12}.zip");
    let zip_path = library_dir.join(&zip_name);
    let sidecar_path = library_dir.join(format!("{zip_name}.json"));

    let tmp_zip = library_dir.join(format!(".tmp-{}.zip", unique_token()));
    if let Err(e) = write_zip(src, &layout, &tmp_zip) {
        let _ = fs::remove_file(&tmp_zip);
        return Err(e);
    }

    let sidecar = Sidecar {
        schema: SCHEMA_VERSION,
        kind: layout.kind,
        display_name,
        source_zip: file_name,
        sha256: sha,
        engine_version_guess: layout.engine_version.clone().or(name_version),
        content_folder: layout.content_folder.clone(),
        plugin_name: layout.plugin_name.clone(),
        zip_prefix: layout.zip_prefix.clone(),
        game_prefix: layout.game_prefix.clone(),
        allowed_roots: layout.allowed_roots.clone(),
        entry_count: layout.entry_count,
        total_uncompressed: layout.total_uncompressed,
        external_refs: layout.external_refs.clone(),
        sanitized_at: rfc3339_utc(unix_secs()),
        marketplace_url: layout.marketplace_url.clone(),
        friendly_name: layout.friendly_name.clone(),
        extra: serde_json::Map::new(),
    };
    let json = serde_json::to_vec_pretty(&sidecar).map_err(|e| SanitizeError::Io(e.to_string()))?;

    // Commit: zip rename, then sidecar rename last.
    if zip_path.exists() {
        // An uncommitted leftover (zip without sidecar); find_duplicate proved no sidecar exists.
        fs::remove_file(&zip_path)?;
    }
    if let Err(e) = fs::rename(&tmp_zip, &zip_path) {
        let _ = fs::remove_file(&tmp_zip);
        return Err(e.into());
    }
    let tmp_json = library_dir.join(format!(".tmp-{}.json", unique_token()));
    let write_sidecar = || -> std::io::Result<()> {
        let mut f = File::create(&tmp_json)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        if sidecar_path.exists() {
            fs::remove_file(&sidecar_path)?;
        }
        fs::rename(&tmp_json, &sidecar_path)
    };
    if let Err(e) = write_sidecar() {
        let _ = fs::remove_file(&tmp_json);
        return Err(e.into());
    }
    Ok(SanitizeOutcome::Sanitized {
        library_zip: zip_path,
        sidecar_path,
        sidecar,
    })
}

/// Kind helper for callers printing outcomes.
pub fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Content => "content",
        Kind::Plugin => "plugin",
        Kind::Project => "project",
    }
}
