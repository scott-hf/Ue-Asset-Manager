//! Per-pack asset index (content and project packs): the packages inside a library zip with class, size, editor thumbnail flag
//! and dependency links, stored as a derived overlay `<zip>.assets.json` next to the sidecar.
//!
//! The overlay is never authoritative: it can always be rebuilt from the zip. It records the
//! sidecar's `sha256` and `entry_count`; a mismatch means regenerate. Writers: the sanitizer after
//! the sidecar commit, and the app on demand (best effort; a read-only share just means the index
//! lives in memory only).

use crate::error::ImportError;
use crate::harden::{normalize_name, EntryInfo};
use crate::import::open_zip;
use crate::library::LibraryEntry;
use crate::sidecar::Kind;
use crate::uasset;
use crate::util::unique_token;
use crate::zipfix::{scan_with_repairs, Repairs};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use zip::ZipArchive;

pub const INDEX_SCHEMA: u32 = 1;

/// Package headers bigger than this are not read (listed as `header_unreadable`).
const MAX_HEADER: u64 = 16 * 1024 * 1024;
/// First read of a package: enough for the summary, then the rest of the header is read.
const FIRST_READ: u64 = 64 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AssetIndex {
    pub schema: u32,
    pub sidecar_sha256: String,
    pub entry_count: u64,
    /// Sorted by `game_path`.
    pub packages: Vec<PackageInfo>,
    /// Non-package files, zip-relative path + size.
    pub other_files: Vec<OtherFile>,
    /// Lowest / highest `major.minor` engine version the package headers were saved with
    /// (`None` when no header records one). Old overlays without the fields still load.
    #[serde(default)]
    pub engine_version_min: Option<String>,
    #[serde(default)]
    pub engine_version_max: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PackageInfo {
    /// `/Game/Grass/Meshes/SM_Grass_03`.
    pub game_path: String,
    /// Main file entry, e.g. `Content/Grass/Meshes/SM_Grass_03.uasset`.
    pub zip_path: String,
    /// Zip paths of `.uexp` / `.ubulk` / `.uptnl` with the same stem.
    pub companions: Vec<String>,
    /// Main export class short name (`StaticMesh`, `Material`, ...).
    pub class: Option<String>,
    /// Uncompressed bytes of `zip_path` plus companions.
    pub size: u64,
    /// `game_path`s of packages in this pack that this one needs.
    pub deps: Vec<String>,
    /// Referenced packages that are NOT in this pack.
    pub external: Vec<String>,
    pub has_thumbnail: bool,
    pub header_unreadable: bool,
    /// Short code when `header_unreadable`: `zip_entry` (the zip entry itself cannot be read,
    /// e.g. a bad CRC or damaged zip64 data), `header_too_large`, `unversioned`, `bad_magic`,
    /// `bad_header`. Old overlays without the field still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OtherFile {
    pub path: String,
    pub size: u64,
}

fn ext_lower(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => file[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Path without its last extension.
fn strip_ext(path: &str) -> &str {
    let file_start = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    match path[file_start..].rfind('.') {
        Some(i) if i > 0 => &path[..file_start + i],
        _ => path,
    }
}

/// `Content/A/B.uasset` -> `/Game/A/B`. Content packs and project packs (sanitized project zips
/// have the project root at the zip root) map the same way; anything outside `Content/` is `None`.
/// Plugin packs are not indexed.
fn game_path_of(kind: Kind, zip_path: &str) -> Option<String> {
    if !matches!(kind, Kind::Content | Kind::Project) {
        return None;
    }
    let no_ext = strip_ext(zip_path);
    let comps: Vec<&str> = no_ext.split('/').collect();
    if comps.len() >= 2 && comps[0].eq_ignore_ascii_case("Content") {
        Some(format!("/Game/{}", comps[1..].join("/")))
    } else {
        None
    }
}

/// True when the zip path lies under `Content/` (case-insensitive first component).
pub(crate) fn under_content(zip_path: &str) -> bool {
    let mut it = zip_path.split('/');
    it.next().is_some_and(|c| c.eq_ignore_ascii_case("Content")) && it.next().is_some()
}

fn mount_of(game_path: &str) -> &str {
    game_path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
}

/// `Anim_X_10` -> `Anim_X` (assets renamed after cooking keep the original package name).
fn strip_numeric_suffix(stem: &str) -> Option<&str> {
    let i = stem.rfind('_')?;
    let tail = &stem[i + 1..];
    (i > 0 && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())).then(|| &stem[..i])
}

/// Why a package header could not be read: a short code plus detail text.
struct Unreadable {
    code: &'static str,
    detail: String,
}

fn parse_code(e: &uasset::ParseError) -> &'static str {
    if e.0.contains("unversioned") {
        "unversioned"
    } else if e.0.contains("not an Unreal package") {
        "bad_magic"
    } else {
        "bad_header"
    }
}

/// Read and parse the header of one zip entry; the error says why it is unreadable.
fn read_package_err<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    idx: usize,
    path: &str,
    repairs: &Repairs,
) -> Result<uasset::Package, Unreadable> {
    if repairs.has(path) {
        let r = repairs.with_entry(path, |r| Ok(parse_reader(r)));
        return r.map_err(|e| Unreadable {
            code: "zip_entry",
            detail: e.to_string(),
        })?;
    }
    let mut f = zip.by_index(idx).map_err(|e| Unreadable {
        code: "zip_entry",
        detail: e.to_string(),
    })?;
    parse_reader(&mut f)
}

/// Read the first `TotalHeaderSize` bytes (at most [`MAX_HEADER`]) from a package stream and parse.
fn parse_reader(f: &mut dyn Read) -> Result<uasset::Package, Unreadable> {
    let zip_err = |e: String| Unreadable {
        code: "zip_entry",
        detail: e,
    };
    let mut buf = Vec::new();
    (&mut *f)
        .take(FIRST_READ)
        .read_to_end(&mut buf)
        .map_err(|e| zip_err(e.to_string()))?;
    let bad = |e: uasset::ParseError, buf: &[u8]| Unreadable {
        code: parse_code(&e),
        detail: format!("{e} [head {}]", hex_head(buf)),
    };
    let total = uasset::header_size(&buf).map_err(|e| bad(e, &buf))? as u64;
    if total > MAX_HEADER {
        return Err(Unreadable {
            code: "header_too_large",
            detail: format!("header too large: {total} bytes"),
        });
    }
    if total > buf.len() as u64 {
        let more = total - buf.len() as u64;
        (&mut *f)
            .take(more)
            .read_to_end(&mut buf)
            .map_err(|e| zip_err(e.to_string()))?;
    }
    uasset::parse(&buf).map_err(|e| bad(e, &buf))
}

fn hex_head(b: &[u8]) -> String {
    b.iter()
        .take(32)
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

/// `5.0.3` -> (5, 0); `None` for an empty or odd string.
fn major_minor(engine: &str) -> Option<(u32, u32)> {
    let mut it = engine.split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next()?.parse().ok()?;
    (major > 0).then_some((major, minor))
}

/// Diagnostics: why is the package at `zip_path` unreadable (`Ok(summary)` when it parses).
#[doc(hidden)]
pub fn probe_package(entry: &LibraryEntry, zip_path: &str) -> Result<String, String> {
    let mut zip = open_zip(entry).map_err(|e| e.to_string())?;
    let idx = (0..zip.len())
        .find(|&i| {
            zip.by_index_raw(i)
                .map(|f| f.name() == zip_path)
                .unwrap_or(false)
        })
        .ok_or("not in zip")?;
    let repairs = Repairs::load(&entry.zip_path);
    let p = read_package_err(&mut zip, idx, zip_path, &repairs)
        .map_err(|u| format!("{}: {}", u.code, u.detail))?;
    Ok(format!(
        "ue4 {} ue5 {} flags {:#x} engine {}",
        p.ue4, p.ue5, p.flags, p.engine
    ))
}

#[derive(Default)]
struct Group {
    main: Option<usize>,
    companions: Vec<usize>,
}

/// Package references of one header: (imports + soft refs, name-table paths).
struct Refs {
    hard: Vec<String>,
    from_names: Vec<String>,
}

struct Pending {
    info: PackageInfo,
    refs: Option<Refs>,
}

/// Content and project packs can be indexed; plugin packs are `WrongKind(Plugin)`.
fn require_kind(entry: &LibraryEntry) -> Result<(), ImportError> {
    match entry.sidecar.kind {
        Kind::Content | Kind::Project => Ok(()),
        k => Err(ImportError::WrongKind(k)),
    }
}

/// Build the index by reading the package headers of the library zip.
pub fn build_index(entry: &LibraryEntry) -> Result<AssetIndex, ImportError> {
    require_kind(entry)?;
    let kind = entry.sidecar.kind;
    let mut zip = open_zip(entry)?;
    let (infos, repairs) =
        scan_with_repairs(&mut zip, &entry.zip_path).map_err(ImportError::Unsafe)?;
    let files: Vec<&EntryInfo> = infos.iter().filter(|e| !e.is_dir).collect();

    // Group package files by stem.
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut other: Vec<OtherFile> = Vec::new();
    let mut push_other = |e: &EntryInfo| {
        // Project packs: only the Content/ tree is browsable (Config/, Source/, Plugins/, ... are not).
        if kind == Kind::Project && !under_content(&e.path) {
            return;
        }
        other.push(OtherFile {
            path: e.path.clone(),
            size: e.size,
        })
    };
    for (i, e) in files.iter().enumerate() {
        let ext = ext_lower(&e.path);
        match ext.as_str() {
            "uasset" | "umap" if game_path_of(kind, &e.path).is_some() => {
                let g = groups
                    .entry(strip_ext(&e.path).to_ascii_lowercase())
                    .or_default();
                if g.main.is_none() {
                    g.main = Some(i);
                } else {
                    push_other(e);
                }
            }
            "uexp" | "ubulk" | "uptnl" => {
                let mut key = strip_ext(&e.path).to_ascii_lowercase();
                if key.ends_with(".m") {
                    key.truncate(key.len() - 2);
                }
                groups.entry(key).or_default().companions.push(i);
            }
            _ => push_other(e),
        }
    }

    let mut pending: Vec<Pending> = Vec::new();
    let mut engine_min: Option<(u32, u32)> = None;
    let mut engine_max: Option<(u32, u32)> = None;
    for g in groups.into_values() {
        match g.main {
            Some(m) => {
                let main = files[m];
                let Some(game_path) = game_path_of(kind, &main.path) else {
                    push_other(main);
                    continue;
                };
                let mut companions: Vec<String> = g
                    .companions
                    .iter()
                    .map(|&c| files[c].path.clone())
                    .collect();
                companions.sort();
                let size = main
                    .size
                    .saturating_add(g.companions.iter().map(|&c| files[c].size).sum::<u64>());
                let (pkg, reason) =
                    match read_package_err(&mut zip, main.index, &main.path, &repairs) {
                        Ok(p) => (Some(p), None),
                        Err(u) => (None, Some(u.code.to_string())),
                    };
                if let Some(mm) = pkg.as_ref().and_then(|p| major_minor(&p.engine)) {
                    engine_min = Some(engine_min.map_or(mm, |m: (u32, u32)| m.min(mm)));
                    engine_max = Some(engine_max.map_or(mm, |m: (u32, u32)| m.max(mm)));
                }
                let has_thumbnail = pkg.as_ref().is_some_and(|p| p.thumbnail.is_some());
                pending.push(Pending {
                    info: PackageInfo {
                        game_path,
                        zip_path: main.path.clone(),
                        companions,
                        class: pkg.as_ref().and_then(|p| p.class.clone()),
                        size,
                        deps: Vec::new(),
                        external: Vec::new(),
                        has_thumbnail,
                        header_unreadable: pkg.is_none(),
                        unreadable_reason: reason,
                    },
                    refs: pkg.map(|p| {
                        let mut hard = p.imported_packages();
                        hard.extend(p.soft_packages());
                        Refs {
                            hard,
                            from_names: p.name_table_packages(),
                        }
                    }),
                });
            }
            None => {
                for c in g.companions {
                    push_other(files[c]);
                }
            }
        }
    }

    // Lookup of pack packages by lower-case game path (+ renamed-after-cooking fallback).
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut by_stripped: HashMap<String, Option<usize>> = HashMap::new();
    for (i, p) in pending.iter().enumerate() {
        let low = p.info.game_path.to_ascii_lowercase();
        by_path.insert(low.clone(), i);
        let (dir, stem) = low.rsplit_once('/').unwrap_or(("", &low));
        if let Some(base) = strip_numeric_suffix(stem) {
            let key = format!("{dir}/{base}");
            by_stripped
                .entry(key)
                .and_modify(|v| *v = None) // ambiguous
                .or_insert(Some(i));
        }
    }
    let resolve = |dep: &str| -> Option<usize> {
        let low = dep.to_ascii_lowercase();
        by_path
            .get(&low)
            .copied()
            .or_else(|| by_stripped.get(&low).copied().flatten())
    };

    for i in 0..pending.len() {
        let Some(refs) = pending[i].refs.take() else {
            continue;
        };
        let own = pending[i].info.game_path.to_ascii_lowercase();
        let own_mount = mount_of(&pending[i].info.game_path).to_ascii_lowercase();
        // (path, from_name_table)
        let mut raw: Vec<(String, bool)> = Vec::new();
        raw.extend(refs.hard.into_iter().map(|p| (p, false)));
        raw.extend(refs.from_names.into_iter().map(|p| (p, true)));
        let mut deps: BTreeSet<String> = BTreeSet::new();
        let mut external: BTreeSet<String> = BTreeSet::new();
        for (path, from_names) in raw {
            if path.eq_ignore_ascii_case(&own) {
                continue;
            }
            if let Some(t) = resolve(&path) {
                if t != i {
                    deps.insert(pending[t].info.game_path.clone());
                }
            } else if !from_names {
                external.insert(path);
            } else {
                // Name-table strings are noisy: only own-mount and /Game/ strings count.
                let m = mount_of(&path).to_ascii_lowercase();
                if m == "game" || m == own_mount {
                    external.insert(path);
                }
            }
        }
        pending[i].info.deps = deps.into_iter().collect();
        pending[i].info.external = external.into_iter().collect();
    }

    let mut packages: Vec<PackageInfo> = pending.into_iter().map(|p| p.info).collect();
    packages.sort_by(|a, b| a.game_path.cmp(&b.game_path));
    other.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(AssetIndex {
        schema: INDEX_SCHEMA,
        sidecar_sha256: entry.sidecar.sha256.clone(),
        entry_count: entry.sidecar.entry_count,
        packages,
        other_files: other,
        engine_version_min: engine_min.map(|(a, b)| format!("{a}.{b}")),
        engine_version_max: engine_max.map(|(a, b)| format!("{a}.{b}")),
    })
}

/// `<zip>.assets.json`, next to the sidecar.
pub fn index_path(entry: &LibraryEntry) -> PathBuf {
    let mut s = entry.zip_path.clone().into_os_string();
    s.push(".assets.json");
    PathBuf::from(s)
}

fn is_fresh(entry: &LibraryEntry, idx: &AssetIndex) -> bool {
    idx.schema == INDEX_SCHEMA
        && idx.sidecar_sha256 == entry.sidecar.sha256
        && idx.entry_count == entry.sidecar.entry_count
}

/// Write the overlay atomically (temp file in the same folder, then rename).
pub fn write_index(entry: &LibraryEntry, index: &AssetIndex) -> Result<(), ImportError> {
    let path = index_path(entry);
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let json = serde_json::to_vec(index).map_err(|e| ImportError::Io(e.to_string()))?;
    let tmp = dir.join(format!(".tmp-{}.json", unique_token()));
    let res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &path)
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(ImportError::Io(e.to_string()));
    }
    Ok(())
}

/// Read the overlay when it is valid and fresh, else build the index and try to write it
/// (a write failure, e.g. a read-only share, is ignored).
pub fn load_or_build_index(entry: &LibraryEntry) -> Result<AssetIndex, ImportError> {
    if let Ok(text) = fs::read_to_string(index_path(entry)) {
        let text = text.trim_start_matches('\u{feff}');
        if let Ok(idx) = serde_json::from_str::<AssetIndex>(text) {
            if is_fresh(entry, &idx) {
                return Ok(idx);
            }
        }
    }
    let idx = build_index(entry)?;
    let _ = write_index(entry, &idx);
    Ok(idx)
}

/// Transitive closure over `deps`. `selected` are `game_path`s; unknown ones are dropped.
/// Returns every package needed including the selected ones, sorted, without duplicates.
pub fn closure(index: &AssetIndex, selected: &[String]) -> Vec<String> {
    let map: HashMap<String, &PackageInfo> = index
        .packages
        .iter()
        .map(|p| (p.game_path.to_ascii_lowercase(), p))
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<&PackageInfo> = selected
        .iter()
        .filter_map(|s| map.get(&s.to_ascii_lowercase()).copied())
        .collect();
    while let Some(p) = stack.pop() {
        if !seen.insert(p.game_path.clone()) {
            continue;
        }
        for d in &p.deps {
            if let Some(q) = map.get(&d.to_ascii_lowercase()) {
                if !seen.contains(&q.game_path) {
                    stack.push(q);
                }
            }
        }
    }
    seen.into_iter().collect()
}

/// PNG/JPEG bytes of the editor thumbnail of `game_path`, read lazily from the zip header.
/// `Ok(None)`: no thumbnail (cooked, unreadable or none stored). Unknown package: `UnknownPackage`.
pub fn read_thumbnail(
    entry: &LibraryEntry,
    game_path: &str,
) -> Result<Option<Vec<u8>>, ImportError> {
    require_kind(entry)?;
    let kind = entry.sidecar.kind;
    let mut zip = open_zip(entry)?;
    let want = game_path.to_ascii_lowercase();
    let mut found: Option<(usize, String)> = None;
    for i in 0..zip.len() {
        let name = {
            let f = zip.by_index_raw(i)?;
            if f.is_dir() {
                continue;
            }
            f.name().to_string()
        };
        let Ok(path) = normalize_name(&name) else {
            continue;
        };
        if !matches!(ext_lower(&path).as_str(), "uasset" | "umap") {
            continue;
        }
        if game_path_of(kind, &path).is_some_and(|g| g.to_ascii_lowercase() == want) {
            found = Some((i, path.clone()));
            break;
        }
    }
    let Some((i, norm_path)) = found else {
        return Err(ImportError::UnknownPackage(game_path.to_string()));
    };
    let repairs = Repairs::load(&entry.zip_path);
    Ok(read_package_err(&mut zip, i, &norm_path, &repairs)
        .ok()
        .and_then(|p| p.thumbnail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(gp: &str, deps: &[&str]) -> PackageInfo {
        PackageInfo {
            game_path: gp.into(),
            zip_path: format!("Content{}.uasset", &gp[5..]),
            companions: vec![],
            class: None,
            size: 1,
            deps: deps.iter().map(|d| d.to_string()).collect(),
            external: vec![],
            has_thumbnail: false,
            header_unreadable: false,
            unreadable_reason: None,
        }
    }

    #[test]
    fn paths_map_to_game_paths() {
        assert_eq!(
            game_path_of(Kind::Content, "Content/A/B.uasset").as_deref(),
            Some("/Game/A/B")
        );
        assert_eq!(
            game_path_of(Kind::Project, "content/A/B.umap").as_deref(),
            Some("/Game/A/B")
        );
        assert_eq!(
            game_path_of(Kind::Project, "Plugins/P/Content/X.uasset"),
            None
        );
        assert_eq!(
            game_path_of(Kind::Content, "Content/X.uasset").as_deref(),
            Some("/Game/X")
        );
        assert_eq!(
            game_path_of(Kind::Plugin, "Plugins/P/Content/X.uasset"),
            None
        );
        assert_eq!(strip_ext("Content/A.b/C"), "Content/A.b/C");
        assert_eq!(strip_ext("Content/A.b/C.m.ubulk"), "Content/A.b/C.m");
        assert!(under_content("Content/x"));
        assert!(!under_content("Content"));
        assert!(!under_content("Config/x"));
    }

    #[test]
    fn closure_with_cycles_and_missing_deps() {
        let idx = AssetIndex {
            schema: INDEX_SCHEMA,
            sidecar_sha256: String::new(),
            entry_count: 0,
            packages: vec![
                p("/Game/A", &["/Game/B", "/Game/Missing"]),
                p("/Game/B", &["/Game/C"]),
                p("/Game/C", &["/Game/A", "/Game/C"]),
                p("/Game/D", &[]),
            ],
            other_files: vec![],
            engine_version_min: None,
            engine_version_max: None,
        };
        let sel = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            closure(&idx, &sel(&["/Game/B"])),
            sel(&["/Game/A", "/Game/B", "/Game/C"])
        );
        assert_eq!(closure(&idx, &sel(&["/Game/D"])), sel(&["/Game/D"]));
        assert!(closure(&idx, &sel(&["/Game/Zed"])).is_empty());
    }
}
