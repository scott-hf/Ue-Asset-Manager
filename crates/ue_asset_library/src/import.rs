//! Safe import of library zips into a project (or as a new project).

use crate::asset_index::{closure, under_content, AssetIndex, PackageInfo};
use crate::error::{ImportError, NeedsReview};
use crate::harden::{check_paths, under_root, EntryInfo};
use crate::library::LibraryEntry;
use crate::sidecar::Kind;
use crate::util::{compact_utc, move_file, sha256_file, unique_token, unix_secs};
use crate::zipfix::{scan_with_repairs, Repairs};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

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

// ---------------------------------------------------------------------------------------------
// bundle export (hosted server): selected packages as one verified zip, nothing is imported
// ---------------------------------------------------------------------------------------------

/// `format` value of `manifest.json`.
pub const BUNDLE_FORMAT: u32 = 1;
/// Name of the manifest entry at the zip root.
pub const BUNDLE_MANIFEST: &str = "manifest.json";

/// One file of a bundle: project-relative path (`Content/...`, forward slashes), its size and the
/// SHA-256 over the uncompressed bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BundleFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// What one [`BundleWriter::add_subset`] call resolved.
#[derive(Clone, Debug, Default)]
pub struct SubsetExport {
    /// Files of the selection (including ones another pack already contributed identically).
    pub files: usize,
    /// Packages referenced by the selection that are NOT in the pack.
    pub external_missing: Vec<String>,
    /// In-pack dependencies of the selection that were not added (`include_deps == false`).
    pub unselected_deps: Vec<String>,
}

/// The finished bundle file.
#[derive(Clone, Debug)]
pub struct BundleOutcome {
    /// Sorted by path.
    pub files: Vec<BundleFile>,
    pub uncompressed_bytes: u64,
    /// Size of the zip file.
    pub size_bytes: u64,
    /// SHA-256 of the zip file.
    pub sha256: String,
}

#[derive(Debug)]
pub enum BundleError {
    Import(ImportError),
    /// Content paths that two packs fill with different bytes (or that differ only in case).
    Conflict(Vec<String>),
    /// The bundle would hold `size` uncompressed bytes; the cap is `cap`.
    CapExceeded {
        size: u64,
        cap: u64,
    },
    Io(String),
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BundleError::Import(e) => write!(f, "{e}"),
            BundleError::Conflict(p) => {
                write!(f, "packs disagree about: {}", p.join(", "))
            }
            BundleError::CapExceeded { size, cap } => {
                write!(f, "bundle of {size} bytes exceeds the cap of {cap} bytes")
            }
            BundleError::Io(s) => write!(f, "i/o error: {s}"),
        }
    }
}

impl std::error::Error for BundleError {}

impl From<ImportError> for BundleError {
    fn from(e: ImportError) -> Self {
        BundleError::Import(e)
    }
}

impl From<io::Error> for BundleError {
    fn from(e: io::Error) -> Self {
        BundleError::Io(e.to_string())
    }
}

impl From<zip::result::ZipError> for BundleError {
    fn from(e: zip::result::ZipError) -> Self {
        BundleError::Io(e.to_string())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copy up to `size` bytes (plus one, to notice an over-long entry) from `r` into `sink` and
/// return how many were read with their SHA-256.
fn pump(r: &mut dyn Read, size: u64, sink: &mut dyn Write) -> io::Result<(u64, String)> {
    let mut limited = r.take(size + 1);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0u64;
    loop {
        let k = match limited.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..k]);
        sink.write_all(&buf[..k])?;
        n += k as u64;
    }
    Ok((n, hex(&h.finalize())))
}

/// Stream one zip entry (through the repair view when its zip64 record is malformed) into `sink`.
fn stream_entry(
    zip: &mut Zip,
    repairs: &Repairs,
    e: &EntryInfo,
    sink: &mut dyn Write,
) -> Result<(u64, String), ImportError> {
    let (n, sha) = if repairs.has(&e.path) {
        repairs.with_entry(&e.path, |r| pump(r, e.size, sink))?
    } else {
        let mut f = zip.by_index(e.index)?;
        pump(&mut f, e.size, sink)?
    };
    if n != e.size {
        return Err(ImportError::SizeMismatch(format!(
            "{}: expected {} bytes, got {}",
            e.path, e.size, n
        )));
    }
    Ok((n, sha))
}

/// Writes a bundle zip: the selected packages, their companions and (with `include_deps`) in-pack
/// dependencies of one or more packs, every entry under `Content/<rel>`, plus `manifest.json`.
///
/// Bytes are copied exactly (the manifest hashes are over the uncompressed bytes). `.uexp` and
/// `.ubulk` are Stored, everything else Deflated. The same project path from two packs is written
/// once when the bytes are identical and is a conflict (reported by [`BundleWriter::finish`]) when
/// they differ. Nothing here touches the library: sources are only read.
pub struct BundleWriter {
    zip: ZipWriter<BufWriter<File>>,
    path: PathBuf,
    files: Vec<BundleFile>,
    by_lower: HashMap<String, usize>,
    total: u64,
    cap: u64,
    conflicts: BTreeSet<String>,
}

impl BundleWriter {
    /// Create a new zip at `path` (must not exist). `max_uncompressed` caps the sum of the file
    /// sizes.
    pub fn create(path: &Path, max_uncompressed: u64) -> Result<BundleWriter, BundleError> {
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(BundleWriter {
            zip: ZipWriter::new(BufWriter::with_capacity(1 << 20, f)),
            path: path.to_path_buf(),
            files: Vec::new(),
            by_lower: HashMap::new(),
            total: 0,
            cap: max_uncompressed,
            conflicts: BTreeSet::new(),
        })
    }

    /// Uncompressed bytes written so far.
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// Add the selection of one pack. The whole library zip is validated first (same checks as an
    /// import); only `Content/` entries are accepted.
    pub fn add_subset(
        &mut self,
        entry: &LibraryEntry,
        index: &AssetIndex,
        sel: &Selection,
    ) -> Result<SubsetExport, BundleError> {
        if sel.game_paths.is_empty() {
            return Err(ImportError::EmptySelection.into());
        }
        require_subset_kind(entry)?;
        let plan = plan_subset(index, sel)?;
        let mut zip = open_zip(entry)?;
        let (mut infos, repairs) = subset_infos(entry, &mut zip, &plan)?;
        if let Some(e) = infos.iter().find(|e| !under_content(&e.path)) {
            return Err(ImportError::OutsideAllowedRoots(e.path.clone()).into());
        }
        infos.sort_by(|a, b| a.path.cmp(&b.path));
        let fresh: u64 = infos
            .iter()
            .filter(|e| !self.by_lower.contains_key(&e.path.to_lowercase()))
            .map(|e| e.size)
            .sum();
        let would_be = self.total.saturating_add(fresh);
        if would_be > self.cap {
            return Err(BundleError::CapExceeded {
                size: would_be,
                cap: self.cap,
            });
        }
        for e in &infos {
            let lower = e.path.to_lowercase();
            if let Some(&i) = self.by_lower.get(&lower) {
                let (_, sha) = stream_entry(&mut zip, &repairs, e, &mut io::sink())?;
                let prev = &self.files[i];
                if prev.path != e.path {
                    self.conflicts
                        .insert(format!("{} vs {}", prev.path, e.path));
                } else if sha != prev.sha256 {
                    self.conflicts.insert(e.path.clone());
                }
                continue;
            }
            let lowered = e.path.to_ascii_lowercase();
            let stored = lowered.ends_with(".uexp") || lowered.ends_with(".ubulk");
            let opts = SimpleFileOptions::default()
                .compression_method(if stored {
                    CompressionMethod::Stored
                } else {
                    CompressionMethod::Deflated
                })
                .large_file(e.size >= u64::from(u32::MAX));
            self.zip.start_file(e.path.clone(), opts)?;
            let (n, sha) = stream_entry(&mut zip, &repairs, e, &mut self.zip)?;
            self.by_lower.insert(lower, self.files.len());
            self.files.push(BundleFile {
                path: e.path.clone(),
                size: n,
                sha256: sha,
            });
            self.total += n;
        }
        Ok(SubsetExport {
            files: infos.len(),
            external_missing: plan.external_missing,
            unselected_deps: plan.unselected_deps,
        })
    }

    /// Write `manifest.json` (the caller's fields plus `format`, `files` and
    /// `total_uncompressed_bytes`), close the zip and hash it. Fails with
    /// [`BundleError::Conflict`] when two packs disagreed about a path.
    pub fn finish(
        mut self,
        mut manifest: Map<String, Value>,
    ) -> Result<BundleOutcome, BundleError> {
        if !self.conflicts.is_empty() {
            return Err(BundleError::Conflict(
                std::mem::take(&mut self.conflicts).into_iter().collect(),
            ));
        }
        let rules: Vec<EntryInfo> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, f)| EntryInfo {
                index,
                path: f.path.clone(),
                is_dir: false,
                size: f.size,
            })
            .collect();
        check_paths(&rules).map_err(|e| BundleError::Import(ImportError::Unsafe(e)))?;
        let mut files = std::mem::take(&mut self.files);
        files.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.insert("format".into(), Value::from(BUNDLE_FORMAT));
        manifest.insert(
            "files".into(),
            serde_json::to_value(&files).map_err(|e| BundleError::Io(e.to_string()))?,
        );
        manifest.insert("total_uncompressed_bytes".into(), Value::from(self.total));
        let bytes = serde_json::to_vec(&Value::Object(manifest))
            .map_err(|e| BundleError::Io(e.to_string()))?;
        self.zip.start_file(
            BUNDLE_MANIFEST,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        self.zip.write_all(&bytes)?;
        let mut out = self.zip.finish()?;
        out.flush()?;
        let file = out
            .into_inner()
            .map_err(|e| BundleError::Io(e.to_string()))?;
        file.sync_all()?;
        drop(file);
        let size_bytes = fs::metadata(&self.path)?.len();
        let sha256 = sha256_file(&self.path)?;
        Ok(BundleOutcome {
            files,
            uncompressed_bytes: self.total,
            size_bytes,
            sha256,
        })
    }
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
