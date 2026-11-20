//! Minimal reader for the header of Unreal packages (`.uasset` / `.umap`).
//!
//! Ported from the idea and structure of Hoard's `hoard-unreal` package reader (MIT-0): the layout
//! follows `FPackageFileSummary`, `FObjectImport` and `FObjectExport`, gated on the file's UE4/UE5
//! object versions. Only what selective import needs is kept: names, imports, exports (main class),
//! soft package references, soft object paths and the editor thumbnail.
//!
//! Every read is bounds checked and every count is checked against the bytes that are left, so
//! garbage or truncated input returns a [`ParseError`] and never panics or over-allocates.

use std::fmt;

pub const PACKAGE_TAG: u32 = 0x9E2A_83C1;
/// Package flag: editor-only data stripped (a cooked package).
const PKG_FILTER_EDITOR_ONLY: u32 = 0x8000_0000;

// UE4 object versions (EUnrealEngineObjectUE4Version).
mod ue4 {
    pub const REMOVE_NET_INDEX: i32 = 196;
    pub const ENGINE_VERSION_OBJECT: i32 = 336;
    pub const LOAD_FOR_EDITOR_GAME: i32 = 365;
    pub const ADD_STRING_ASSET_REFERENCES_MAP: i32 = 384;
    pub const SERIALIZE_TEXT_IN_PACKAGES: i32 = 459;
    pub const COOKED_ASSETS_IN_EDITOR_SUPPORT: i32 = 485;
    pub const NAME_HASHES_SERIALIZED: i32 = 504;
    pub const PRELOAD_DEPENDENCIES_IN_COOKED_EXPORTS: i32 = 507;
    pub const TEMPLATE_INDEX_IN_COOKED_EXPORTS: i32 = 508;
    pub const ADDED_SEARCHABLE_NAMES: i32 = 510;
    pub const BIT64_EXPORTMAP_SERIALSIZES: i32 = 511;
    pub const ADDED_PACKAGE_SUMMARY_LOCALIZATION_ID: i32 = 516;
    pub const ADDED_PACKAGE_OWNER: i32 = 518;
    pub const NON_OUTER_PACKAGE_IMPORT: i32 = 520;
}

// UE5 object versions (EUnrealEngineObjectUE5Version).
mod ue5 {
    pub const OPTIONAL_RESOURCES: i32 = 1003;
    pub const REMOVE_OBJECT_EXPORT_PACKAGE_GUID: i32 = 1005;
    pub const TRACK_OBJECT_EXPORT_IS_INHERITED: i32 = 1006;
    pub const FSOFTOBJECTPATH_REMOVE_ASSET_PATH_FNAMES: i32 = 1007;
    pub const ADD_SOFTOBJECTPATH_LIST: i32 = 1008;
    pub const SCRIPT_SERIALIZATION_OFFSET: i32 = 1010;
    pub const METADATA_SERIALIZATION_OFFSET: i32 = 1014;
    pub const VERSE_CELLS: i32 = 1015;
    pub const PACKAGE_SAVED_HASH: i32 = 1016;
    pub const IMPORT_TYPE_HIERARCHIES: i32 = 1018;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "uasset: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

type Res<T> = Result<T, ParseError>;

fn err<T>(m: impl Into<String>) -> Res<T> {
    Err(ParseError(m.into()))
}

/// Little-endian reader over a byte slice; every read is bounds checked.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn seek(&mut self, pos: i64) -> Res<()> {
        let p = usize::try_from(pos).or_else(|_| err("negative offset"))?;
        if p > self.data.len() {
            return err(format!(
                "offset {p} past the end ({} bytes)",
                self.data.len()
            ));
        }
        self.pos = p;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        let end = match self.pos.checked_add(n) {
            Some(e) => e,
            None => return err("length overflow"),
        };
        match self.data.get(self.pos..end) {
            Some(s) => {
                self.pos = end;
                Ok(s)
            }
            None => err("data ends early"),
        }
    }

    fn skip(&mut self, n: u64) -> Res<()> {
        let n = usize::try_from(n).or_else(|_| err("skip too large"))?;
        self.take(n).map(|_| ())
    }

    fn arr<const N: usize>(&mut self) -> Res<[u8; N]> {
        let s = self.take(N)?;
        let mut b = [0u8; N];
        b.copy_from_slice(s);
        Ok(b)
    }

    fn u16(&mut self) -> Res<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    fn i32(&mut self) -> Res<i32> {
        Ok(i32::from_le_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Res<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn guid(&mut self) -> Res<[u8; 16]> {
        self.arr()
    }

    /// A count that must be plausible before anything is allocated for it: non-negative, at most
    /// `max`, and `min_item` bytes per item must still fit in the data that is left.
    fn count(&mut self, max: usize, min_item: usize) -> Res<usize> {
        let n = self.i32()?;
        let n = match usize::try_from(n) {
            Ok(n) => n,
            Err(_) => return err(format!("negative count {n}")),
        };
        if n > max {
            return err(format!("implausible count {n}"));
        }
        match n.checked_mul(min_item) {
            Some(b) if b <= self.remaining() => Ok(n),
            _ => err(format!("count {n} does not fit in the data")),
        }
    }

    /// `FString`: length including the terminator; negative means UTF-16.
    fn fstring(&mut self) -> Res<String> {
        let len = self.i32()?;
        if len == 0 {
            return Ok(String::new());
        }
        let n = len.unsigned_abs() as usize;
        if n > 1 << 20 {
            return err(format!("implausible string length {n}"));
        }
        if len > 0 {
            let s = self.take(n)?;
            let s = s.strip_suffix(&[0]).unwrap_or(s);
            Ok(String::from_utf8_lossy(s).into_owned())
        } else {
            let raw = self.take(n * 2)?;
            let mut units: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            if units.last() == Some(&0) {
                units.pop();
            }
            Ok(String::from_utf16_lossy(&units))
        }
    }
}

/// `FName` as stored in tables: an index into the name map plus an instance number.
fn fname(r: &mut Reader<'_>, names: &[String]) -> Res<String> {
    let index = r.i32()?;
    let number = r.i32()?;
    let base = match usize::try_from(index).ok().and_then(|i| names.get(i)) {
        Some(b) => b,
        None => return err("name index out of range"),
    };
    Ok(if number > 0 {
        format!("{base}_{}", number - 1)
    } else {
        base.clone()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub class_name: String,
    pub outer_index: i32,
    pub object_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub class_index: i32,
    pub outer_index: i32,
    pub name: String,
    pub is_asset: bool,
}

/// What one package header tells us.
#[derive(Debug, Default, Clone)]
pub struct Package {
    /// "5.3.2" (empty when the package did not record it).
    pub engine: String,
    pub cooked: bool,
    /// Short class name of the main export, e.g. `StaticMesh`.
    pub class: Option<String>,
    /// Object name of the main export.
    pub asset_name: Option<String>,
    pub total_header_size: u64,
    pub ue4: i32,
    pub ue5: i32,
    pub flags: u32,
    pub custom_versions: Vec<([u8; 16], i32)>,
    pub names: Vec<String>,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
    /// Soft package reference table (UE 4.x+), raw names.
    pub soft_package_refs: Vec<String>,
    /// Soft object paths table (UE 5.1+), `Package.Asset:Sub` strings.
    pub soft_object_paths: Vec<String>,
    /// Editor thumbnail, PNG or JPEG bytes (not present in cooked packages).
    pub thumbnail: Option<Vec<u8>>,
}

/// Prefixes that are never in a content pack.
fn is_engine_package(p: &str) -> bool {
    const SKIP: [&str; 4] = ["/Script/", "/Engine/", "/Temp/", "/Memory/"];
    SKIP.iter()
        .any(|s| p.len() >= s.len() && p.as_bytes()[..s.len()].eq_ignore_ascii_case(s.as_bytes()))
        || p.eq_ignore_ascii_case("/Script")
        || p.eq_ignore_ascii_case("/Engine")
}

/// Turn an object path (`/Game/A/B.B:Sub`, `/Game/A/B.B_C`) into a package path (`/Game/A/B`).
/// Returns `None` when the string does not look like `/<Mount>/<something>`.
pub fn normalize_package_path(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with('/') || s.len() > 400 {
        return None;
    }
    let cut = s.find(['.', ':']).map(|i| &s[..i]).unwrap_or(s);
    let cut = cut.trim_end_matches('/');
    if cut.chars().any(|c| {
        (c as u32) < 0x20
            || c == '\u{7f}'
            || matches!(c, '\\' | '"' | '<' | '>' | '|' | '?' | '*' | ',')
    }) {
        return None;
    }
    let mut parts = cut[1..].split('/');
    let mount = parts.next()?;
    if mount.is_empty() {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() || rest.iter().any(|p| p.is_empty()) {
        return None;
    }
    if is_engine_package(cut) {
        return None;
    }
    Some(cut.to_string())
}

impl Package {
    /// The main asset export (flagged as the asset, else the first top-level object).
    pub fn main_export(&self) -> Option<&Export> {
        self.exports
            .iter()
            .find(|e| e.is_asset)
            .or_else(|| self.exports.iter().find(|e| e.outer_index == 0))
    }

    /// Packages named by the import table (engine and `/Script/` left out), sorted, unique.
    pub fn imported_packages(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .imports
            .iter()
            .filter(|i| i.class_name == "Package" && i.object_name.starts_with('/'))
            .filter_map(|i| normalize_package_path(&i.object_name))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Soft package references and the package part of soft object paths, sorted, unique.
    pub fn soft_packages(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .soft_package_refs
            .iter()
            .chain(self.soft_object_paths.iter())
            .filter_map(|p| normalize_package_path(p))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Name-table entries that look like package paths (soft object paths live in the name table).
    pub fn name_table_packages(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .names
            .iter()
            .filter(|n| n.starts_with('/'))
            .filter_map(|n| normalize_package_path(n))
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Tag, legacy file version, UE4 and UE5 object versions (licensee version skipped).
fn versions(r: &mut Reader<'_>) -> Res<(i32, i32, i32)> {
    let tag = r.u32()?;
    if tag != PACKAGE_TAG {
        if tag == PACKAGE_TAG.swap_bytes() {
            return err("big-endian packages are not supported");
        }
        return err("not an Unreal package");
    }
    let legacy = r.i32()?;
    if !(-9..0).contains(&legacy) {
        return err(format!("unsupported package format {legacy}"));
    }
    if legacy != -4 {
        r.i32()?; // legacy UE3 version
    }
    let ue4v = r.i32()?;
    let ue5v = if legacy <= -8 { r.i32()? } else { 0 };
    r.i32()?; // licensee version
    if ue4v == 0 && ue5v == 0 {
        return err("unversioned package (cooked without version info)");
    }
    Ok((legacy, ue4v, ue5v))
}

fn custom_versions(r: &mut Reader<'_>, legacy: i32) -> Res<Vec<([u8; 16], i32)>> {
    let mut out = Vec::new();
    if legacy > -2 {
        return Ok(out);
    }
    let n = r.count(4096, 8)?;
    for _ in 0..n {
        match legacy {
            -2 => r.skip(8)?, // enum tag + version
            -5..=-3 => {
                let g = r.guid()?;
                out.push((g, r.i32()?));
                r.fstring()?;
            }
            _ => {
                let g = r.guid()?;
                out.push((g, r.i32()?));
            }
        }
    }
    Ok(out)
}

/// `TotalHeaderSize` from the start of the summary, without parsing the rest. Needs the first
/// few hundred bytes (plus the custom version list for packages older than UE 5.5).
pub fn header_size(data: &[u8]) -> Result<usize, ParseError> {
    let mut r = Reader::new(data);
    let (legacy, _ue4v, ue5v) = versions(&mut r)?;
    if ue5v >= ue5::PACKAGE_SAVED_HASH {
        r.skip(20)?;
    } else {
        custom_versions(&mut r, legacy)?;
    }
    let n = r.i32()?;
    match usize::try_from(n) {
        Ok(n) if n > 0 => Ok(n),
        _ => err(format!("bad header size {n}")),
    }
}

fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n") || b.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Parse a package header. `data` must hold at least `TotalHeaderSize` bytes (the rest of the
/// file is not needed).
pub fn parse(data: &[u8]) -> Result<Package, ParseError> {
    let mut r = Reader::new(data);
    let (legacy, ue4v, ue5v) = versions(&mut r)?;
    let total_header;
    let customs;
    if ue5v >= ue5::PACKAGE_SAVED_HASH {
        r.skip(20)?;
        total_header = r.i32()?;
        customs = custom_versions(&mut r, legacy)?;
    } else {
        customs = custom_versions(&mut r, legacy)?;
        total_header = r.i32()?;
    }
    r.fstring()?; // folder name
    let flags = r.u32()?;
    let cooked = flags & PKG_FILTER_EDITOR_ONLY != 0;
    let name_count = r.count(4_000_000, 4)?;
    let name_offset = r.i32()?;
    let (mut soft_obj_count, mut soft_obj_offset) = (0usize, 0i32);
    if ue5v >= ue5::ADD_SOFTOBJECTPATH_LIST {
        soft_obj_count = r.count(4_000_000, 0)?;
        soft_obj_offset = r.i32()?;
    }
    if !cooked && ue4v >= ue4::ADDED_PACKAGE_SUMMARY_LOCALIZATION_ID {
        r.fstring()?;
    }
    if ue4v >= ue4::SERIALIZE_TEXT_IN_PACKAGES {
        r.i32()?;
        r.i32()?;
    }
    let export_count = r.count(4_000_000, 0)?;
    let export_offset = r.i32()?;
    let import_count = r.count(4_000_000, 0)?;
    let import_offset = r.i32()?;
    if ue5v >= ue5::VERSE_CELLS {
        for _ in 0..4 {
            r.i32()?;
        }
    }
    if ue5v >= ue5::METADATA_SERIALIZATION_OFFSET {
        r.i32()?;
    }
    r.i32()?; // depends offset
    let (mut soft_ref_count, mut soft_ref_offset) = (0i32, 0i32);
    if ue4v >= ue4::ADD_STRING_ASSET_REFERENCES_MAP {
        soft_ref_count = r.i32()?;
        soft_ref_offset = r.i32()?;
    }
    if ue4v >= ue4::ADDED_SEARCHABLE_NAMES {
        r.i32()?;
    }
    let thumbnail_offset = r.i32()?;
    if ue5v >= ue5::IMPORT_TYPE_HIERARCHIES {
        r.i32()?;
        r.i32()?;
    }
    if ue5v < ue5::PACKAGE_SAVED_HASH {
        r.guid()?;
    }
    if !cooked && ue4v >= ue4::ADDED_PACKAGE_OWNER {
        r.guid()?;
        if ue4v < ue4::NON_OUTER_PACKAGE_IMPORT {
            r.guid()?;
        }
    }
    let generations = r.count(100_000, 8)?;
    let per_generation: u64 = if ue4v >= ue4::REMOVE_NET_INDEX { 8 } else { 12 };
    r.skip(generations as u64 * per_generation)?;
    let engine = if ue4v >= ue4::ENGINE_VERSION_OBJECT {
        let (major, minor, patch) = (r.u16()?, r.u16()?, r.u16()?);
        r.u32()?; // changelist
        r.fstring()?; // branch
        if major > 0 {
            format!("{major}.{minor}.{patch}")
        } else {
            String::new()
        }
    } else {
        r.i32()?;
        String::new()
    };
    // Everything after this point in the summary is not needed.

    let total_header_size = match usize::try_from(total_header) {
        Ok(t) if t > 0 && t <= data.len() => t,
        _ => return err(format!("header size {total_header} does not fit")),
    };
    // Tables must lie inside the header.
    let data = &data[..total_header_size];
    let mut r = Reader::new(data);

    let mut pkg = Package {
        engine,
        cooked,
        total_header_size: total_header_size as u64,
        ue4: ue4v,
        ue5: ue5v,
        flags,
        custom_versions: customs,
        ..Package::default()
    };

    // Name map.
    r.seek(i64::from(name_offset))?;
    let mut names = Vec::with_capacity(name_count.min(1 << 16));
    for _ in 0..name_count {
        names.push(r.fstring()?);
        if ue4v >= ue4::NAME_HASHES_SERIALIZED {
            r.skip(4)?;
        }
    }

    // Imports.
    r.seek(i64::from(import_offset))?;
    if import_count
        .checked_mul(20)
        .is_none_or(|b| b > r.remaining())
    {
        return err("import table does not fit");
    }
    let mut imports = Vec::with_capacity(import_count.min(1 << 16));
    for _ in 0..import_count {
        fname(&mut r, &names)?; // class package
        let class_name = fname(&mut r, &names)?;
        let outer_index = r.i32()?;
        let object_name = fname(&mut r, &names)?;
        if !cooked && ue4v >= ue4::NON_OUTER_PACKAGE_IMPORT {
            fname(&mut r, &names)?;
        }
        if ue5v >= ue5::OPTIONAL_RESOURCES {
            r.i32()?;
        }
        imports.push(Import {
            class_name,
            outer_index,
            object_name,
        });
    }

    // Exports.
    r.seek(i64::from(export_offset))?;
    if export_count
        .checked_mul(40)
        .is_none_or(|b| b > r.remaining())
    {
        return err("export table does not fit");
    }
    let mut exports = Vec::with_capacity(export_count.min(1 << 16));
    for _ in 0..export_count {
        let class_index = r.i32()?;
        r.i32()?; // super
        if ue4v >= ue4::TEMPLATE_INDEX_IN_COOKED_EXPORTS {
            r.i32()?;
        }
        let outer_index = r.i32()?;
        let name = fname(&mut r, &names)?;
        r.u32()?; // object flags
        if ue4v >= ue4::BIT64_EXPORTMAP_SERIALSIZES {
            r.skip(16)?; // serial size + offset
        } else {
            r.skip(8)?;
        }
        r.skip(12)?; // forced export, not for client, not for server
        if ue5v < ue5::REMOVE_OBJECT_EXPORT_PACKAGE_GUID {
            r.guid()?;
        }
        if ue5v >= ue5::TRACK_OBJECT_EXPORT_IS_INHERITED {
            r.i32()?;
        }
        r.u32()?; // package flags
        if ue4v >= ue4::LOAD_FOR_EDITOR_GAME {
            r.i32()?;
        }
        let is_asset = ue4v >= ue4::COOKED_ASSETS_IN_EDITOR_SUPPORT && r.i32()? != 0;
        if ue5v >= ue5::OPTIONAL_RESOURCES {
            r.i32()?;
        }
        if ue4v >= ue4::PRELOAD_DEPENDENCIES_IN_COOKED_EXPORTS {
            r.skip(20)?;
        }
        if ue5v >= ue5::SCRIPT_SERIALIZATION_OFFSET {
            // Present for tagged (non-unversioned) properties only.
            if flags & 0x0000_2000 == 0 {
                r.skip(16)?;
            }
        }
        exports.push(Export {
            class_index,
            outer_index,
            name,
            is_asset,
        });
    }
    let class_of = |e: &Export| -> Option<String> {
        match e.class_index {
            i if i < 0 => usize::try_from(-i64::from(i) - 1)
                .ok()
                .and_then(|k| imports.get(k))
                .map(|imp| imp.object_name.clone()),
            i if i > 0 => usize::try_from(i - 1)
                .ok()
                .and_then(|k| exports.get(k))
                .map(|ex| ex.name.clone()),
            _ => None,
        }
    };
    if let Some(main) = exports
        .iter()
        .find(|e| e.is_asset)
        .or_else(|| exports.iter().find(|e| e.outer_index == 0))
    {
        pkg.class = class_of(main);
        pkg.asset_name = Some(main.name.clone());
    }

    // Soft package references (best effort).
    if soft_ref_count > 0 && soft_ref_offset > 0 {
        let _ = (|| -> Res<()> {
            r.seek(i64::from(soft_ref_offset))?;
            let n = usize::try_from(soft_ref_count).unwrap_or(0).min(1_000_000);
            if n.checked_mul(8).is_none_or(|b| b > r.remaining()) {
                return err("soft package refs do not fit");
            }
            for _ in 0..n {
                pkg.soft_package_refs.push(fname(&mut r, &names)?);
            }
            Ok(())
        })();
    }

    // Soft object paths (best effort).
    if soft_obj_count > 0 && soft_obj_offset > 0 {
        let _ = (|| -> Res<()> {
            r.seek(i64::from(soft_obj_offset))?;
            for _ in 0..soft_obj_count.min(1_000_000) {
                let path = if ue5v >= ue5::FSOFTOBJECTPATH_REMOVE_ASSET_PATH_FNAMES {
                    let package = fname(&mut r, &names)?;
                    let asset = fname(&mut r, &names)?;
                    let sub = r.fstring()?;
                    let mut p = if asset == "None" {
                        package
                    } else {
                        format!("{package}.{asset}")
                    };
                    if !sub.is_empty() {
                        p = format!("{p}:{sub}");
                    }
                    p
                } else {
                    let path = fname(&mut r, &names)?;
                    let sub = r.fstring()?;
                    if sub.is_empty() {
                        path
                    } else {
                        format!("{path}:{sub}")
                    }
                };
                pkg.soft_object_paths.push(path);
            }
            Ok(())
        })();
    }

    // Thumbnail table (editor packages only; best effort).
    if !cooked && thumbnail_offset > 0 {
        let asset_name = pkg.asset_name.clone();
        let found = (|| -> Res<(Option<String>, Option<Vec<u8>>)> {
            r.seek(i64::from(thumbnail_offset))?;
            let n = r.count(10_000, 12)?;
            let mut entries: Vec<(String, String, i32)> = Vec::new();
            for _ in 0..n {
                let class = r.fstring()?;
                let path = r.fstring()?;
                let offset = r.i32()?;
                entries.push((class, path, offset));
            }
            // Prefer the entry named like the main asset.
            let want = |p: &str| {
                asset_name.as_deref().is_some_and(|a| {
                    let tail = p.rsplit(['.', '/', ':']).next().unwrap_or(p);
                    tail == a
                })
            };
            let mut order: Vec<usize> = (0..entries.len())
                .filter(|&i| want(&entries[i].1))
                .collect();
            order.extend((0..entries.len()).filter(|&i| !want(&entries[i].1)));
            let first_class = entries.first().map(|e| e.0.clone());
            for i in order {
                let mut tr = Reader::new(data);
                if tr.seek(i64::from(entries[i].2)).is_err() {
                    continue;
                }
                let (Ok(_w), Ok(_h)) = (tr.i32(), tr.i32()) else {
                    continue;
                };
                let Ok(len) = tr.count(64 << 20, 1) else {
                    continue;
                };
                if len == 0 {
                    continue;
                }
                if let Ok(bytes) = tr.take(len) {
                    if looks_like_image(bytes) {
                        return Ok((first_class, Some(bytes.to_vec())));
                    }
                }
            }
            Ok((first_class, None))
        })();
        if let Ok((class, thumb)) = found {
            if pkg.class.is_none() {
                pkg.class = class.filter(|c| !c.is_empty());
            }
            pkg.thumbnail = thumb;
        }
    }

    pkg.names = names;
    pkg.imports = imports;
    pkg.exports = exports;
    Ok(pkg)
}

/// Test-only synthetic package writer (also used by the integration tests). Lays out an editor or
/// cooked package field by field, the way [`parse`] reads it, so no real Unreal content is needed.
#[doc(hidden)]
pub mod synth {
    use super::PACKAGE_TAG;

    #[derive(Clone, Debug)]
    pub struct Synth {
        pub class: String,
        pub asset: String,
        /// Imported packages (`/Game/...`, `/Script/Engine`, `/Engine/...`).
        pub deps: Vec<String>,
        /// Soft package reference table entries.
        pub soft_refs: Vec<String>,
        /// Extra name-table entries (soft object paths live there).
        pub extra_names: Vec<String>,
        /// 0 = UE 4.27 style, 1009 = UE 5.3 style, 1017 = 5.5 style (saved hash).
        pub ue5: i32,
        pub cooked: bool,
        pub thumbnail: Option<Vec<u8>>,
        /// Bytes appended after the header (export data).
        pub body: Vec<u8>,
    }

    impl Synth {
        pub fn new(class: &str, asset: &str) -> Self {
            Self {
                class: class.into(),
                asset: asset.into(),
                deps: Vec::new(),
                soft_refs: Vec::new(),
                extra_names: Vec::new(),
                ue5: 1009,
                cooked: false,
                thumbnail: Some(b"\x89PNG\r\n\x1a\nthumb".to_vec()),
                body: Vec::new(),
            }
        }

        pub fn dep(mut self, d: &str) -> Self {
            self.deps.push(d.into());
            self
        }

        pub fn soft(mut self, d: &str) -> Self {
            self.soft_refs.push(d.into());
            self
        }

        pub fn name(mut self, d: &str) -> Self {
            self.extra_names.push(d.into());
            self
        }

        pub fn build(&self) -> Vec<u8> {
            build(self)
        }
    }

    fn fstr(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }

    fn idx(names: &mut Vec<String>, s: &str) -> i32 {
        if let Some(i) = names.iter().position(|x| x == s) {
            return i as i32;
        }
        names.push(s.to_owned());
        (names.len() - 1) as i32
    }

    fn patch(s: &mut [u8], at: usize, v: usize) {
        s[at..at + 4].copy_from_slice(&(v as i32).to_le_bytes());
    }

    fn build(p: &Synth) -> Vec<u8> {
        let ue5 = p.ue5;
        let cooked = p.cooked;
        let mut names: Vec<String> = Vec::new();
        for n in [
            "/Script/Engine",
            "Package",
            p.class.as_str(),
            p.asset.as_str(),
            "Class",
            "None",
        ] {
            idx(&mut names, n);
        }
        for d in p
            .deps
            .iter()
            .chain(p.soft_refs.iter())
            .chain(p.extra_names.iter())
        {
            idx(&mut names, d);
        }
        let i32b = |v: i32| v.to_le_bytes();

        let mut s = Vec::new();
        s.extend_from_slice(&PACKAGE_TAG.to_le_bytes());
        s.extend_from_slice(&i32b(if ue5 > 0 { -8 } else { -7 }));
        s.extend_from_slice(&i32b(864));
        s.extend_from_slice(&i32b(522));
        if ue5 > 0 {
            s.extend_from_slice(&i32b(ue5));
        }
        s.extend_from_slice(&i32b(0)); // licensee
        let total_at;
        if ue5 >= 1016 {
            s.extend_from_slice(&[0u8; 20]); // saved hash
            total_at = s.len();
            s.extend_from_slice(&i32b(0));
            s.extend_from_slice(&i32b(0)); // custom versions
        } else {
            s.extend_from_slice(&i32b(0)); // custom versions
            total_at = s.len();
            s.extend_from_slice(&i32b(0)); // total header size
        }
        fstr(&mut s, "None");
        let flags: u32 = if cooked { 0x8000_0000 } else { 0 };
        s.extend_from_slice(&flags.to_le_bytes());
        s.extend_from_slice(&i32b(names.len() as i32));
        let name_off_at = s.len();
        s.extend_from_slice(&i32b(0));
        if ue5 >= 1008 {
            s.extend_from_slice(&i32b(0)); // soft object paths
            s.extend_from_slice(&i32b(0));
        }
        if !cooked {
            fstr(&mut s, ""); // localization id
        }
        s.extend_from_slice(&i32b(0)); // gatherable text
        s.extend_from_slice(&i32b(0));
        s.extend_from_slice(&i32b(1)); // exports
        let export_off_at = s.len();
        s.extend_from_slice(&i32b(0));
        s.extend_from_slice(&i32b((2 + p.deps.len()) as i32)); // imports
        let import_off_at = s.len();
        s.extend_from_slice(&i32b(0));
        if ue5 >= 1015 {
            for _ in 0..4 {
                s.extend_from_slice(&i32b(0));
            }
        }
        if ue5 >= 1014 {
            s.extend_from_slice(&i32b(0));
        }
        s.extend_from_slice(&i32b(0)); // depends
        s.extend_from_slice(&i32b(p.soft_refs.len() as i32));
        let soft_off_at = s.len();
        s.extend_from_slice(&i32b(0));
        s.extend_from_slice(&i32b(0)); // searchable names
        let thumb_off_at = s.len();
        s.extend_from_slice(&i32b(0));
        if ue5 >= 1018 {
            s.extend_from_slice(&i32b(0));
            s.extend_from_slice(&i32b(0));
        }
        if ue5 < 1016 {
            s.extend_from_slice(&[0u8; 16]); // guid
        }
        if !cooked {
            s.extend_from_slice(&[0u8; 16]); // persistent guid
        }
        s.extend_from_slice(&i32b(1)); // generations
        s.extend_from_slice(&i32b(1));
        s.extend_from_slice(&i32b(names.len() as i32));
        s.extend_from_slice(&(if ue5 > 0 { 5u16 } else { 4 }).to_le_bytes());
        s.extend_from_slice(&3u16.to_le_bytes());
        s.extend_from_slice(&2u16.to_le_bytes());
        s.extend_from_slice(&0u32.to_le_bytes());
        fstr(&mut s, "++UE5+Release-5.3");

        // Tables.
        let at = s.len();
        patch(&mut s, name_off_at, at);
        for n in &names {
            fstr(&mut s, n);
            s.extend_from_slice(&[0u8; 4]);
        }
        let find = |n: &str| names.iter().position(|x| x == n).unwrap_or(0) as i32;
        let fname = |s: &mut Vec<u8>, i: i32| {
            s.extend_from_slice(&i.to_le_bytes());
            s.extend_from_slice(&0i32.to_le_bytes());
        };
        let at = s.len();
        patch(&mut s, import_off_at, at);
        let mut imports: Vec<(&str, i32, &str)> = vec![
            ("Package", 0, "/Script/Engine"),
            ("Class", -1, p.class.as_str()),
        ];
        imports.extend(p.deps.iter().map(|d| ("Package", 0, d.as_str())));
        for (cls, outer, obj) in imports {
            fname(&mut s, find("/Script/Engine"));
            fname(&mut s, find(cls));
            s.extend_from_slice(&i32b(outer));
            fname(&mut s, find(obj));
            if !cooked {
                fname(&mut s, 0);
            }
            if ue5 >= 1003 {
                s.extend_from_slice(&i32b(0));
            }
        }
        let at = s.len();
        patch(&mut s, export_off_at, at);
        s.extend_from_slice(&i32b(-2)); // class: import 2
        s.extend_from_slice(&i32b(0));
        s.extend_from_slice(&i32b(0));
        s.extend_from_slice(&i32b(0));
        fname(&mut s, find(&p.asset));
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&0i64.to_le_bytes()); // serial size
        s.extend_from_slice(&0i64.to_le_bytes()); // serial offset
        s.extend_from_slice(&[0u8; 12]);
        if ue5 < 1005 {
            s.extend_from_slice(&[0u8; 16]);
        }
        if ue5 >= 1006 {
            s.extend_from_slice(&i32b(0));
        }
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&i32b(0));
        s.extend_from_slice(&i32b(1)); // is asset
        if ue5 >= 1003 {
            s.extend_from_slice(&i32b(0));
        }
        s.extend_from_slice(&[0u8; 20]);
        if ue5 >= 1010 {
            s.extend_from_slice(&[0u8; 16]);
        }
        if !p.soft_refs.is_empty() {
            let at = s.len();
            patch(&mut s, soft_off_at, at);
            for r in &p.soft_refs {
                fname(&mut s, find(r));
            }
        }
        if !cooked {
            if let Some(thumb) = &p.thumbnail {
                let at = s.len();
                patch(&mut s, thumb_off_at, at);
                s.extend_from_slice(&i32b(1));
                fstr(&mut s, &p.class);
                fstr(&mut s, &p.asset);
                let data_at_pos = s.len();
                s.extend_from_slice(&i32b(0));
                let at = s.len();
                patch(&mut s, data_at_pos, at);
                s.extend_from_slice(&i32b(4));
                s.extend_from_slice(&i32b(4));
                s.extend_from_slice(&i32b(thumb.len() as i32));
                s.extend_from_slice(thumb);
            }
        }
        let header = s.len();
        patch(&mut s, total_at, header);
        s.extend_from_slice(&p.body);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::synth::Synth;
    use super::*;

    #[test]
    fn reads_synthetic_editor_package() {
        let bytes = Synth::new("StaticMesh", "SM_Rock")
            .dep("/Game/Pack/Materials/M_Rock")
            .dep("/Engine/BasicShapes/Cube")
            .soft("/Game/Pack/Textures/T_Soft")
            .name("/Game/Pack/Textures/T_Name.T_Name")
            .name("/Game/")
            .name("not a path")
            .build();
        assert_eq!(header_size(&bytes).unwrap(), bytes.len());
        let p = parse(&bytes).unwrap();
        assert_eq!(p.engine, "5.3.2");
        assert!(!p.cooked);
        assert_eq!(p.class.as_deref(), Some("StaticMesh"));
        assert_eq!(p.asset_name.as_deref(), Some("SM_Rock"));
        assert_eq!(
            p.imported_packages(),
            vec!["/Game/Pack/Materials/M_Rock".to_string()]
        );
        assert_eq!(
            p.soft_packages(),
            vec!["/Game/Pack/Textures/T_Soft".to_string()]
        );
        let np = p.name_table_packages();
        assert!(
            np.contains(&"/Game/Pack/Textures/T_Name".to_string()),
            "{np:?}"
        );
        assert!(!np.iter().any(|n| n == "/Game"));
        assert!(p.thumbnail.as_deref().unwrap().starts_with(b"\x89PNG"));
    }

    #[test]
    fn reads_other_versions_and_cooked() {
        for ue5 in [0, 1017, 1018] {
            let mut s = Synth::new("Material", "M_A").dep("/Game/X/Y");
            s.ue5 = ue5;
            let p = parse(&s.build()).unwrap_or_else(|e| panic!("ue5={ue5}: {e}"));
            assert_eq!(p.class.as_deref(), Some("Material"), "ue5={ue5}");
            assert_eq!(p.imported_packages(), vec!["/Game/X/Y".to_string()]);
            assert!(p.thumbnail.is_some(), "ue5={ue5}");
        }
        let mut s = Synth::new("Texture2D", "T_A").dep("/Game/X/Y");
        s.cooked = true;
        let p = parse(&s.build()).unwrap();
        assert!(p.cooked);
        assert!(p.thumbnail.is_none());
        assert_eq!(p.class.as_deref(), Some("Texture2D"));
        assert_eq!(p.imported_packages(), vec!["/Game/X/Y".to_string()]);
    }

    #[test]
    fn jpeg_thumbnail_and_non_image_thumbnail() {
        let mut s = Synth::new("Material", "M_A");
        s.thumbnail = Some(vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]);
        assert!(parse(&s.build()).unwrap().thumbnail.is_some());
        s.thumbnail = Some(b"garbage bytes".to_vec());
        assert!(parse(&s.build()).unwrap().thumbnail.is_none());
        s.thumbnail = None;
        assert!(parse(&s.build()).unwrap().thumbnail.is_none());
    }

    #[test]
    fn garbage_and_truncated_never_panic() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0u8; 100]).is_err());
        assert!(header_size(&[1, 2, 3]).is_err());
        let good = Synth::new("StaticMesh", "SM_Rock")
            .dep("/Game/A/B")
            .soft("/Game/A/C")
            .build();
        // Every truncation either parses or errors.
        for n in 0..good.len() {
            let _ = parse(&good[..n]);
            let _ = header_size(&good[..n]);
        }
        // Single-byte corruption at every position, a few values.
        for i in 0..good.len() {
            for v in [0x00u8, 0xFF, 0x7F, 0x80] {
                let mut b = good.clone();
                b[i] = v;
                let _ = parse(&b);
            }
        }
        // Pseudo-random buffers behind a valid tag.
        let mut x: u32 = 12345;
        for _ in 0..200 {
            let mut b = PACKAGE_TAG.to_le_bytes().to_vec();
            for _ in 0..300 {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                b.push((x >> 24) as u8);
            }
            let _ = parse(&b);
        }
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(
            normalize_package_path("/Game/A/B.B_C"),
            Some("/Game/A/B".into())
        );
        assert_eq!(
            normalize_package_path("/Game/A/B:Sub"),
            Some("/Game/A/B".into())
        );
        assert_eq!(
            normalize_package_path("/MyPlugin/A"),
            Some("/MyPlugin/A".into())
        );
        assert_eq!(normalize_package_path("/Game"), None);
        assert_eq!(normalize_package_path("/Game/"), None);
        assert_eq!(normalize_package_path("/Script/Engine"), None);
        assert_eq!(normalize_package_path("/Engine/BasicShapes/Cube"), None);
        assert_eq!(normalize_package_path("Game/A"), None);
        assert_eq!(normalize_package_path("/Game//A"), None);
        assert_eq!(normalize_package_path("/Game/A\\B"), None);
    }
}
