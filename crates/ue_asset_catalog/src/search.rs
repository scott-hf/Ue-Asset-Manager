//! In-memory search over the catalog: a tokenised inverted index with weighted fields
//! (name > tags > folder > pack), AND of the query terms with prefix match on the last term,
//! and filters (kind, class, tags, pack, engine version, thumbnail).
//!
//! Isolated behind [`Catalog::search`] so SQLite/FTS can replace it later.

use crate::catalog::{Asset, Catalog, SourceKind};
use crate::release::Version;
use crate::tags::{self, tokenize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const F_NAME: u8 = 0;
pub const F_TAG: u8 = 1;
pub const F_FOLDER: u8 = 2;
pub const F_PACK: u8 = 3;
pub const F_DESC: u8 = 4;

const W_NAME: f32 = 10.0;
const W_TAG: f32 = 5.0;
const W_FOLDER: f32 = 2.5;
const W_PACK: f32 = 1.5;
const W_DESC: f32 = 1.0;
const PREFIX_FACTOR: f32 = 0.6;

fn weight(field: u8) -> f32 {
    match field {
        F_NAME => W_NAME,
        F_TAG => W_TAG,
        F_FOLDER => W_FOLDER,
        F_DESC => W_DESC,
        _ => W_PACK,
    }
}

/// token -> postings `(asset, field)`.
#[derive(Default)]
pub struct SearchIndex {
    map: BTreeMap<String, Vec<(u32, u8)>>,
}

impl SearchIndex {
    pub fn add(&mut self, asset: u32, field: u8, token: &str) {
        if token.is_empty() {
            return;
        }
        self.map
            .entry(token.to_lowercase())
            .or_default()
            .push((asset, field));
    }

    /// Index a tag: the whole tag, plus the words of its value for `ns:value` tags so that the
    /// plain word `normal` finds `map:normal`.
    pub fn add_tag(&mut self, asset: u32, field: u8, tag: &str) {
        self.add(asset, field, tag);
        if let Some((_, value)) = tag.split_once(':') {
            for t in tokenize(value) {
                self.add(asset, field, &t);
            }
        }
    }

    pub fn finalize(&mut self) {
        for v in self.map.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
    }

    pub fn token_count(&self) -> usize {
        self.map.len()
    }

    /// Best score per asset for one term (exact match, plus prefix matches when `prefix`).
    fn lookup(&self, term: &str, prefix: bool) -> HashMap<u32, f32> {
        let mut out: HashMap<u32, f32> = HashMap::new();
        if let Some(v) = self.map.get(term) {
            for &(a, f) in v {
                let s = weight(f);
                let e = out.entry(a).or_insert(0.0);
                if s > *e {
                    *e = s;
                }
            }
        }
        if prefix {
            for (k, v) in self.map.range(term.to_string()..) {
                if !k.starts_with(term) {
                    break;
                }
                if k == term {
                    continue;
                }
                for &(a, f) in v {
                    let s = weight(f) * PREFIX_FACTOR;
                    let e = out.entry(a).or_insert(0.0);
                    if s > *e {
                        *e = s;
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    /// Free text; `ns:value` words are tag filters (`map:normal`, `class:staticmesh`).
    pub text: String,
    /// Friendly type (`staticmesh`, `skeletalmesh`, `material`, ...; `mesh` matches both mesh
    /// types) or pack kind (`content`, `project`).
    pub kind: Option<String>,
    pub class: Option<String>,
    /// All of these tags must be present.
    pub tags: Vec<String>,
    /// Pack id (sha12 or prefix), pack slug or part of the pack name.
    pub pack: Option<String>,
    /// Pack family slugs to leave out (the `slug` of [`crate::catalog::PackFacet`] / `Family`, the
    /// identity of the UI's pack filter). Applied to results, totals and paging alike.
    pub exclude_packs: Vec<String>,
    /// Keep assets that have a release for this engine version or older.
    pub engine_version: Option<Version>,
    pub has_thumbnail: Option<bool>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for SearchQuery {
    fn default() -> Self {
        SearchQuery {
            text: String::new(),
            kind: None,
            class: None,
            tags: Vec::new(),
            pack: None,
            exclude_packs: Vec::new(),
            engine_version: None,
            has_thumbnail: None,
            limit: 20,
            offset: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub asset: usize,
    pub score: f32,
}

#[derive(Clone, Debug, Default)]
pub struct SearchOutput {
    /// Matches before `offset`/`limit`.
    pub total: usize,
    /// The page that was asked for.
    pub offset: usize,
    pub limit: usize,
    pub hits: Vec<Hit>,
    /// Hints for the caller (ignored words, unknown kind, ...).
    pub notes: Vec<String>,
}

impl SearchOutput {
    /// More results exist after this page.
    pub fn has_more(&self) -> bool {
        self.offset + self.hits.len() < self.total
    }

    /// Offset of the next page, when there is one.
    pub fn next_offset(&self) -> Option<usize> {
        self.has_more().then_some(self.offset + self.hits.len())
    }

    /// The assets of this page, in rank order.
    pub fn assets<'a>(&'a self, catalog: &'a Catalog) -> impl Iterator<Item = &'a Asset> + 'a {
        self.hits.iter().map(|h| &catalog.assets[h.asset])
    }
}

/// Split the query text into word terms and `ns:value` tag filters.
fn parse_text(text: &str) -> (Vec<String>, Vec<String>) {
    let mut terms = Vec::new();
    let mut tag_filters = Vec::new();
    for piece in text.split_whitespace() {
        if let Some((ns, val)) = piece.split_once(':') {
            if !ns.is_empty() && !val.is_empty() {
                tag_filters.push(piece.to_lowercase());
                continue;
            }
        }
        // `SM_Grass` -> `Grass`: convention prefixes are not words.
        let (_, rest) = tags::split_prefix(piece);
        for t in tokenize(rest) {
            let single = t.chars().count() < 2 && !t.chars().all(|c| c.is_ascii_digit());
            if single || tags::is_stopword(&t) {
                continue;
            }
            terms.push(t);
        }
    }
    (terms, tag_filters)
}

impl Catalog {
    /// Indexes of the families whose slug is in `slugs` (case-insensitive).
    pub fn excluded_families(&self, slugs: &[String]) -> HashSet<usize> {
        let want: HashSet<String> = slugs
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .collect();
        self.families
            .iter()
            .enumerate()
            .filter(|(_, f)| want.contains(&f.slug.to_ascii_lowercase()))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn search(&self, q: &SearchQuery) -> SearchOutput {
        let mut out = SearchOutput::default();
        let (terms, text_tags) = parse_text(&q.text);

        // Filters.
        let mut want_tags: Vec<String> = q
            .tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        want_tags.extend(text_tags);
        let kind = q.kind.as_deref().map(|k| k.trim().to_lowercase());
        let mut pack_kind: Option<&str> = None;
        let mut asset_kind: Option<String> = None;
        if let Some(k) = &kind {
            match k.as_str() {
                "content" | "project" | "plugin" => pack_kind = Some(k.as_str()),
                _ if tags::is_type_filter(k) => asset_kind = Some(tags::normalize_type(k)),
                _ => {
                    out.notes.push(format!(
                        "unknown kind `{k}`; use one of {} (asset kinds) or content, project (pack kinds)",
                        tags::TYPE_NAMES.join(", ")
                    ));
                    return out;
                }
            }
        }
        // `type:mesh` as a tag filter is the umbrella too.
        if let Some(i) = want_tags.iter().position(|t| t == "type:mesh") {
            want_tags.remove(i);
            asset_kind.get_or_insert_with(|| "mesh".to_string());
        }
        let excluded = self.excluded_families(&q.exclude_packs);
        let class = q.class.as_deref().map(|c| c.trim().to_lowercase());
        let pack_set: Option<HashSet<usize>> = q.pack.as_deref().map(|p| {
            let m: HashSet<usize> = self.match_packs(p).into_iter().collect();
            m
        });
        // Generated assets have no pack: they match by their family (`generated`, `generated-mesh`).
        let gen_families: HashSet<usize> = match q.pack.as_deref() {
            Some(p) => {
                let p = p.trim().to_ascii_lowercase();
                self.families
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| {
                        f.source == SourceKind::Generated
                            && !p.is_empty()
                            && (f.slug.contains(&p) || f.name.to_lowercase().contains(&p))
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
            None => HashSet::new(),
        };
        if let Some(s) = &pack_set {
            if s.is_empty() && gen_families.is_empty() {
                out.notes
                    .push("no pack matches the `pack` filter; see list_packs".to_string());
                return out;
            }
        }

        let passes = |a: &Asset| -> bool {
            if excluded.contains(&a.family) {
                return false;
            }
            if let Some(k) = &asset_kind {
                if !tags::type_matches(k, a.kind) {
                    return false;
                }
            }
            if let Some(k) = pack_kind {
                let fam = &self.families[a.family];
                let fk = format!("{:?}", fam.kind).to_lowercase();
                if fam.source != SourceKind::Pack || fk != k {
                    return false;
                }
            }
            if let Some(c) = &class {
                if a.class.as_deref().map(str::to_lowercase).as_deref() != Some(c.as_str()) {
                    return false;
                }
            }
            if let Some(h) = q.has_thumbnail {
                if a.has_thumbnail != h {
                    return false;
                }
            }
            for t in &want_tags {
                if !self.asset_has_tag(a, t) {
                    return false;
                }
            }
            if let Some(set) = &pack_set {
                let in_pack = a.releases.iter().any(|r| set.contains(&r.pack));
                if !in_pack && !gen_families.contains(&a.family) {
                    return false;
                }
            }
            if let Some(v) = q.engine_version {
                if a.source == SourceKind::Generated {
                    return true;
                }
                let ok = a
                    .releases
                    .iter()
                    .any(|r| match self.packs[r.pack].version_range() {
                        None => true,
                        Some((lo, _)) => lo <= v,
                    });
                if !ok {
                    return false;
                }
            }
            true
        };

        // Candidates and scores.
        let mut scored: Vec<Hit> = Vec::new();
        if terms.is_empty() {
            for (i, a) in self.assets.iter().enumerate() {
                if passes(a) {
                    scored.push(Hit {
                        asset: i,
                        score: 0.0,
                    });
                }
            }
            scored.sort_by(|a, b| {
                let (x, y) = (&self.assets[a.asset], &self.assets[b.asset]);
                x.game_path.cmp(&y.game_path)
            });
        } else {
            let last = terms.len() - 1;
            let maps: Vec<HashMap<u32, f32>> = terms
                .iter()
                .enumerate()
                .map(|(i, t)| self.search.lookup(t, i == last))
                .collect();
            let missing: Vec<&String> = terms
                .iter()
                .zip(&maps)
                .filter(|(_, m)| m.is_empty())
                .map(|(t, _)| t)
                .collect();
            let strict = intersect(&maps.iter().collect::<Vec<_>>());
            let (cands, used): (HashMap<u32, f32>, Vec<usize>) =
                if !strict.is_empty() || missing.is_empty() || missing.len() == terms.len() {
                    (strict, (0..terms.len()).collect())
                } else {
                    // Unknown words (`sm`, typos) must not zero the result: retry without them.
                    let keep: Vec<usize> =
                        (0..terms.len()).filter(|i| !maps[*i].is_empty()).collect();
                    let loose = intersect(&keep.iter().map(|&i| &maps[i]).collect::<Vec<_>>());
                    if !loose.is_empty() {
                        out.notes.push(format!(
                            "ignored words with no match: {}",
                            missing
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    (loose, keep)
                };
            let used_terms: Vec<String> = used.iter().map(|&i| terms[i].clone()).collect();
            for (ai, base) in cands {
                let a = &self.assets[ai as usize];
                if !passes(a) {
                    continue;
                }
                let mut score = base;
                let (nt, adj) = if a.source == SourceKind::Generated {
                    (
                        crate::generated::words(&a.name),
                        tags::rank_adjust(a.kind, a.class.as_deref(), &[], &[]),
                    )
                } else {
                    let t = tags::tag_asset(&a.game_path, a.class.as_deref());
                    let adj = tags::rank_adjust(
                        a.kind,
                        a.class.as_deref(),
                        &t.name_tokens,
                        &t.folder_tokens,
                    );
                    (t.name_tokens, adj)
                };
                score += adj;
                if nt == used_terms {
                    score += 20.0;
                } else if nt.starts_with(&used_terms) {
                    score += 6.0;
                }
                score += 2.0 / (nt.len().max(1) as f32);
                scored.push(Hit {
                    asset: ai as usize,
                    score,
                });
            }
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        let (x, y) = (&self.assets[a.asset], &self.assets[b.asset]);
                        x.name
                            .len()
                            .cmp(&y.name.len())
                            .then_with(|| x.game_path.cmp(&y.game_path))
                    })
            });
        }
        out.total = scored.len();
        let limit = q.limit.clamp(1, 100);
        out.offset = q.offset;
        out.limit = limit;
        out.hits = scored.into_iter().skip(q.offset).take(limit).collect();
        out
    }
}

/// Intersection of the per-term maps; the score of an asset is the sum over terms.
fn intersect(maps: &[&HashMap<u32, f32>]) -> HashMap<u32, f32> {
    let Some((first, rest)) = maps.split_first() else {
        return HashMap::new();
    };
    let mut out: HashMap<u32, f32> = HashMap::new();
    'outer: for (&a, &s) in first.iter() {
        let mut total = s;
        for m in rest {
            match m.get(&a) {
                Some(x) => total += x,
                None => continue 'outer,
            }
        }
        out.insert(a, total);
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::catalog::{Catalog, IndexOutcome};
    use crate::search::SearchQuery;
    use crate::testutil::{entry, index, pkg};
    use ue_asset_library::Kind;

    fn catalog() -> Catalog {
        let nature = entry(
            "Nature Pack",
            "aaaaaaaaaaaa0000",
            Some("5.3"),
            Kind::Content,
        );
        let nature55 = entry(
            "Nature Pack",
            "bbbbbbbbbbbb0000",
            Some("5.5"),
            Kind::Content,
        );
        let kit = entry(
            "Medieval Castle Kit",
            "cccccccccccc0000",
            Some("5.4"),
            Kind::Content,
        );
        let mut tex = pkg("/Game/Rock/T_Rock_N", "Texture2D", &[]);
        tex.has_thumbnail = true;
        Catalog::from_entries(vec![
            (
                nature,
                IndexOutcome::Indexed(index(
                    "aaaaaaaaaaaa0000",
                    vec![
                        pkg("/Game/Foliage/SM_Grass_01", "StaticMesh", &[]),
                        pkg("/Game/Foliage/SM_Grass_02", "StaticMesh", &[]),
                        pkg("/Game/Foliage/SM_GrassPatchLarge", "StaticMesh", &[]),
                        pkg("/Game/Rock/SM_Rock_Big", "StaticMesh", &[]),
                        tex,
                        pkg("/Game/Rock/M_Rock", "Material", &[]),
                        pkg("/Game/Misc/BP_Spawner", "Blueprint", &[]),
                    ],
                )),
            ),
            (
                nature55,
                IndexOutcome::Indexed(index(
                    "bbbbbbbbbbbb0000",
                    vec![
                        pkg("/Game/Foliage/SM_Grass_01", "StaticMesh", &[]),
                        pkg("/Game/Foliage/SM_Bush", "StaticMesh", &[]),
                    ],
                )),
            ),
            (
                kit,
                IndexOutcome::Indexed(index(
                    "cccccccccccc0000",
                    vec![
                        pkg("/Game/Castle/SM_Door", "StaticMesh", &[]),
                        pkg("/Game/Castle/SM_Door_Frame", "StaticMesh", &[]),
                        pkg("/Game/Castle/Doors/SM_Gate", "StaticMesh", &[]),
                    ],
                )),
            ),
        ])
    }

    fn names(c: &Catalog, q: &SearchQuery) -> Vec<String> {
        c.search(q)
            .hits
            .iter()
            .map(|h| c.assets[h.asset].name.clone())
            .collect()
    }

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            ..SearchQuery::default()
        }
    }

    #[test]
    fn name_beats_tags_and_exact_name_first() {
        let c = catalog();
        let n = names(&c, &q("door"));
        // Exact name SM_Door first, then SM_Door_Frame (name starts with door), then the gate
        // (found through the synonym tag + folder word, lower weight).
        assert_eq!(n[0], "SM_Door");
        assert_eq!(n[1], "SM_Door_Frame");
        assert!(n.contains(&"SM_Gate".to_string()));
    }

    #[test]
    fn synonym_tags_find_assets() {
        let c = catalog();
        let n = names(&c, &q("foliage"));
        // Grass assets: tag `foliage` through the synonym and the Foliage folder.
        assert!(n.iter().any(|x| x == "SM_Grass_01"));
        let n = names(&c, &q("stone"));
        assert!(n.iter().any(|x| x == "SM_Rock_Big"));
    }

    #[test]
    fn last_term_is_a_prefix_and_terms_are_anded() {
        let c = catalog();
        let n = names(&c, &q("grass pat"));
        assert_eq!(n, ["SM_GrassPatchLarge"]);
        // Non-last terms are exact: `gra` alone is a prefix; in `gra rock` it matches no word,
        // so it is ignored (with a note) instead of zeroing the result.
        assert!(!names(&c, &q("gra")).is_empty());
        let out = c.search(&q("gra rock"));
        assert!(out.total > 0);
        assert!(out.notes.iter().any(|n| n.contains("gra")));
    }

    #[test]
    fn unknown_words_are_dropped_with_a_note() {
        let c = catalog();
        let out = c.search(&q("zzzz grass"));
        assert!(out.total >= 3);
        assert!(out.notes.iter().any(|n| n.contains("zzzz")));
        // Prefixed convention names work as queries.
        assert!(names(&c, &q("SM_Grass")).len() >= 3);
    }

    #[test]
    fn filters_kind_class_tag_pack_engine_thumbnail() {
        let c = catalog();
        assert_eq!(
            names(
                &c,
                &SearchQuery {
                    kind: Some("material".into()),
                    ..q("rock")
                }
            ),
            ["M_Rock"]
        );
        assert_eq!(
            names(
                &c,
                &SearchQuery {
                    kind: Some("texture".into()),
                    ..q("")
                }
            ),
            ["T_Rock_N"]
        );
        assert_eq!(
            names(
                &c,
                &SearchQuery {
                    class: Some("blueprint".into()),
                    ..q("")
                }
            ),
            ["BP_Spawner"]
        );
        // Tag filters: via the tags parameter and via ns:value in the text.
        assert_eq!(
            names(
                &c,
                &SearchQuery {
                    tags: vec!["map:normal".into()],
                    ..q("")
                }
            ),
            ["T_Rock_N"]
        );
        assert_eq!(names(&c, &q("rock map:normal")), ["T_Rock_N"]);
        // Pack by slug fragment, by id prefix and by name.
        assert_eq!(
            c.search(&SearchQuery {
                pack: Some("castle".into()),
                ..q("")
            })
            .total,
            3
        );
        assert_eq!(
            c.search(&SearchQuery {
                pack: Some("cccc".into()),
                ..q("")
            })
            .total,
            3
        );
        assert_eq!(
            c.search(&SearchQuery {
                pack: Some("nope".into()),
                ..q("")
            })
            .total,
            0
        );
        // Pack kind through `kind`.
        assert_eq!(
            c.search(&SearchQuery {
                kind: Some("project".into()),
                ..q("")
            })
            .total,
            0
        );
        // Engine <= 5.4: Nature 5.5 only assets (SM_Bush) drop out, the rest have a 5.3 release.
        let e54 = SearchQuery {
            engine_version: Some((5, 4)),
            ..q("")
        };
        let n = names(&c, &e54);
        assert!(!n.contains(&"SM_Bush".to_string()));
        assert!(n.contains(&"SM_Grass_01".to_string()));
        // Thumbnails.
        let th = SearchQuery {
            has_thumbnail: Some(true),
            ..q("")
        };
        assert_eq!(names(&c, &th), ["T_Rock_N"]);
        // Unknown kind explained.
        let out = c.search(&SearchQuery {
            kind: Some("banana".into()),
            ..q("")
        });
        assert_eq!(out.total, 0);
        assert!(!out.notes.is_empty());
    }

    #[test]
    fn useful_kinds_beat_level_blueprints_and_demo_content_on_name_ties() {
        let e = entry("Env Pack", "dddddddddddd0000", Some("5.3"), Kind::Content);
        let c = Catalog::from_entries(vec![(
            e,
            IndexOutcome::Indexed(index(
                "dddddddddddd0000",
                vec![
                    pkg("/Game/ETC/Maps/Grass1", "BlueprintGeneratedClass", &[]),
                    pkg("/Game/ETC/Maps/Grass2", "BlueprintGeneratedClass", &[]),
                    pkg("/Game/Demo/BP_Grass", "Blueprint", &[]),
                    pkg(
                        "/Game/Env/Materials/MI_Grass",
                        "MaterialInstanceConstant",
                        &[],
                    ),
                    pkg("/Game/Env/Meshes/SM_Grass", "StaticMesh", &[]),
                    pkg("/Game/Env/Meshes/SM_Grass_Example", "StaticMesh", &[]),
                    pkg("/Game/Env/Textures/T_Grass", "Texture2D", &[]),
                ],
            )),
        )]);
        let n = names(&c, &q("grass"));
        let pos = |x: &str| n.iter().position(|y| y == x).unwrap();
        for good in ["SM_Grass", "MI_Grass", "T_Grass"] {
            for bad in ["Grass1", "Grass2", "BP_Grass"] {
                assert!(pos(good) < pos(bad), "{good} before {bad}: {n:?}");
            }
        }
        assert!(pos("SM_Grass") < pos("SM_Grass_Example"), "{n:?}");
        // Page helpers.
        let out = c.search(&SearchQuery {
            limit: 2,
            ..q("grass")
        });
        assert_eq!((out.total, out.offset, out.limit), (n.len(), 0, 2));
        assert!(out.has_more());
        assert_eq!(out.next_offset(), Some(2));
        assert_eq!(out.assets(&c).count(), 2);
    }

    #[test]
    fn exclude_packs_keeps_totals_and_paging_right() {
        let c = catalog();
        let all = c.search(&SearchQuery {
            limit: 100,
            ..q("")
        });
        let ex = c.search(&SearchQuery {
            exclude_packs: vec!["Medieval-Castle-Kit".into()],
            limit: 2,
            ..q("")
        });
        assert_eq!(ex.total, all.total - 3);
        assert_eq!(ex.hits.len(), 2);
        assert!(ex.assets(&c).all(|a| !a.game_path.contains("Castle")));
        assert!(
            c.search(&SearchQuery {
                exclude_packs: vec!["medieval-castle-kit".into()],
                ..q("door")
            })
            .total
                == 0
        );
        // Unknown slugs exclude nothing.
        assert_eq!(
            c.search(&SearchQuery {
                exclude_packs: vec!["nope".into()],
                limit: 100,
                ..q("")
            })
            .total,
            all.total
        );
        // Facets agree.
        let f = c.facets_excluding(&["medieval-castle-kit".to_string()]);
        assert_eq!(f.kinds.values().sum::<usize>(), ex.total);
        assert!(f.packs.iter().all(|p| p.slug != "medieval-castle-kit"));
    }

    #[test]
    fn mesh_umbrella_and_split_types() {
        let e = entry("Chars", "eeeeeeeeeeee0000", Some("5.3"), Kind::Content);
        let c = Catalog::from_entries(vec![(
            e,
            IndexOutcome::Indexed(index(
                "eeeeeeeeeeee0000",
                vec![
                    pkg("/Game/C/SM_Hat", "StaticMesh", &[]),
                    pkg("/Game/C/SKM_Hero", "SkeletalMesh", &[]),
                    pkg("/Game/C/M_Skin", "Material", &[]),
                ],
            )),
        )]);
        let n = |k: &str| {
            names(
                &c,
                &SearchQuery {
                    kind: Some(k.into()),
                    ..q("")
                },
            )
        };
        assert_eq!(n("staticmesh"), ["SM_Hat"]);
        assert_eq!(n("skeletalmesh"), ["SKM_Hero"]);
        assert_eq!(n("Skeletal Mesh"), ["SKM_Hero"]);
        assert_eq!(n("mesh").len(), 2);
        assert_eq!(
            names(
                &c,
                &SearchQuery {
                    tags: vec!["type:mesh".into()],
                    ..q("")
                }
            )
            .len(),
            2
        );
        assert_eq!(names(&c, &q("type:skeletalmesh")), ["SKM_Hero"]);
        let f = c.facets();
        assert_eq!((f.kinds["staticmesh"], f.kinds["skeletalmesh"]), (1, 1));
        assert!(!f.kinds.contains_key("mesh"));
    }

    #[test]
    fn paging_and_determinism() {
        let c = catalog();
        let all = names(
            &c,
            &SearchQuery {
                limit: 100,
                ..q("")
            },
        );
        assert_eq!(all.len(), c.assets.len());
        let first = names(&c, &SearchQuery { limit: 2, ..q("") });
        let second = names(
            &c,
            &SearchQuery {
                limit: 2,
                offset: 2,
                ..q("")
            },
        );
        assert_eq!(first, all[..2]);
        assert_eq!(second, all[2..4]);
        assert_eq!(names(&c, &q("grass")), names(&c, &q("grass")));
    }

    #[test]
    fn family_dedupe_lists_an_asset_once() {
        let c = catalog();
        let n = names(&c, &q("grass 01"));
        assert_eq!(n.iter().filter(|x| x.as_str() == "SM_Grass_01").count(), 1);
    }
}
