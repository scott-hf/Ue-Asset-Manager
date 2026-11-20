//! Plan and apply imports of catalog assets into an Unreal project.
//!
//! This is the GUI-friendly layer over `ue_asset_library::safe_import_subset`, shared by the MCP
//! server and any other front end (the GTK "Assets" page): [`plan_import`] is a dry run that never
//! writes; [`apply_plan`] runs a plan with the library's safe import (whole-zip validation,
//! byte-exact `.uasset`, backups, never `ZipArchive::extract`).
//!
//! Release choice follows the no-downgrade rule: per pack family the release that holds all the
//! selected assets and best fits the project's engine (exact, else the highest not above it, else
//! the lowest with a warning).

use crate::catalog::{split_asset_id, Catalog, SourceKind};
use crate::release::{format_version, parse_version, Version};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use ue_asset_library::{
    closure, preview_import_subset, safe_import_subset, AssetIndex, ImportPolicy, ImportReport,
    Kind, LibraryEntry, PackageInfo, Selection,
};

/// A project that may be written to.
#[derive(Clone, Debug)]
pub struct ProjectTarget {
    pub root: PathBuf,
    pub uproject: PathBuf,
    pub name: String,
    pub engine: Option<Version>,
    /// The raw `EngineAssociation` (a version, or a GUID for a custom engine).
    pub engine_raw: Option<String>,
}

fn find_uproject(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("uproject"))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

impl ProjectTarget {
    /// Validate a `.uproject` file or a project root and read its engine version.
    pub fn from_path(path: &Path) -> Result<ProjectTarget, String> {
        let (root, uproject) = if path.is_file() {
            let is_up = path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("uproject"));
            if !is_up {
                return Err(format!("{} is not a .uproject file", path.display()));
            }
            let root = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (root, path.to_path_buf())
        } else if path.is_dir() {
            let up = find_uproject(path)
                .ok_or_else(|| format!("no .uproject found in {}", path.display()))?;
            (path.to_path_buf(), up)
        } else {
            return Err(format!("{} does not exist", path.display()));
        };
        let name = uproject
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let engine_raw = std::fs::read_to_string(&uproject)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
            .and_then(|v| {
                v.get("EngineAssociation")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let engine = engine_raw.as_deref().and_then(parse_version);
        Ok(ProjectTarget {
            root,
            uproject,
            name,
            engine,
            engine_raw,
        })
    }

    /// A project with a known engine version and no `.uproject` read (the GUI already has it).
    pub fn new(root: PathBuf, uproject: PathBuf, name: &str, engine: Option<Version>) -> Self {
        ProjectTarget {
            root,
            uproject,
            name: name.to_string(),
            engine,
            engine_raw: engine.map(format_version),
        }
    }
}

/// Lexically normalised path text used to compare project paths (`/`, no `.`/`..`, lower case
/// on Windows).
pub fn norm_path(s: &str) -> String {
    let mut t = s.trim().replace('\\', "/");
    if let Some(r) = t.strip_prefix("//?/") {
        t = r.to_string();
    }
    let unc = t.starts_with("//");
    let abs = t.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in t.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut r = out.join("/");
    if unc {
        r = format!("//{r}");
    } else if abs {
        r = format!("/{r}");
    }
    if cfg!(windows) {
        r = r.to_lowercase();
    }
    r
}

/// What will be taken from one pack.
#[derive(Clone, Debug)]
pub struct PackPlan {
    /// Index into `Catalog::packs`.
    pub pack: usize,
    pub pack_id: String,
    pub pack_name: String,
    pub kind: Kind,
    /// Engine label of the chosen release (`5.3` or `5.0-5.8`).
    pub pack_engine: Option<String>,
    /// `exact`, `older`, `newer` (every release is newer than the project) or `unknown`.
    pub verdict: &'static str,
    /// Selected game paths (exact casing from the pack).
    pub selected: Vec<String>,
    /// Asset ids (`<packId>:<gamePath>`) added because the selection needs them.
    pub dependencies: Vec<String>,
    /// Project-relative files that will be written (packages plus companions).
    pub files: Vec<String>,
    pub size_bytes: u64,
    /// Project-relative files that already exist (skipped, or backed up with overwrite).
    pub existing_files: Vec<String>,
    /// Referenced packages that are not in the pack.
    pub external: Vec<String>,
    /// Packages whose header could not be analysed (their dependencies may be incomplete).
    pub unreadable: usize,
}

/// A dry-run result. Nothing has been written.
#[derive(Clone, Debug)]
pub struct ImportPlan {
    pub project: ProjectTarget,
    pub include_deps: bool,
    pub packs: Vec<PackPlan>,
    pub warnings: Vec<String>,
    /// Canonical (lower-case) asset ids the plan was made for.
    pub assets: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlanTotals {
    pub assets: usize,
    pub files: usize,
    pub size_bytes: u64,
    pub existing_files: usize,
    pub external: usize,
}

impl ImportPlan {
    pub fn totals(&self) -> PlanTotals {
        let mut t = PlanTotals::default();
        let mut ext: BTreeSet<&str> = BTreeSet::new();
        for p in &self.packs {
            t.assets += p.selected.len() + p.dependencies.len();
            t.files += p.files.len();
            t.size_bytes += p.size_bytes;
            t.existing_files += p.existing_files.len();
            ext.extend(p.external.iter().map(String::as_str));
        }
        t.external = ext.len();
        t
    }
}

/// Resolve asset ids (`<packId>:<gamePath>`; any release's pack id works) to catalog indexes.
/// Unknown ids and generated assets are errors that explain what to do.
pub fn resolve_assets(catalog: &Catalog, ids: &[String], max: usize) -> Result<Vec<usize>, String> {
    if ids.is_empty() {
        return Err("`asset_ids` must contain at least one id (see search_assets)".into());
    }
    if ids.len() > max {
        return Err(format!("at most {max} asset ids per plan"));
    }
    let mut out: Vec<usize> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for id in ids {
        match resolve_asset(catalog, id) {
            Ok(a) => {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors.join("; "))
    }
}

/// Resolve one asset id with a helpful error.
pub fn resolve_asset(catalog: &Catalog, id: &str) -> Result<usize, String> {
    let id = id.trim();
    if id.starts_with("gen:") {
        return catalog.find_asset(id).ok_or_else(|| {
            format!("no generated asset `{id}` (finished jobs only; try refresh_catalog)")
        });
    }
    if split_asset_id(id).is_none() {
        return Err(format!(
            "`{id}` is not an asset id; expected `<packId>:<gamePath>` as returned by search_assets"
        ));
    }
    catalog.find_asset(id).ok_or_else(|| {
        format!("no asset with id `{id}` (the pack or the asset is unknown); use search_assets to find ids")
    })
}

/// Canonical (lower-case) ids of resolved assets: the identity of a selection.
pub fn canonical_ids(catalog: &Catalog, assets: &[usize]) -> BTreeSet<String> {
    assets
        .iter()
        .map(|&a| catalog.assets[a].id.to_lowercase())
        .collect()
}

/// Group the assets by the pack release they will be taken from, with the engine verdict per pack.
#[allow(clippy::type_complexity)]
fn assign_packs(
    catalog: &Catalog,
    assets: &[usize],
    engine: Option<Version>,
    warnings: &mut Vec<String>,
) -> Result<(BTreeMap<usize, Vec<String>>, HashMap<usize, &'static str>), String> {
    let mut by_family: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &ai in assets {
        by_family
            .entry(catalog.assets[ai].family)
            .or_default()
            .push(ai);
    }
    let mut result: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut verdicts: HashMap<usize, &'static str> = HashMap::new();
    let path_in = |a: &crate::catalog::Asset, pack: usize| -> Option<String> {
        a.releases
            .iter()
            .find(|r| r.pack == pack)
            .and_then(|r| catalog.package(*r))
            .map(|p| p.game_path.clone())
    };
    for (fam, list) in by_family {
        // Releases that hold every selected asset of this family keep the pack consistent.
        let mut full: Vec<usize> = catalog.families[fam]
            .packs
            .iter()
            .copied()
            .filter(|&p| {
                list.iter()
                    .all(|&ai| catalog.assets[ai].releases.iter().any(|r| r.pack == p))
            })
            .collect();
        full.sort_unstable();
        if let Some((pack, verdict)) = catalog.choose_pack(&full, engine) {
            verdicts.insert(pack, verdict);
            for &ai in &list {
                if let Some(p) = path_in(&catalog.assets[ai], pack) {
                    result.entry(pack).or_default().push(p);
                }
            }
            continue;
        }
        warnings.push(format!(
            "assets of `{}` are not all in one release; they are taken from different releases",
            catalog.families[fam].name
        ));
        for &ai in &list {
            let a = &catalog.assets[ai];
            let mut cands: Vec<usize> = a.releases.iter().map(|r| r.pack).collect();
            cands.sort_unstable();
            let Some((pack, verdict)) = catalog.choose_pack(&cands, engine) else {
                return Err(format!("no release found for {}", a.id));
            };
            verdicts.entry(pack).or_insert(verdict);
            if let Some(p) = path_in(a, pack) {
                result.entry(pack).or_default().push(p);
            }
        }
    }
    Ok((result, verdicts))
}

fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Dry run: choose releases, add dependencies (`include_deps`), list files, size, clashes with
/// existing project files, the engine verdict and the references that are not in the pack.
/// Writes nothing.
pub fn plan_import(
    catalog: &Catalog,
    project: &ProjectTarget,
    assets: &[usize],
    include_deps: bool,
) -> Result<ImportPlan, String> {
    if let Some(&g) = assets
        .iter()
        .find(|&&a| catalog.assets[a].source == SourceKind::Generated)
    {
        let a = &catalog.assets[g];
        let gi = a.generated.as_ref();
        return Err(format!(
            "{} is a generated asset (finished pipeline job); it cannot be imported here. It lives at {} ({})",
            a.id,
            gi.and_then(|x| x.unreal_object_path.clone()).unwrap_or_default(),
            gi.and_then(|x| x.output_path.clone()).unwrap_or_default()
        ));
    }
    let mut warnings: Vec<String> = Vec::new();
    if project.engine.is_none() {
        warnings.push(format!(
            "the project's engine version is unknown (EngineAssociation {:?}); the newest release of each pack was chosen",
            project.engine_raw
        ));
    }
    let (by_pack, verdicts) = assign_packs(catalog, assets, project.engine, &mut warnings)?;

    let mut packs: Vec<PackPlan> = Vec::new();
    let mut all_external: BTreeSet<String> = BTreeSet::new();
    for (pi, paths) in &by_pack {
        let pack = &catalog.packs[*pi];
        let index = pack
            .index
            .as_ref()
            .ok_or_else(|| format!("pack {} has no index", pack.display_name))?;
        let mut selected = paths.clone();
        selected.sort();
        selected.dedup();
        let needed: Vec<String> = if include_deps {
            closure(index, &selected)
        } else {
            selected.clone()
        };
        let sel_lc: BTreeSet<String> = selected.iter().map(|s| s.to_ascii_lowercase()).collect();
        let by_gp: HashMap<String, &PackageInfo> = index
            .packages
            .iter()
            .map(|p| (p.game_path.to_ascii_lowercase(), p))
            .collect();
        let mut files: Vec<String> = Vec::new();
        let mut size = 0u64;
        let mut external: BTreeSet<String> = BTreeSet::new();
        let mut dependencies: Vec<String> = Vec::new();
        let mut unreadable = 0usize;
        let mut reasons: BTreeSet<String> = BTreeSet::new();
        for g in &needed {
            let Some(p) = by_gp.get(&g.to_ascii_lowercase()) else {
                continue;
            };
            files.push(p.zip_path.clone());
            files.extend(p.companions.iter().cloned());
            size += p.size;
            external.extend(p.external.iter().cloned());
            if p.header_unreadable {
                unreadable += 1;
                reasons.insert(
                    p.unreadable_reason
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                );
            }
            if !sel_lc.contains(&g.to_ascii_lowercase()) {
                dependencies.push(format!("{}:{}", pack.id, p.game_path));
            }
        }
        let sel = Selection {
            game_paths: selected.clone(),
            include_deps,
        };
        let existing = preview_import_subset(&pack.entry, &project.root, index, &sel)
            .map_err(|e| format!("{}: {e}", pack.display_name))?;
        let existing_files: Vec<String> = existing
            .iter()
            .map(|c| rel_path(&project.root, c))
            .collect();
        let verdict = verdicts.get(pi).copied().unwrap_or("unknown");
        let pack_engine = pack.engine_label();
        match (verdict, &pack_engine, project.engine) {
            ("newer", Some(pe), Some(pv)) => warnings.push(format!(
                "{}: pack is for UE {pe} but the project is on {}; assets saved by a newer engine usually cannot be opened",
                pack.display_name,
                format_version(pv)
            )),
            ("older", Some(pe), Some(pv)) => warnings.push(format!(
                "{}: pack is for UE {pe}, the project is on {}; Unreal upgrades the assets when they are opened and saved",
                pack.display_name,
                format_version(pv)
            )),
            _ => {}
        }
        if unreadable > 0 {
            warnings.push(format!(
                "{}: {unreadable} package(s) could not be analysed (reason: {}); their dependencies are unknown and may be missing",
                pack.display_name,
                reasons.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if !existing_files.is_empty() {
            warnings.push(format!(
                "{}: {} file(s) already exist in the project; they are skipped unless overwrite=true (then backed up)",
                pack.display_name,
                existing_files.len()
            ));
        }
        all_external.extend(external.iter().cloned());
        packs.push(PackPlan {
            pack: *pi,
            pack_id: pack.id.clone(),
            pack_name: pack.display_name.clone(),
            kind: pack.kind,
            pack_engine,
            verdict,
            selected,
            dependencies,
            files,
            size_bytes: size,
            existing_files,
            external: external.into_iter().collect(),
            unreadable,
        });
    }
    if !all_external.is_empty() {
        warnings.push(format!(
            "{} referenced package(s) are not in the pack(s) (e.g. {}); the imported assets may show missing references until they are added from elsewhere",
            all_external.len(),
            all_external.iter().next().cloned().unwrap_or_default()
        ));
    }
    Ok(ImportPlan {
        project: project.clone(),
        include_deps,
        packs,
        warnings,
        assets: canonical_ids(catalog, assets),
    })
}

/// What will be bundled from one pack (hosted server; everything the build needs is owned, so
/// the catalog lock is not held while the bundle is written).
#[derive(Clone, Debug)]
pub struct BundlePackPlan {
    pub pack_id: String,
    pub pack_name: String,
    /// Engine label of the chosen release (`5.3` or `5.0-5.8`).
    pub pack_engine: Option<String>,
    /// `exact`, `older`, `newer` (every release is newer than the project) or `unknown`.
    pub verdict: &'static str,
    pub entry: LibraryEntry,
    pub index: AssetIndex,
    pub selection: Selection,
    /// Uncompressed bytes of the selection plus its dependencies (from the asset index).
    pub estimated_bytes: u64,
}

/// A bundle request resolved against the catalog. Nothing has been written.
#[derive(Clone, Debug)]
pub struct BundlePlan {
    pub engine: Version,
    pub include_deps: bool,
    pub packs: Vec<BundlePackPlan>,
    pub warnings: Vec<String>,
    pub estimated_bytes: u64,
}

/// Like [`plan_import`] for a client project this server cannot see: only the engine version of
/// that project is known, so there is no clash preview. Release choice, dependency closure,
/// verdicts and warnings are the same.
pub fn plan_bundle(
    catalog: &Catalog,
    assets: &[usize],
    engine: Version,
    include_deps: bool,
) -> Result<BundlePlan, String> {
    if let Some(&g) = assets
        .iter()
        .find(|&&a| catalog.assets[a].source == SourceKind::Generated)
    {
        return Err(format!(
            "{} is a generated asset (finished pipeline job); it is not part of the pack library and cannot be bundled",
            catalog.assets[g].id
        ));
    }
    let mut warnings: Vec<String> = Vec::new();
    let (by_pack, verdicts) = assign_packs(catalog, assets, Some(engine), &mut warnings)?;
    let mut packs: Vec<BundlePackPlan> = Vec::new();
    for (pi, paths) in &by_pack {
        let pack = &catalog.packs[*pi];
        let index = pack
            .index
            .as_ref()
            .ok_or_else(|| format!("pack {} has no index", pack.display_name))?;
        let mut selected = paths.clone();
        selected.sort();
        selected.dedup();
        let needed: Vec<String> = if include_deps {
            closure(index, &selected)
        } else {
            selected.clone()
        };
        let by_gp: HashMap<String, &PackageInfo> = index
            .packages
            .iter()
            .map(|p| (p.game_path.to_ascii_lowercase(), p))
            .collect();
        let mut size = 0u64;
        let mut unreadable = 0usize;
        let mut reasons: BTreeSet<String> = BTreeSet::new();
        for g in &needed {
            let Some(p) = by_gp.get(&g.to_ascii_lowercase()) else {
                continue;
            };
            size += p.size;
            if p.header_unreadable {
                unreadable += 1;
                reasons.insert(
                    p.unreadable_reason
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                );
            }
        }
        let verdict = verdicts.get(pi).copied().unwrap_or("unknown");
        let pack_engine = pack.engine_label();
        match (verdict, &pack_engine) {
            ("newer", Some(pe)) => warnings.push(format!(
                "{}: pack is for UE {pe} but the project is on {}; assets saved by a newer engine usually cannot be opened",
                pack.display_name,
                format_version(engine)
            )),
            ("older", Some(pe)) => warnings.push(format!(
                "{}: pack is for UE {pe}, the project is on {}; Unreal upgrades the assets when they are opened and saved",
                pack.display_name,
                format_version(engine)
            )),
            _ => {}
        }
        if unreadable > 0 {
            warnings.push(format!(
                "{}: {unreadable} package(s) could not be analysed (reason: {}); their dependencies are unknown and may be missing",
                pack.display_name,
                reasons.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        packs.push(BundlePackPlan {
            pack_id: pack.id.clone(),
            pack_name: pack.display_name.clone(),
            pack_engine,
            verdict,
            entry: pack.entry.clone(),
            index: index.clone(),
            selection: Selection {
                game_paths: selected,
                include_deps,
            },
            estimated_bytes: size,
        });
    }
    let estimated_bytes = packs.iter().map(|p| p.estimated_bytes).sum();
    Ok(BundlePlan {
        engine,
        include_deps,
        packs,
        warnings,
        estimated_bytes,
    })
}

/// The result of applying one pack of a plan.
#[derive(Debug)]
pub struct PackOutcome {
    pub pack_id: String,
    pub pack_name: String,
    /// The library's report, or why this pack failed.
    pub result: Result<ImportReport, String>,
}

/// Apply a plan with `safe_import_subset`, one call per pack. Existing files are skipped unless
/// `overwrite` (then they are moved to `<project>/Saved/EAM_Backups/<timestamp>/`). Never writes
/// outside the project. Every pack is attempted even when an earlier one failed.
pub fn apply_plan(catalog: &Catalog, plan: &ImportPlan, overwrite: bool) -> Vec<PackOutcome> {
    let policy = if overwrite {
        ImportPolicy::OverwriteWithBackup
    } else {
        ImportPolicy::SkipExisting
    };
    plan.packs
        .iter()
        .map(|pp| {
            let result = (|| -> Result<ImportReport, String> {
                let pi = catalog
                    .pack_by_id(&pp.pack_id)
                    .ok_or("pack is no longer in the library (catalog was refreshed)")?;
                let pack = &catalog.packs[pi];
                let index = pack.index.as_ref().ok_or("pack has no index")?;
                let sel = Selection {
                    game_paths: pp.selected.clone(),
                    include_deps: plan.include_deps,
                };
                safe_import_subset(&pack.entry, &plan.project.root, policy, index, &sel)
                    .map_err(|e| e.to_string())
            })();
            PackOutcome {
                pack_id: pp.pack_id.clone(),
                pack_name: pp.pack_name.clone(),
                result,
            }
        })
        .collect()
}

/// Append an install-log record for a pack that was imported (see [`crate::install_log`]).
pub fn log_install(
    log_path: &Path,
    catalog: &Catalog,
    pack_id: &str,
    project: &ProjectTarget,
) -> Result<(), String> {
    let pi = catalog
        .pack_by_id(pack_id)
        .ok_or("pack is no longer in the library")?;
    let pack = &catalog.packs[pi];
    let rec = crate::install_log::InstallRecord {
        asset_id: pack.entry.sidecar.sha256.clone(),
        name: pack.display_name.clone(),
        source: "local".to_string(),
        engine_version: pack.engine_label().unwrap_or_default(),
        project_path: project.uproject.to_string_lossy().to_string(),
        project_name: project.name.clone(),
        installed_at: crate::install_log::now_rfc3339(),
    };
    crate::install_log::append(log_path, &rec).map_err(|e| e.to_string())
}

/// The project-relative form of a path written by an import (for display).
pub fn relative_to(project: &ProjectTarget, p: &Path) -> String {
    rel_path(&project.root, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_normalisation() {
        assert_eq!(norm_path("E:\\Proj\\.\\A\\..\\B/"), norm_path("E:/Proj/B"));
        assert_eq!(norm_path("  E:/x/ "), norm_path("E:\\x"));
        assert_ne!(norm_path("E:/p/../other"), norm_path("E:/p"));
        assert_eq!(norm_path("//srv/share/a"), norm_path("\\\\srv\\share\\a"));
    }

    #[test]
    fn project_from_path_needs_a_uproject() {
        let t = crate::testutil::Tmp::new();
        assert!(ProjectTarget::from_path(t.path()).is_err());
        assert!(ProjectTarget::from_path(&t.path().join("nope")).is_err());
        std::fs::write(
            t.path().join("Game.uproject"),
            r#"{"EngineAssociation":"5.4"}"#,
        )
        .unwrap();
        let p = ProjectTarget::from_path(t.path()).unwrap();
        assert_eq!((p.name.as_str(), p.engine), ("Game", Some((5, 4))));
        let q = ProjectTarget::from_path(&t.path().join("Game.uproject")).unwrap();
        assert_eq!(q.root, t.path());
    }
}
