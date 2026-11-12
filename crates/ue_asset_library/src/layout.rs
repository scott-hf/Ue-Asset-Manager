//! Layout planning: classify an archive and compute the rewrite of its entries.

use crate::error::NeedsReview;
use crate::harden::{check_paths, scan_entries, EntryInfo};
use crate::name::parse_engine_version;
use crate::sidecar::Kind;
use regex::bytes::Regex as BytesRegex;
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Seek};
use std::sync::OnceLock;
use zip::ZipArchive;

const HEAD_BYTES: u64 = 256 * 1024;
const DROP_PLUGIN_DIRS: [&str; 4] = ["binaries", "intermediate", "saved", "deriveddatacache"];
const ASSET_EXTS: [&str; 5] = ["uasset", "umap", "uexp", "ubulk", "uptnl"];
const MAX_EXTERNAL_REFS: usize = 100;

#[derive(Clone, Debug)]
pub struct EntryRename {
    pub index: usize,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub kind: Kind,
    pub zip_prefix: String,
    pub game_prefix: String,
    pub allowed_roots: Vec<String>,
    /// Kept file entries (directory entries are never copied).
    pub renames: Vec<EntryRename>,
    /// Entries left out (outside the root, build output, junk).
    pub dropped: usize,
    pub content_folder: Option<String>,
    pub plugin_name: Option<String>,
    pub engine_version: Option<String>,
    pub friendly_name: Option<String>,
    pub marketplace_url: Option<String>,
    pub external_refs: Vec<String>,
    pub entry_count: u64,
    pub total_uncompressed: u64,
    pub unresolved_assets: usize,
}

fn comps(p: &str) -> Vec<&str> {
    if p.is_empty() {
        Vec::new()
    } else {
        p.split('/').collect()
    }
}

fn ext_of(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => file[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn stem_of(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    }
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// UE One-File-Per-Actor folders: they sit next to the content folders under `Content/`
/// and hold actor/object packages without a `/Game/` own path of their own.
fn is_ofpa_dir(name: &str) -> bool {
    name.eq_ignore_ascii_case("__ExternalActors__")
        || name.eq_ignore_ascii_case("__ExternalObjects__")
}

fn in_ofpa(path: &str) -> bool {
    path.split('/').any(is_ofpa_dir)
}

fn is_junk(path: &str) -> bool {
    let c: Vec<&str> = path.split('/').collect();
    c.iter().any(|x| x.eq_ignore_ascii_case("__MACOSX"))
        || matches!(
            c.last().map(|s| s.to_ascii_lowercase()).as_deref(),
            Some(".ds_store") | Some("thumbs.db") | Some("desktop.ini")
        )
}

/// Path components of `path` after removing `prefix` (case-insensitive), if it lies under it.
fn strip_prefix_ci<'a>(path: &'a str, prefix: &[&str]) -> Option<Vec<&'a str>> {
    let p: Vec<&str> = path.split('/').collect();
    if p.len() <= prefix.len() {
        return None;
    }
    for (a, b) in p.iter().zip(prefix.iter()) {
        if !a.eq_ignore_ascii_case(b) {
            return None;
        }
    }
    Some(p[prefix.len()..].to_vec())
}

fn read_head<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    idx: usize,
    max: u64,
) -> Result<Vec<u8>, NeedsReview> {
    let f = zip
        .by_index(idx)
        .map_err(|e| NeedsReview::unsafe_entry("UNREADABLE_ENTRY", format!("entry {idx}: {e}")))?;
    let mut buf = Vec::new();
    f.take(max)
        .read_to_end(&mut buf)
        .map_err(|e| NeedsReview::unsafe_entry("UNREADABLE_ENTRY", format!("entry {idx}: {e}")))?;
    Ok(buf)
}

fn game_regexes() -> &'static (BytesRegex, BytesRegex) {
    static RE: OnceLock<(BytesRegex, BytesRegex)> = OnceLock::new();
    RE.get_or_init(|| {
        (
            BytesRegex::new(r"(?-u)/Game/[\x21-\x7e]+").unwrap(),
            BytesRegex::new(r"(?-u)/\x00G\x00a\x00m\x00e\x00/\x00(?:[\x21-\x7e]\x00)+").unwrap(),
        )
    })
}

/// Extract `/Game/...` paths from an asset header (ASCII and UTF-16LE passes).
pub fn extract_game_paths(bytes: &[u8]) -> Vec<String> {
    let (a, w) = game_regexes();
    let mut raw: Vec<String> = Vec::new();
    for m in a.find_iter(bytes) {
        raw.push(String::from_utf8_lossy(m.as_bytes()).into_owned());
    }
    for m in w.find_iter(bytes) {
        let b: Vec<u8> = m.as_bytes().iter().step_by(2).copied().collect();
        raw.push(String::from_utf8_lossy(&b).into_owned());
    }
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let cut = r.find(['.', '\'', '"', ',']).map(|i| &r[..i]).unwrap_or(&r);
        let cut = cut.trim_end_matches('/');
        if cut.len() <= "/Game/".len() {
            continue;
        }
        if !out.iter().any(|x| x == cut) {
            out.push(cut.to_string());
        }
    }
    out
}

pub fn plan_layout<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Result<Layout, NeedsReview> {
    let entries = scan_entries(zip)?;
    let files: Vec<&EntryInfo> = entries
        .iter()
        .filter(|e| !e.is_dir && !is_junk(&e.path))
        .collect();
    let uprojects: Vec<&&EntryInfo> = files
        .iter()
        .filter(|e| ext_of(&e.path) == "uproject")
        .collect();
    let uplugins: Vec<&&EntryInfo> = files
        .iter()
        .filter(|e| ext_of(&e.path) == "uplugin")
        .collect();
    if !uprojects.is_empty() {
        plan_project(&files, &uprojects)
    } else if !uplugins.is_empty() {
        plan_plugin(zip, &files, &uplugins)
    } else {
        plan_content(zip, &files)
    }
}

fn finish(
    kind: Kind,
    zip_prefix: String,
    game_prefix: String,
    allowed_roots: Vec<String>,
    renames: Vec<EntryRename>,
    total_files: usize,
    sizes: &HashMap<usize, u64>,
) -> Result<Layout, NeedsReview> {
    // Re-validate the rewritten names (case collisions, length, conflicts).
    let infos: Vec<EntryInfo> = renames
        .iter()
        .map(|r| EntryInfo {
            index: r.index,
            path: r.to.clone(),
            is_dir: false,
            size: 0,
        })
        .collect();
    check_paths(&infos)?;
    let total: u64 = renames
        .iter()
        .map(|r| sizes.get(&r.index).copied().unwrap_or(0))
        .sum();
    Ok(Layout {
        kind,
        zip_prefix,
        game_prefix,
        allowed_roots,
        entry_count: renames.len() as u64,
        total_uncompressed: total,
        dropped: total_files - renames.len(),
        renames,
        content_folder: None,
        plugin_name: None,
        engine_version: None,
        friendly_name: None,
        marketplace_url: None,
        external_refs: Vec::new(),
        unresolved_assets: 0,
    })
}

fn size_map(files: &[&EntryInfo]) -> HashMap<usize, u64> {
    files.iter().map(|e| (e.index, e.size)).collect()
}

fn plan_project(files: &[&EntryInfo], uprojects: &[&&EntryInfo]) -> Result<Layout, NeedsReview> {
    if uprojects.len() > 1 {
        return Err(NeedsReview::Ambiguous(format!(
            "multiple .uproject files: {}",
            uprojects
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let root = dir_of(&uprojects[0].path).to_string();
    let rc = comps(&root);
    let mut renames = Vec::new();
    for e in files {
        let rel = if rc.is_empty() {
            Some(e.path.split('/').collect::<Vec<_>>())
        } else {
            strip_prefix_ci(&e.path, &rc)
        };
        if let Some(r) = rel {
            renames.push(EntryRename {
                index: e.index,
                from: e.path.clone(),
                to: r.join("/"),
            });
        }
    }
    finish(
        Kind::Project,
        root,
        String::new(),
        Vec::new(),
        renames,
        files.len(),
        &size_map(files),
    )
}

fn plan_plugin<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    files: &[&EntryInfo],
    uplugins: &[&&EntryInfo],
) -> Result<Layout, NeedsReview> {
    // The root plugin is the shallowest .uplugin; every other one must live inside its tree.
    let mut sorted: Vec<&&EntryInfo> = uplugins.to_vec();
    sorted.sort_by_key(|e| e.path.matches('/').count());
    let root_entry = sorted[0];
    let root = dir_of(&root_entry.path).to_string();
    let rc = comps(&root);
    for other in &sorted[1..] {
        let inside = rc.is_empty() || strip_prefix_ci(&other.path, &rc).is_some();
        if !inside {
            return Err(NeedsReview::Ambiguous(format!(
                "multiple plugins: {}",
                uplugins
                    .iter()
                    .map(|e| e.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    // Two .uplugin at the same depth under the same root are not a single plugin.
    if sorted.len() > 1 {
        let d0 = sorted[0].path.matches('/').count();
        if sorted[1].path.matches('/').count() == d0 {
            return Err(NeedsReview::Ambiguous(format!(
                "multiple plugins: {}",
                uplugins
                    .iter()
                    .map(|e| e.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let plugin_name = stem_of(&root_entry.path).to_string();
    let (mut engine, mut friendly, mut market) = (None, None, None);
    if let Ok(bytes) = read_head(zip, root_entry.index, 1024 * 1024) {
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim_start_matches('\u{feff}');
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            engine = v
                .get("EngineVersion")
                .and_then(|x| x.as_str())
                .and_then(parse_engine_version);
            friendly = v
                .get("FriendlyName")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            market = v
                .get("MarketplaceURL")
                .and_then(|x| x.as_str())
                .map(str::to_string);
        }
    }
    let base = format!("Plugins/{plugin_name}");
    let mut renames = Vec::new();
    for e in files {
        let rel = if rc.is_empty() {
            Some(e.path.split('/').collect::<Vec<_>>())
        } else {
            strip_prefix_ci(&e.path, &rc)
        };
        let Some(rel) = rel else { continue };
        if rel.len() > 1 && DROP_PLUGIN_DIRS.contains(&rel[0].to_ascii_lowercase().as_str()) {
            continue;
        }
        renames.push(EntryRename {
            index: e.index,
            from: e.path.clone(),
            to: format!("{base}/{}", rel.join("/")),
        });
    }
    let mut l = finish(
        Kind::Plugin,
        root,
        base.clone(),
        vec![base],
        renames,
        files.len(),
        &size_map(files),
    )?;
    l.plugin_name = Some(plugin_name);
    l.engine_version = engine;
    l.friendly_name = friendly;
    l.marketplace_url = market;
    Ok(l)
}

#[derive(Clone)]
struct Rule {
    zip_prefix: String,
    game_prefix: String,
}

fn plan_content<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    files: &[&EntryInfo],
) -> Result<Layout, NeedsReview> {
    let assets: Vec<&&EntryInfo> = files
        .iter()
        .filter(|e| matches!(ext_of(&e.path).as_str(), "uasset" | "umap") && !in_ofpa(&e.path))
        .collect();
    if assets.is_empty() {
        return Err(NeedsReview::NoGameMatch);
    }
    let mut votes: HashMap<(String, String), (Rule, usize)> = HashMap::new();
    let mut unresolved = 0usize;
    let mut all_refs: BTreeSet<String> = BTreeSet::new();
    for a in &assets {
        let head = read_head(zip, a.index, HEAD_BYTES)?;
        let cands = extract_game_paths(&head);
        for c in &cands {
            all_refs.insert(c.clone());
        }
        let stem = stem_of(&a.path).to_ascii_lowercase();
        let ec: Vec<&str> = comps(dir_of(&a.path));
        let m = ec.len();
        let mut best: Option<(usize, Rule)> = None;
        let mut tie = false;
        for c in &cands {
            let parts: Vec<&str> = c.trim_start_matches("/Game/").split('/').collect();
            if parts.last().map(|s| s.to_ascii_lowercase()) != Some(stem.clone()) {
                continue;
            }
            let gc = &parts[..parts.len() - 1];
            let n = gc.len();
            let mut s = 0;
            while s < m.min(n) && ec[m - 1 - s].eq_ignore_ascii_case(gc[n - 1 - s]) {
                s += 1;
            }
            let rule = Rule {
                zip_prefix: ec[..m - s].join("/"),
                game_prefix: gc[..n - s].join("/"),
            };
            match &best {
                Some((bs, br)) if *bs > s => {
                    let _ = br;
                }
                Some((bs, br)) if *bs == s => {
                    if !(br.zip_prefix.eq_ignore_ascii_case(&rule.zip_prefix)
                        && br.game_prefix.eq_ignore_ascii_case(&rule.game_prefix))
                    {
                        tie = true;
                    }
                }
                _ => {
                    best = Some((s, rule));
                    tie = false;
                }
            }
        }
        match best {
            Some((_, rule)) if !tie => {
                let key = (
                    rule.zip_prefix.to_lowercase(),
                    rule.game_prefix.to_lowercase(),
                );
                votes.entry(key).or_insert((rule, 0)).1 += 1;
            }
            _ => unresolved += 1,
        }
    }
    let resolved: usize = votes.values().map(|v| v.1).sum();
    if resolved == 0 {
        return Err(NeedsReview::NoGameMatch);
    }
    // A clear majority rule (at least 95% of the resolved assets) wins; the few outliers
    // (an author's moved asset) count as unresolved. Any real root difference still shows
    // up later as MULTI_ROOT.
    if votes.len() > 1 {
        let (top_key, top_n) = votes
            .iter()
            .map(|(k, v)| (k.clone(), v.1))
            .max_by_key(|(_, n)| *n)
            .unwrap();
        if top_n * 20 >= resolved * 19 {
            unresolved += resolved - top_n;
            votes.retain(|k, _| *k == top_key);
        }
    }
    if unresolved * 5 > assets.len() {
        return Err(NeedsReview::TooManyUnresolved {
            unresolved,
            total: assets.len(),
        });
    }
    if votes.len() > 1 {
        let mut d: Vec<String> = votes
            .values()
            .map(|(r, n)| format!("'{}' -> '{}' ({} assets)", r.zip_prefix, r.game_prefix, n))
            .collect();
        d.sort();
        return Err(NeedsReview::RuleDisagree(d));
    }
    let rule = votes.into_values().next().unwrap().0;
    let zc = comps(&rule.zip_prefix);
    let gc = comps(&rule.game_prefix);

    let mut renames = Vec::new();
    let mut roots: BTreeSet<String> = BTreeSet::new();
    let mut ofpa_roots: BTreeSet<String> = BTreeSet::new();
    let mut root_names: HashMap<String, String> = HashMap::new();
    for e in files {
        let rel: Option<Vec<&str>> = if zc.is_empty() {
            Some(e.path.split('/').collect())
        } else {
            strip_prefix_ci(&e.path, &zc)
        };
        let Some(rel) = rel else { continue };
        let mut out: Vec<&str> = vec!["Content"];
        let ofpa = rel.first().is_some_and(|c| is_ofpa_dir(c));
        if !ofpa {
            out.extend(gc.iter().copied());
        }
        out.extend(rel.iter().copied());
        if out.len() == 2 {
            // A file directly in Content/.
            if ASSET_EXTS.contains(&ext_of(&e.path).as_str()) {
                return Err(NeedsReview::Ambiguous(format!(
                    "asset directly in Content root: {}",
                    e.path
                )));
            }
            continue;
        }
        if ofpa {
            // Kept at Content/__ExternalActors__/... (relative position under Content),
            // but never a content root.
            ofpa_roots.insert(format!("Content/{}", out[1]));
            renames.push(EntryRename {
                index: e.index,
                from: e.path.clone(),
                to: out.join("/"),
            });
            continue;
        }
        let low = out[1].to_lowercase();
        roots.insert(low.clone());
        root_names.entry(low).or_insert_with(|| out[1].to_string());
        renames.push(EntryRename {
            index: e.index,
            from: e.path.clone(),
            to: out.join("/"),
        });
    }
    if roots.is_empty() {
        return Err(NeedsReview::NoContent);
    }
    if roots.len() > 1 {
        return Err(NeedsReview::MultiRoot(
            roots.iter().map(|r| root_names[r].clone()).collect(),
        ));
    }
    let folder = root_names.values().next().unwrap().clone();
    let mut l = finish(
        Kind::Content,
        rule.zip_prefix.clone(),
        rule.game_prefix.clone(),
        {
            let mut v = vec![format!("Content/{folder}")];
            v.extend(ofpa_roots);
            v
        },
        renames,
        files.len(),
        &size_map(files),
    )?;
    let own_root = format!("/game/{}", folder.to_lowercase());
    l.external_refs = all_refs
        .into_iter()
        .filter(|r| {
            let low = r.to_lowercase();
            !(low == own_root || low.starts_with(&format!("{own_root}/")))
        })
        .take(MAX_EXTERNAL_REFS)
        .collect();
    l.content_folder = Some(folder);
    l.unresolved_assets = unresolved;
    Ok(l)
}
