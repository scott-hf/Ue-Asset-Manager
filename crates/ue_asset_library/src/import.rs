//! Safe import of library zips into a project (or as a new project).

use crate::error::{ImportError, NeedsReview};
use crate::harden::{scan_entries, under_root, EntryInfo};
use crate::library::LibraryEntry;
use crate::sidecar::Kind;
use crate::util::{compact_utc, move_file, unique_token, unix_secs};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportPolicy {
    SkipExisting,
    OverwriteWithBackup,
}

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    /// Files now present in the project that this import wrote (including overwrites).
    pub written: Vec<PathBuf>,
    /// Existing files left untouched (`SkipExisting`).
    pub skipped: Vec<PathBuf>,
    /// Backup locations of overwritten files (under `Saved/EAM_Backups/<ts>/`).
    pub backed_up: Vec<PathBuf>,
    /// Existing project files that clashed with the import.
    pub conflicts: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

type Zip = ZipArchive<BufReader<File>>;

fn open_zip(entry: &LibraryEntry) -> Result<Zip, ImportError> {
    let f = File::open(&entry.zip_path)?;
    Ok(ZipArchive::new(BufReader::with_capacity(1 << 20, f))?)
}

/// Validate every entry again and check the sidecar contract.
fn validate(
    entry: &LibraryEntry,
    zip: &mut Zip,
    restrict_roots: bool,
) -> Result<Vec<EntryInfo>, ImportError> {
    let infos = scan_entries(zip).map_err(ImportError::Unsafe)?;
    let mut count = 0u64;
    let mut total = 0u64;
    for e in infos.iter().filter(|e| !e.is_dir) {
        if restrict_roots
            && !entry
                .sidecar
                .allowed_roots
                .iter()
                .any(|r| under_root(&e.path, r))
        {
            return Err(ImportError::OutsideAllowedRoots(e.path.clone()));
        }
        let f = zip.by_index_raw(e.index)?;
        match f.enclosed_name() {
            Some(p) if p.components().count() > 0 => {}
            _ => {
                return Err(ImportError::Unsafe(NeedsReview::unsafe_entry(
                    "SLIP",
                    e.path.clone(),
                )));
            }
        }
        count += 1;
        total = total.saturating_add(e.size);
    }
    if count != entry.sidecar.entry_count {
        return Err(ImportError::SidecarMismatch(format!(
            "{} entries in zip, sidecar says {}",
            count, entry.sidecar.entry_count
        )));
    }
    if total != entry.sidecar.total_uncompressed {
        return Err(ImportError::SidecarMismatch(format!(
            "{} bytes in zip, sidecar says {}",
            total, entry.sidecar.total_uncompressed
        )));
    }
    Ok(infos)
}

/// Removes a directory tree on drop (cleanup runs on success and on error).
struct TmpDir(PathBuf);
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn extract_all(zip: &mut Zip, infos: &[EntryInfo], dest: &Path) -> Result<(), ImportError> {
    for e in infos.iter().filter(|e| !e.is_dir) {
        let target = dest.join(&e.path);
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        let f = zip.by_index(e.index)?;
        let mut limited = f.take(e.size + 1);
        let mut out = BufWriter::new(File::create(&target)?);
        let n = std::io::copy(&mut limited, &mut out)?;
        out.flush()?;
        if n != e.size {
            return Err(ImportError::SizeMismatch(format!(
                "{}: expected {} bytes, got {}",
                e.path, e.size, n
            )));
        }
    }
    Ok(())
}

fn require_project(root: &Path) -> Result<(), ImportError> {
    let has = fs::read_dir(root)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.path()
                    .extension()
                    .map(|x| x.eq_ignore_ascii_case("uproject"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if has {
        Ok(())
    } else {
        Err(ImportError::NotAProject(root.to_path_buf()))
    }
}

/// Fail when a project path needs a directory where a file exists (or vice versa).
fn check_type_conflicts(root: &Path, rel: &str) -> Result<(), ImportError> {
    let comps: Vec<&str> = rel.split('/').collect();
    let mut p = root.to_path_buf();
    for (i, c) in comps.iter().enumerate() {
        p.push(c);
        if i + 1 < comps.len() {
            if p.is_file() {
                return Err(ImportError::PathConflict(format!(
                    "{} is a file, expected a folder",
                    p.display()
                )));
            }
        } else if p.is_dir() {
            return Err(ImportError::PathConflict(format!(
                "{} is a folder, expected a file",
                p.display()
            )));
        }
    }
    Ok(())
}

fn require_kind(entry: &LibraryEntry) -> Result<(), ImportError> {
    match entry.sidecar.kind {
        Kind::Content | Kind::Plugin => Ok(()),
        k => Err(ImportError::WrongKind(k)),
    }
}

/// List existing project files that an import would clash with. Nothing is written.
pub fn preview_import(
    entry: &LibraryEntry,
    project_root: &Path,
) -> Result<Vec<PathBuf>, ImportError> {
    require_kind(entry)?;
    require_project(project_root)?;
    let mut zip = open_zip(entry)?;
    let infos = validate(entry, &mut zip, true)?;
    let mut conflicts = Vec::new();
    for e in infos.iter().filter(|e| !e.is_dir) {
        check_type_conflicts(project_root, &e.path)?;
        let dest = project_root.join(&e.path);
        if dest.is_file() {
            conflicts.push(dest);
        }
    }
    Ok(conflicts)
}

/// Import a Content or Plugin library entry into an existing project.
pub fn safe_import(
    entry: &LibraryEntry,
    project_root: &Path,
    policy: ImportPolicy,
) -> Result<ImportReport, ImportError> {
    require_kind(entry)?;
    require_project(project_root)?;
    let mut zip = open_zip(entry)?;
    let infos = validate(entry, &mut zip, true)?;
    for e in infos.iter().filter(|e| !e.is_dir) {
        check_type_conflicts(project_root, &e.path)?;
    }

    let ts = format!("{}-{}", compact_utc(unix_secs()), unique_token());
    let tmp_root = project_root.join("Saved").join("EAM_Import_Tmp");
    let tmp = tmp_root.join(&ts);
    fs::create_dir_all(&tmp)?;
    let _guard = TmpDir(tmp.clone());
    extract_all(&mut zip, &infos, &tmp)?;

    let mut report = ImportReport::default();
    let backup_root = project_root.join("Saved").join("EAM_Backups").join(&ts);
    for e in infos.iter().filter(|e| !e.is_dir) {
        let dest = project_root.join(&e.path);
        let src = tmp.join(&e.path);
        if dest.is_file() {
            report.conflicts.push(dest.clone());
            match policy {
                ImportPolicy::SkipExisting => {
                    report.skipped.push(dest);
                    continue;
                }
                ImportPolicy::OverwriteWithBackup => {
                    let backup = backup_root.join(&e.path);
                    move_file(&dest, &backup)?;
                    report.backed_up.push(backup);
                }
            }
        }
        move_file(&src, &dest)?;
        report.written.push(dest);
    }

    if entry.sidecar.kind == Kind::Plugin
        && !project_root.join("Source").is_dir()
        && infos.iter().any(|e| {
            e.path
                .split('/')
                .nth(2)
                .map(|c| c.eq_ignore_ascii_case("Source"))
                .unwrap_or(false)
        })
    {
        report.warnings.push(
            "This plugin contains C++ Source but the project has no Source/ folder; \
             it must be built from source (add a C++ class / make it a code project) and the \
             plugin's Binaries were removed."
                .to_string(),
        );
    }
    if !entry.sidecar.external_refs.is_empty() {
        report.warnings.push(format!(
            "{} asset reference(s) point outside this pack (e.g. {}); copy those dependencies separately.",
            entry.sidecar.external_refs.len(),
            entry.sidecar.external_refs[0]
        ));
    }
    drop(_guard);
    let _ = fs::remove_dir(&tmp_root);
    Ok(report)
}

fn validate_project_name(name: &str) -> Result<(), ImportError> {
    let bad = |m: &str| Err(ImportError::InvalidName(format!("{name:?}: {m}")));
    if name.trim().is_empty() {
        return bad("empty");
    }
    if name != name.trim() || name.ends_with('.') {
        return bad("leading/trailing space or trailing dot");
    }
    if name.chars().count() > 100 {
        return bad("too long");
    }
    if name == "." || name == ".." {
        return bad("reserved");
    }
    if name.chars().any(|c| {
        (c as u32) < 0x20 || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return bad("contains invalid characters");
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return bad("reserved Windows name");
    }
    Ok(())
}

/// Extract a Project library entry into `<parent_dir>/<project_name>/`; returns that directory.
pub fn safe_import_new_project(
    entry: &LibraryEntry,
    parent_dir: &Path,
    project_name: &str,
) -> Result<PathBuf, ImportError> {
    if entry.sidecar.kind != Kind::Project {
        return Err(ImportError::WrongKind(entry.sidecar.kind));
    }
    validate_project_name(project_name)?;
    let target = parent_dir.join(project_name);
    if target.exists() {
        let empty_dir = target.is_dir()
            && fs::read_dir(&target)
                .map(|mut r| r.next().is_none())
                .unwrap_or(false);
        if !empty_dir {
            return Err(ImportError::TargetExists(target));
        }
    }
    let mut zip = open_zip(entry)?;
    let infos = validate(entry, &mut zip, false)?;
    fs::create_dir_all(parent_dir)?;
    let tmp = parent_dir.join(format!(".eam-new-{}", unique_token()));
    fs::create_dir_all(&tmp)?;
    let guard = TmpDir(tmp.clone());
    extract_all(&mut zip, &infos, &tmp)?;
    if target.exists() {
        fs::remove_dir(&target)?;
    }
    fs::rename(&tmp, &target)?;
    // Renamed away: the guard's remove is now a harmless no-op.
    drop(guard);
    Ok(target)
}
