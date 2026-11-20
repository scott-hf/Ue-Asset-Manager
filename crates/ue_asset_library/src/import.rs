//! Safe import of library zips into a project (or as a new project).

use crate::asset_index::{closure, under_content, AssetIndex, PackageInfo};
use crate::error::{ImportError, NeedsReview};
use crate::harden::{under_root, EntryInfo};
use crate::library::LibraryEntry;
use crate::sidecar::Kind;
use crate::util::{compact_utc, move_file, unique_token, unix_secs};
use crate::zipfix::{scan_with_repairs, Repairs};
use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// Selective import: packages referenced by the imported ones that are NOT in this pack.
    pub external_missing: Vec<String>,
}

/// Which packages of a pack to import (selective import).
#[derive(Clone, Debug)]
pub struct Selection {
    /// `PackageInfo::game_path` values.
    pub game_paths: Vec<String>,
    /// Apply [`crate::asset_index::closure`] on top of `game_paths`.
    pub include_deps: bool,
}

type Zip = ZipArchive<BufReader<File>>;

pub(crate) fn open_zip(entry: &LibraryEntry) -> Result<Zip, ImportError> {
    let f = File::open(&entry.zip_path)?;
    Ok(ZipArchive::new(BufReader::with_capacity(1 << 20, f))?)
}

/// Validate every entry again and check the sidecar contract.
fn validate(
    entry: &LibraryEntry,
    zip: &mut Zip,
    restrict_roots: bool,
) -> Result<(Vec<EntryInfo>, Repairs), ImportError> {
    let (infos, repairs) = scan_with_repairs(zip, &entry.zip_path).map_err(ImportError::Unsafe)?;
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
    Ok((infos, repairs))
}

/// Removes a directory tree on drop (cleanup runs on success and on error).
struct TmpDir(PathBuf);
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_limited(r: &mut dyn Read, target: &Path, size: u64) -> std::io::Result<u64> {
    let mut limited = r.take(size + 1);
    let mut out = BufWriter::new(File::create(target)?);
    let n = std::io::copy(&mut limited, &mut out)?;
    out.flush()?;
    Ok(n)
}

fn extract_all(
    zip: &mut Zip,
    infos: &[EntryInfo],
    dest: &Path,
    repairs: &Repairs,
) -> Result<(), ImportError> {
    for e in infos.iter().filter(|e| !e.is_dir) {
        let target = dest.join(&e.path);
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        // Entries with a malformed zip64 record (older sanitizer builds) go through the repair view.
        let n = if repairs.has(&e.path) {
            repairs.with_entry(&e.path, |r| copy_limited(r, &target, e.size))?
        } else {
            let mut f = zip.by_index(e.index)?;
            copy_limited(&mut f, &target, e.size)?
        };
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

/// Existing project files the given entries would clash with. Nothing is written.
fn preview_files(project_root: &Path, infos: &[EntryInfo]) -> Result<Vec<PathBuf>, ImportError> {
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

/// Extract `infos` to a temp folder inside the project, then apply them with the policy.
/// `full_pack` adds the whole-pack warnings (plugin Source, sidecar external refs).
fn apply_files(
    entry: &LibraryEntry,
    project_root: &Path,
    zip: &mut Zip,
    infos: &[EntryInfo],
    repairs: &Repairs,
    policy: ImportPolicy,
    full_pack: bool,
) -> Result<ImportReport, ImportError> {
    for e in infos.iter().filter(|e| !e.is_dir) {
        check_type_conflicts(project_root, &e.path)?;
    }

    let ts = format!("{}-{}", compact_utc(unix_secs()), unique_token());
    let tmp_root = project_root.join("Saved").join("EAM_Import_Tmp");
    let tmp = tmp_root.join(&ts);
    fs::create_dir_all(&tmp)?;
    let _guard = TmpDir(tmp.clone());
    extract_all(zip, infos, &tmp, repairs)?;

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

    if full_pack {
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
    }
    drop(_guard);
    let _ = fs::remove_dir(&tmp_root);
    Ok(report)
}

/// List existing project files that an import would clash with. Nothing is written.
pub fn preview_import(
    entry: &LibraryEntry,
    project_root: &Path,
) -> Result<Vec<PathBuf>, ImportError> {
    require_kind(entry)?;
    require_project(project_root)?;
    let mut zip = open_zip(entry)?;
    let (infos, _) = validate(entry, &mut zip, true)?;
    preview_files(project_root, &infos)
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
    let (infos, repairs) = validate(entry, &mut zip, true)?;
    apply_files(
        entry,
        project_root,
        &mut zip,
        &infos,
        &repairs,
        policy,
        true,
    )
}

/// Selective import accepts content packs and project packs (the latter only into `Content/`).
fn require_subset_kind(entry: &LibraryEntry) -> Result<(), ImportError> {
    match entry.sidecar.kind {
        Kind::Content | Kind::Project => Ok(()),
        k => Err(ImportError::WrongKind(k)),
    }
}

/// What a selection resolves to: the zip entries to extract and the missing external packages.
struct SubsetPlan {
    zip_paths: HashSet<String>,
    external_missing: Vec<String>,
    /// In-pack dependencies of the selection that are NOT part of it (`include_deps == false`).
    unselected_deps: Vec<String>,
}

fn plan_subset(index: &AssetIndex, sel: &Selection) -> Result<SubsetPlan, ImportError> {
    if sel.game_paths.is_empty() {
        return Err(ImportError::EmptySelection);
    }
    let map: HashMap<String, &PackageInfo> = index
        .packages
        .iter()
        .map(|p| (p.game_path.to_ascii_lowercase(), p))
        .collect();
    let mut picked: Vec<String> = Vec::new();
    for g in &sel.game_paths {
        match map.get(&g.to_ascii_lowercase()) {
            Some(p) => picked.push(p.game_path.clone()),
            None => return Err(ImportError::UnknownPackage(g.clone())),
        }
    }
    let needed: Vec<String> = if sel.include_deps {
        closure(index, &picked)
    } else {
        picked.sort();
        picked.dedup();
        picked
    };
    let needed_set: HashSet<&str> = needed.iter().map(String::as_str).collect();
    let mut zip_paths = HashSet::new();
    let mut external: BTreeSet<String> = BTreeSet::new();
    let mut unselected: BTreeSet<String> = BTreeSet::new();
    for g in &needed {
        let Some(p) = map.get(&g.to_ascii_lowercase()) else {
            continue;
        };
        zip_paths.insert(p.zip_path.clone());
        zip_paths.extend(p.companions.iter().cloned());
        external.extend(p.external.iter().cloned());
        for d in &p.deps {
            if !needed_set.contains(d.as_str()) {
                unselected.insert(d.clone());
            }
        }
    }
    Ok(SubsetPlan {
        zip_paths,
        external_missing: external.into_iter().collect(),
        unselected_deps: unselected.into_iter().collect(),
    })
}

/// Whole-zip validation, then only the selected entries. Project packs: `Content/` only.
fn subset_infos(
    entry: &LibraryEntry,
    zip: &mut Zip,
    plan: &SubsetPlan,
) -> Result<(Vec<EntryInfo>, Repairs), ImportError> {
    let (infos, repairs) = validate(entry, zip, entry.sidecar.kind == Kind::Content)?;
    let chosen: Vec<EntryInfo> = infos
        .into_iter()
        .filter(|e| !e.is_dir && plan.zip_paths.contains(&e.path))
        .collect();
    if chosen.len() != plan.zip_paths.len() {
        let have: HashSet<&str> = chosen.iter().map(|e| e.path.as_str()).collect();
        let missing = plan
            .zip_paths
            .iter()
            .find(|p| !have.contains(p.as_str()))
            .cloned()
            .unwrap_or_default();
        return Err(ImportError::SidecarMismatch(format!(
            "asset index lists {missing}, which is not in the zip (stale index)"
        )));
    }
    if entry.sidecar.kind == Kind::Project {
        if let Some(e) = chosen.iter().find(|e| !under_content(&e.path)) {
            return Err(ImportError::OutsideAllowedRoots(e.path.clone()));
        }
    }
    Ok((chosen, repairs))
}

/// Like [`preview_import`] for a selection of packages. Nothing is written.
pub fn preview_import_subset(
    entry: &LibraryEntry,
    project_root: &Path,
    index: &AssetIndex,
    sel: &Selection,
) -> Result<Vec<PathBuf>, ImportError> {
    if sel.game_paths.is_empty() {
        return Err(ImportError::EmptySelection);
    }
    require_subset_kind(entry)?;
    require_project(project_root)?;
    let plan = plan_subset(index, sel)?;
    let mut zip = open_zip(entry)?;
    let (infos, _) = subset_infos(entry, &mut zip, &plan)?;
    preview_files(project_root, &infos)
}

/// Import only the selected packages (plus their companions and, with `include_deps`, their
/// in-pack dependency closure). The whole zip is still validated; nothing else is extracted.
pub fn safe_import_subset(
    entry: &LibraryEntry,
    project_root: &Path,
    policy: ImportPolicy,
    index: &AssetIndex,
    sel: &Selection,
) -> Result<ImportReport, ImportError> {
    if sel.game_paths.is_empty() {
        return Err(ImportError::EmptySelection);
    }
    require_subset_kind(entry)?;
    require_project(project_root)?;
    let plan = plan_subset(index, sel)?;
    let mut zip = open_zip(entry)?;
    let (infos, repairs) = subset_infos(entry, &mut zip, &plan)?;
    let mut report = apply_files(
        entry,
        project_root,
        &mut zip,
        &infos,
        &repairs,
        policy,
        false,
    )?;
    if !plan.external_missing.is_empty() {
        report.warnings.push(format!(
            "{} package(s) referenced by the imported assets are not in this pack (e.g. {}); copy those dependencies separately.",
            plan.external_missing.len(),
            plan.external_missing[0]
        ));
    }
    if !plan.unselected_deps.is_empty() {
        report.warnings.push(format!(
            "{} in-pack dependenc{} not imported (e.g. {}).",
            plan.unselected_deps.len(),
            if plan.unselected_deps.len() == 1 {
                "y was"
            } else {
                "ies were"
            },
            plan.unselected_deps[0]
        ));
    }
    report.external_missing = plan.external_missing;
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
    let (infos, repairs) = validate(entry, &mut zip, false)?;
    fs::create_dir_all(parent_dir)?;
    let tmp = parent_dir.join(format!(".eam-new-{}", unique_token()));
    fs::create_dir_all(&tmp)?;
    let guard = TmpDir(tmp.clone());
    extract_all(&mut zip, &infos, &tmp, &repairs)?;
    if target.exists() {
        fs::remove_dir(&target)?;
    }
    fs::rename(&tmp, &target)?;
    // Renamed away: the guard's remove is now a harmless no-op.
    drop(guard);
    Ok(target)
}
