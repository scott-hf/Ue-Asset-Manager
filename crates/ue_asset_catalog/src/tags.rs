//! Offline, deterministic auto-tagger. Tags are computed at catalog load from a package's game
//! path and class plus its pack's metadata, never stored in the index, so improving the tables
//! here needs no re-index.
//!
//! Tag shapes:
//! * plain words: name tokens (`grass`), folder words, synonyms (`foliage`, `plant`)
//! * `class:staticmesh` (the exported class), `type:staticmesh` / `type:skeletalmesh` (friendly type, see [`TYPE_NAMES`])
//! * `map:normal` (texture map from the name suffix), `lod`, `variant:03`
//! * pack level: `pack:<slug>`, `kind:content|project`, `engine:5.3`, plus the pack-name words
//!
//! Hook point for later work (embeddings, image tags, LLM tagging): add another function that
//! returns extra tags and merge it where [`tag_asset`] is called in `catalog.rs`.

use ue_asset_library::Kind;

/// The friendly asset types in display order: the values of the `kind` search filter and of the
/// `type:` tags. The umbrella value `mesh` is also accepted as a filter (both mesh types).
pub const TYPE_NAMES: &[&str] = &[
    "staticmesh",
    "skeletalmesh",
    "material",
    "texture",
    "blueprint",
    "niagara",
    "animation",
    "sound",
    "level",
    "other",
];

/// Same as [`TYPE_NAMES`] (kept for older callers).
pub const KINDS: &[&str] = TYPE_NAMES;

/// Does an asset of type `kind` pass a `kind` filter value? `mesh` matches both mesh types;
/// spaces, `_` and `-` are ignored (`static mesh`).
pub fn type_matches(filter: &str, kind: &str) -> bool {
    let f = normalize_type(filter);
    f == kind || (f == "mesh" && (kind == "staticmesh" || kind == "skeletalmesh"))
}

/// Lower-case type name without separators.
pub fn normalize_type(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect()
}

/// Is this a known `kind` filter value (a type name or the `mesh` umbrella)?
pub fn is_type_filter(s: &str) -> bool {
    let n = normalize_type(s);
    n == "mesh" || TYPE_NAMES.contains(&n.as_str())
}

/// UE naming-convention prefixes: (prefix without `_`, kind hint when the class is unknown).
/// Prefixes with an empty hint are ambiguous (`A_` is a sound or an animation) and only stripped.
const PREFIXES: &[(&str, &str)] = &[
    ("SM", "staticmesh"),
    ("SK", "skeletalmesh"),
    ("SKM", "skeletalmesh"),
    ("SKEL", "animation"),
    ("T", "texture"),
    ("TX", "texture"),
    ("TEX", "texture"),
    ("RT", "texture"),
    ("TC", "texture"),
    ("M", "material"),
    ("MI", "material"),
    ("MF", "material"),
    ("MM", "material"),
    ("MPC", "material"),
    ("ML", "material"),
    ("BP", "blueprint"),
    ("WBP", "blueprint"),
    ("ABP", "blueprint"),
    ("EUW", "blueprint"),
    ("NS", "niagara"),
    ("NE", "niagara"),
    ("FX", "niagara"),
    ("PS", "niagara"),
    ("AS", "animation"),
    ("AM", "animation"),
    ("BS", "animation"),
    ("AO", "animation"),
    ("SW", "sound"),
    ("SC", "sound"),
    ("MS", "sound"),
    ("A", ""),
    ("S", ""),
    ("P", ""),
    ("DT", ""),
    ("E", ""),
    ("F", ""),
    ("L", "level"),
    ("LV", "level"),
    ("LS", ""),
    ("ST", ""),
    ("PD", ""),
    ("CR", "animation"),
    ("IK", "animation"),
];

/// Texture-map suffixes (lower case) -> map name. `true` = needs a texture context because the
/// suffix is a bare letter or otherwise ambiguous (`_D`, `_N`, `_R`, ...).
const MAP_SUFFIXES: &[(&str, &str, bool)] = &[
    ("bc", "basecolor", true),
    ("basecolor", "basecolor", false),
    ("basecolour", "basecolor", false),
    ("albedo", "basecolor", false),
    ("diffuse", "basecolor", false),
    ("diff", "basecolor", false),
    ("color", "basecolor", true),
    ("col", "basecolor", true),
    ("d", "basecolor", true),
    ("n", "normal", true),
    ("nm", "normal", true),
    ("nrm", "normal", false),
    ("norm", "normal", false),
    ("normal", "normal", false),
    ("normalgl", "normal", false),
    ("normaldx", "normal", false),
    ("r", "roughness", true),
    ("rough", "roughness", false),
    ("roughness", "roughness", false),
    ("m", "metallic", true),
    ("met", "metallic", true),
    ("metal", "metallic", true),
    ("metallic", "metallic", false),
    ("metalness", "metallic", false),
    ("orm", "orm", false),
    ("arm", "orm", true),
    ("rma", "orm", false),
    ("mra", "orm", false),
    ("ao", "ao", true),
    ("occlusion", "ao", false),
    ("ambientocclusion", "ao", false),
    ("h", "height", true),
    ("height", "height", false),
    ("disp", "height", false),
    ("displacement", "height", false),
    ("bump", "height", true),
    ("e", "emissive", true),
    ("emissive", "emissive", false),
    ("emission", "emissive", false),
    ("s", "specular", true),
    ("spec", "specular", false),
    ("specular", "specular", false),
    ("mask", "mask", false),
    ("opacity", "opacity", false),
    ("alpha", "opacity", true),
    ("o", "opacity", true),
];

/// Small data-driven synonym table: when a name or folder word is in a group, every other word
/// of the group is added as a tag. One hop only (no transitive expansion).
const SYNONYMS: &[&[&str]] = &[
    &["grass", "foliage", "plant", "vegetation"],
    &["tree", "foliage", "plant", "vegetation"],
    &["bush", "foliage", "plant", "shrub"],
    &["shrub", "bush", "foliage", "plant"],
    &["flower", "foliage", "plant"],
    &["fern", "foliage", "plant"],
    &["rock", "stone", "boulder"],
    &["stone", "rock"],
    &["boulder", "rock", "stone"],
    &["cliff", "rock", "stone"],
    &["door", "gate", "entrance"],
    &["gate", "door", "entrance"],
    &["wall", "barrier"],
    &["fence", "barrier"],
    &["crate", "box", "container"],
    &["box", "crate", "container"],
    &["barrel", "container", "drum"],
    &["chair", "seat", "furniture"],
    &["stool", "seat", "furniture"],
    &["sofa", "couch", "seat", "furniture"],
    &["couch", "sofa", "seat", "furniture"],
    &["table", "desk", "furniture"],
    &["desk", "table", "furniture"],
    &["bed", "furniture"],
    &["shelf", "furniture", "storage"],
    &["car", "vehicle", "automobile"],
    &["truck", "vehicle", "lorry"],
    &["bike", "bicycle", "vehicle"],
    &["bicycle", "bike", "vehicle"],
    &["gun", "weapon", "firearm"],
    &["rifle", "gun", "weapon", "firearm"],
    &["pistol", "gun", "weapon", "firearm"],
    &["sword", "blade", "weapon"],
    &["axe", "weapon"],
    &["house", "building", "home"],
    &["building", "architecture", "structure"],
    &["floor", "ground", "tile"],
    &["ground", "terrain", "soil"],
    &["dirt", "soil", "ground"],
    &["mud", "dirt", "soil"],
    &["sand", "desert", "ground"],
    &["water", "liquid"],
    &["river", "water", "stream"],
    &["fire", "flame", "burn"],
    &["flame", "fire"],
    &["lamp", "light", "lantern"],
    &["lantern", "lamp", "light"],
    &["torch", "light", "fire"],
    &["metal", "steel", "iron"],
    &["steel", "metal"],
    &["iron", "metal"],
    &["wood", "wooden", "timber"],
    &["wooden", "wood"],
    &["plank", "wood", "timber"],
    &["concrete", "cement"],
    &["brick", "masonry"],
    &["fabric", "cloth", "textile"],
    &["cloth", "fabric", "textile"],
    &["zombie", "undead", "monster"],
    &["skeleton", "undead", "bones"],
    &["character", "humanoid", "person"],
    &["human", "character", "humanoid", "person"],
    &["scifi", "futuristic", "sci-fi"],
    &["sci", "scifi", "futuristic"],
    &["medieval", "fantasy", "historical"],
    &["road", "street", "asphalt"],
    &["street", "road", "city"],
    &["window", "glass"],
    &["glass", "transparent"],
    &["snow", "ice", "winter"],
    &["ice", "snow", "winter"],
    &["smoke", "vfx", "particle"],
    &["explosion", "vfx", "particle", "blast"],
    &["sparks", "vfx", "particle"],
];

const STOPWORDS: &[&str] = &[
    "the", "and", "of", "a", "an", "for", "with", "by", "in", "on", "to", "at", "new", "old",
    "copy", "test", "final", "default",
];

/// Folder words that say nothing (they duplicate the class or the mount).
const GENERIC_FOLDERS: &[&str] = &[
    "game",
    "content",
    "meshes",
    "mesh",
    "textures",
    "texture",
    "materials",
    "material",
    "blueprints",
    "blueprint",
    "assets",
    "asset",
    "common",
    "shared",
    "resources",
    "data",
    "misc",
];

pub fn is_stopword(w: &str) -> bool {
    STOPWORDS.contains(&w)
}

/// Split text into lower-case word tokens: `SM_GrassClump03` -> `[sm, grass, clump, 03]`.
///
/// Splits on punctuation, camelCase, acronym ends (`HTTPServer` -> `http`, `server`) and
/// letter-to-digit boundaries; keeps digit-to-letter runs together (`4k`, `8bit`).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for piece in text.split(|c: char| !c.is_alphanumeric()) {
        if piece.is_empty() {
            continue;
        }
        let chars: Vec<char> = piece.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                let boundary = (prev.is_lowercase() && c.is_uppercase())
                    || (prev.is_alphabetic() && c.is_ascii_digit())
                    || (prev.is_uppercase() && c.is_uppercase() && next_lower);
                if boundary && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur).to_lowercase());
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur.to_lowercase());
        }
    }
    out
}

/// `rocks` -> `rock` (not `glass`, `bus`, `mass`).
pub fn singular(t: &str) -> Option<String> {
    if t.len() > 3
        && t.ends_with('s')
        && !t.ends_with("ss")
        && !t.ends_with("us")
        && !t.ends_with("is")
    {
        Some(t[..t.len() - 1].to_string())
    } else {
        None
    }
}

/// Split a UE convention prefix off an asset name: `SM_Rock_01` -> (`Some("SM")`, `Rock_01`).
/// Only upper-case prefixes from the table count, and something must remain.
pub fn split_prefix(name: &str) -> (Option<&str>, &str) {
    if let Some((p, rest)) = name.split_once('_') {
        if !rest.is_empty() && PREFIXES.iter().any(|(k, _)| *k == p) {
            return (Some(p), rest);
        }
    }
    (None, name)
}

/// Friendly type of a class (`StaticMesh` -> `staticmesh`, `SkeletalMesh` -> `skeletalmesh`).
pub fn kind_of_class(class: &str) -> &'static str {
    let c = class.to_ascii_lowercase();
    match c.as_str() {
        "staticmesh" | "destructiblemesh" | "nanitestaticmesh" => "staticmesh",
        "skeletalmesh" | "skinnedasset" | "skinnedmeshcomponent" | "skeletalmeshlod" => {
            "skeletalmesh"
        }
        "material"
        | "materialinstanceconstant"
        | "materialinstancedynamic"
        | "materialfunction"
        | "materialfunctioninstance"
        | "materialfunctionmateriallayer"
        | "materialfunctionmateriallayerblend"
        | "materialparametercollection"
        | "substrategraphmaterial" => "material",
        "texture2d"
        | "texturecube"
        | "texture2darray"
        | "volumetexture"
        | "texturerendertarget2d"
        | "texturerendertargetcube"
        | "mediatexture"
        | "lightmasstexture2d"
        | "texturecubearray"
        | "virtualtexture"
        | "runtimevirtualtexture"
        | "canvasrendertarget2d" => "texture",
        "blueprint"
        | "widgetblueprint"
        | "animblueprint"
        | "blueprintgeneratedclass"
        | "editorutilityblueprint"
        | "editorutilitywidgetblueprint"
        | "blueprintfunctionlibrary"
        | "userdefinedstruct"
        | "userdefinedenum" => "blueprint",
        "niagarasystem"
        | "niagaraemitter"
        | "niagarascript"
        | "niagaraparametercollection"
        | "niagaraeffecttype"
        | "particlesystem"
        | "niagaradatainterface" => "niagara",
        "animsequence"
        | "animmontage"
        | "blendspace"
        | "blendspace1d"
        | "aimoffsetblendspace"
        | "aimoffsetblendspace1d"
        | "animcomposite"
        | "poseasset"
        | "skeleton"
        | "controlrig"
        | "controlrigblueprint"
        | "ikrigdefinition"
        | "ikretargeter"
        | "animationasset"
        | "levelsequence"
        | "mlpose" => "animation",
        "soundwave" | "soundcue" | "metasoundsource" | "metasoundpatch" | "soundattenuation"
        | "soundclass" | "soundmix" | "soundclassbase" | "soundconcurrency" | "dialoguewave"
        | "reverbeffect" | "soundsubmix" => "sound",
        "world" | "level" | "levelstreaming" | "worldpartitionminimap" => "level",
        // Variants and proxies: skeletal only when the class says so; mesh-ish names default to
        // static. GeometryCollection, groom and physics assets stay `other`.
        _ if c.contains("skeletalmesh") || c.contains("skinned") => "skeletalmesh",
        _ if c.contains("staticmesh") => "staticmesh",
        _ => "other",
    }
}

/// Friendly kind from the class, else from the prefix hint, else `other`.
pub fn asset_kind(class: Option<&str>, prefix: Option<&str>) -> &'static str {
    if let Some(c) = class {
        let k = kind_of_class(c);
        if k != "other" {
            return k;
        }
        // A class we do not know: a convention prefix may still say more.
        if let Some(h) = prefix.and_then(prefix_hint) {
            return h;
        }
        return "other";
    }
    prefix.and_then(prefix_hint).unwrap_or("other")
}

fn prefix_hint(prefix: &str) -> Option<&'static str> {
    PREFIXES
        .iter()
        .find(|(k, _)| *k == prefix)
        .map(|(_, h)| *h)
        .filter(|h| !h.is_empty())
}

/// Synonym tags for a set of word tokens (one hop through the table).
pub fn synonyms_for(tokens: &[String]) -> Vec<String> {
    let mut extra: Vec<String> = Vec::new();
    for t in tokens {
        let base = singular(t).unwrap_or_else(|| t.clone());
        for group in SYNONYMS {
            if group[0] == base || group[0] == t.as_str() {
                extra.extend(group[1..].iter().map(|s| (*s).to_string()));
            }
        }
    }
    extra
}

/// Ranking bonus per friendly kind: assets a person usually wants first.
const KIND_BONUS: &[(&str, f32)] = &[
    ("staticmesh", 4.0),
    ("skeletalmesh", 4.0),
    ("material", 4.0),
    ("texture", 3.0),
    ("niagara", 3.0),
    ("sound", 3.0),
    ("animation", 3.0),
    ("other", 0.0),
    ("blueprint", -2.0),
    ("level", -4.0),
];

/// Classes that are generic containers rather than the asset itself.
const CLASS_PENALTY: &[(&str, f32)] = &[("blueprintgeneratedclass", -2.0)];

/// Name words that mark demo/test content (applied to the asset name).
const DEMO_NAME_WORDS: &[&str] = &["example", "demo", "test", "sample", "tutorial"];

/// Folder words that mark maps and demo content.
const DEMO_FOLDER_WORDS: &[&str] = &[
    "maps", "map", "levels", "demo", "demos", "example", "examples", "test", "tests", "sample",
    "samples", "tutorial",
];

const DEMO_PENALTY: f32 = -4.0;

/// Score adjustment for ties between equally good name matches: positive for useful kinds,
/// negative for level blueprints, generic classes and demo/test/map content.
pub fn rank_adjust(
    kind: &str,
    class: Option<&str>,
    name_tokens: &[String],
    folder_tokens: &[String],
) -> f32 {
    let mut adj = KIND_BONUS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(0.0, |(_, b)| *b);
    if let Some(c) = class {
        let c = c.to_ascii_lowercase();
        adj += CLASS_PENALTY
            .iter()
            .find(|(k, _)| *k == c)
            .map_or(0.0, |(_, p)| *p);
    }
    if name_tokens
        .iter()
        .any(|t| DEMO_NAME_WORDS.contains(&t.as_str()))
    {
        adj += DEMO_PENALTY;
    }
    if folder_tokens
        .iter()
        .any(|t| DEMO_FOLDER_WORDS.contains(&t.as_str()))
    {
        adj += DEMO_PENALTY;
    }
    adj
}

/// Everything the tagger knows about one asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetTags {
    pub kind: &'static str,
    /// Sorted, de-duplicated asset-level tags.
    pub tags: Vec<String>,
    /// Word tokens of the name (prefix, map suffix and variant number removed).
    pub name_tokens: Vec<String>,
    /// Word tokens of the folders.
    pub folder_tokens: Vec<String>,
}

/// The asset name part of a game path (`/Game/A/SM_B` -> `SM_B`).
pub fn name_of(game_path: &str) -> &str {
    game_path.rsplit('/').next().unwrap_or(game_path)
}

/// Name tokens only, same rules as [`tag_asset`] (used for ranking).
pub fn name_tokens(game_path: &str, class: Option<&str>) -> Vec<String> {
    tag_asset(game_path, class).name_tokens
}

/// Tag one asset from its game path and exported class.
pub fn tag_asset(game_path: &str, class: Option<&str>) -> AssetTags {
    let comps: Vec<&str> = game_path.split('/').filter(|c| !c.is_empty()).collect();
    let name = comps.last().copied().unwrap_or(game_path);
    let (prefix, stem) = split_prefix(name);
    let kind = asset_kind(class, prefix);
    let texture_ctx = kind == "texture";

    let mut tags: Vec<String> = Vec::new();
    if let Some(c) = class {
        let c = c.trim();
        if !c.is_empty() {
            tags.push(format!("class:{}", c.to_ascii_lowercase()));
        }
    }
    tags.push(format!("type:{kind}"));

    // Name: peel off the map suffix and the variant, token the rest.
    let mut segs: Vec<&str> = stem.split('_').filter(|s| !s.is_empty()).collect();
    if segs.len() > 1 {
        let last = segs[segs.len() - 1].to_ascii_lowercase();
        if let Some((_, map, needs_ctx)) = MAP_SUFFIXES.iter().find(|(k, _, _)| *k == last) {
            if !*needs_ctx || texture_ctx {
                tags.push(format!("map:{map}"));
                segs.pop();
            }
        }
    }
    // Lettered variants `Rock_A` (not for textures, where a bare letter is a map).
    if segs.len() > 1 && !texture_ctx {
        let last = segs[segs.len() - 1];
        if last.len() == 1 && last.chars().all(|c| c.is_ascii_uppercase()) {
            tags.push(format!("variant:{}", last.to_ascii_lowercase()));
            segs.pop();
        }
    }
    let raw = tokenize(&segs.join("_"));
    let mut name_tokens: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let t = &raw[i];
        let digits = t.chars().all(|c| c.is_ascii_digit());
        if t == "lod" {
            tags.push("lod".to_string());
            if raw
                .get(i + 1)
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
            {
                i += 1;
            }
        } else if digits {
            // A trailing number is a variant number, others are dropped.
            if i + 1 == raw.len() && t.len() <= 4 && !name_tokens.is_empty() {
                tags.push(format!("variant:{t}"));
            }
        } else if t.chars().count() >= 2 && !is_stopword(t) {
            name_tokens.push(t.clone());
        }
        i += 1;
    }
    for t in &name_tokens {
        tags.push(t.clone());
        if let Some(s) = singular(t) {
            tags.push(s);
        }
    }

    // Folders: everything before the name, minus the `Game` mount and generic words.
    let mut folder_tokens: Vec<String> = Vec::new();
    if comps.len() > 1 {
        for dir in &comps[..comps.len() - 1] {
            for t in tokenize(dir) {
                if t.chars().count() < 2
                    || t.chars().all(|c| c.is_ascii_digit())
                    || is_stopword(&t)
                    || GENERIC_FOLDERS.contains(&t.as_str())
                {
                    continue;
                }
                if !folder_tokens.contains(&t) {
                    folder_tokens.push(t);
                }
            }
        }
    }
    for t in &folder_tokens {
        tags.push(t.clone());
        if let Some(s) = singular(t) {
            tags.push(s);
        }
    }

    // Synonyms from name + folder words (one hop).
    let all: Vec<String> = name_tokens
        .iter()
        .chain(folder_tokens.iter())
        .cloned()
        .collect();
    tags.extend(synonyms_for(&all));

    tags.sort();
    tags.dedup();
    AssetTags {
        kind,
        tags,
        name_tokens,
        folder_tokens,
    }
}

/// Pack-level tags: `pack:<slug>`, `kind:content|project`, `engine:5.3`, and the pack-name words.
pub fn tag_pack(display_name: &str, slug: &str, kind: Kind, engines: &[String]) -> Vec<String> {
    let mut tags = vec![
        format!("pack:{slug}"),
        format!(
            "kind:{}",
            match kind {
                Kind::Content => "content",
                Kind::Plugin => "plugin",
                Kind::Project => "project",
            }
        ),
    ];
    for e in engines {
        tags.push(format!("engine:{e}"));
    }
    for t in tokenize(display_name) {
        if t.chars().count() >= 2 && !is_stopword(&t) && !t.chars().all(|c| c.is_ascii_digit()) {
            tags.push(t.clone());
            if let Some(s) = singular(&t) {
                tags.push(s);
            }
        }
    }
    tags.sort();
    tags.dedup();
    tags
}

/// Lower-case slug of a name: alphanumerics, other runs become one `-`.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "pack".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(tags: &[String], t: &str) -> bool {
        tags.iter().any(|x| x == t)
    }

    #[test]
    fn tokenizes_camel_case_digits_and_acronyms() {
        assert_eq!(tokenize("SM_GrassClump03"), ["sm", "grass", "clump", "03"]);
        assert_eq!(tokenize("HTTPServer"), ["http", "server"]);
        assert_eq!(
            tokenize("Rock-Wall 4k_8bit"),
            ["rock", "wall", "4k", "8bit"]
        );
        assert_eq!(tokenize("__"), Vec::<String>::new());
    }

    #[test]
    fn strips_ue_prefixes() {
        assert_eq!(split_prefix("SM_Rock_01"), (Some("SM"), "Rock_01"));
        assert_eq!(split_prefix("MI_Wood"), (Some("MI"), "Wood"));
        assert_eq!(split_prefix("T_Brick_N"), (Some("T"), "Brick_N"));
        // Not a convention prefix, or lower case: untouched.
        assert_eq!(split_prefix("Grass_01"), (None, "Grass_01"));
        assert_eq!(split_prefix("sm_rock"), (None, "sm_rock"));
        assert_eq!(split_prefix("SM_"), (None, "SM_"));
    }

    #[test]
    fn camel_case_names_and_variants() {
        let t = tag_asset("/Game/Grass/Meshes/SM_GrassClump_03", Some("StaticMesh"));
        assert_eq!(t.kind, "staticmesh");
        assert!(has(&t.tags, "grass"));
        assert!(has(&t.tags, "clump"));
        assert!(has(&t.tags, "variant:03"));
        assert!(has(&t.tags, "class:staticmesh"));
        assert!(has(&t.tags, "type:staticmesh"));
        assert!(!has(&t.tags, "type:mesh"));
        assert!(!has(&t.tags, "sm"));
        // "Meshes" is generic, "Grass" is a folder word.
        assert!(!has(&t.tags, "meshes"));
        assert_eq!(t.name_tokens, ["grass", "clump"]);
    }

    #[test]
    fn texture_suffixes_map_to_tags() {
        let n = tag_asset("/Game/Rock/T_Rock_01_N", Some("Texture2D"));
        assert!(has(&n.tags, "map:normal"));
        assert_eq!(n.kind, "texture");
        assert!(has(&n.tags, "variant:01"));
        assert_eq!(n.name_tokens, ["rock"]);
        let bc = tag_asset("/Game/Rock/T_Rock_BC", Some("Texture2D"));
        assert!(has(&bc.tags, "map:basecolor"));
        let orm = tag_asset("/Game/Rock/T_Rock_ORM", None);
        assert!(has(&orm.tags, "map:orm"));
        assert_eq!(orm.kind, "texture");
        let ao = tag_asset("/Game/Rock/T_Rock_AO", Some("Texture2D"));
        assert!(has(&ao.tags, "map:ao"));
        let h = tag_asset("/Game/Rock/Rock_Height", Some("Texture2D"));
        assert!(has(&h.tags, "map:height"));
    }

    #[test]
    fn ambiguous_letters_need_a_texture_context() {
        // A mesh named Chair_D is not a diffuse map; the `D` stays a (dropped, 1 letter) token.
        let t = tag_asset("/Game/Props/SM_Chair_D", Some("StaticMesh"));
        assert!(!has(&t.tags, "map:basecolor"));
        // Lettered series.
        assert!(has(&t.tags, "variant:d"));
        assert_eq!(t.name_tokens, ["chair"]);
    }

    #[test]
    fn lod_hint() {
        let t = tag_asset("/Game/Trees/SM_Pine_LOD2", Some("StaticMesh"));
        assert!(has(&t.tags, "lod"));
        assert_eq!(t.name_tokens, ["pine"]);
    }

    #[test]
    fn synonyms_expand_one_hop() {
        let g = tag_asset("/Game/Env/SM_Grass_Tall", Some("StaticMesh"));
        for w in ["foliage", "plant", "vegetation"] {
            assert!(has(&g.tags, w), "{w}");
        }
        let r = tag_asset("/Game/Env/SM_Rocks_Big", Some("StaticMesh"));
        assert!(has(&r.tags, "rocks"));
        assert!(has(&r.tags, "rock"));
        assert!(has(&r.tags, "stone"));
        assert!(has(&r.tags, "boulder"));
        // `stone -> rock`, but `rock -> boulder` is not applied to a plain `stone` asset.
        let s = tag_asset("/Game/Env/SM_Stone", Some("StaticMesh"));
        assert!(has(&s.tags, "rock"));
        assert!(!has(&s.tags, "boulder"));
    }

    #[test]
    fn kind_from_class_then_prefix_then_other() {
        assert_eq!(
            asset_kind(Some("MaterialInstanceConstant"), None),
            "material"
        );
        assert_eq!(asset_kind(Some("NiagaraSystem"), Some("NS")), "niagara");
        assert_eq!(asset_kind(Some("WidgetBlueprint"), None), "blueprint");
        assert_eq!(asset_kind(Some("SoundWave"), None), "sound");
        assert_eq!(asset_kind(Some("AnimSequence"), None), "animation");
        assert_eq!(asset_kind(Some("World"), None), "level");
        assert_eq!(asset_kind(Some("WhateverNew"), Some("SM")), "staticmesh");
        assert_eq!(asset_kind(Some("SkeletalMesh"), Some("SM")), "skeletalmesh");
        assert_eq!(asset_kind(Some("SkinnedAsset"), None), "skeletalmesh");
        assert_eq!(
            asset_kind(Some("NaniteStaticMeshProxy"), None),
            "staticmesh"
        );
        assert_eq!(asset_kind(None, Some("SK")), "skeletalmesh");
        assert_eq!(asset_kind(None, Some("SKM")), "skeletalmesh");
        assert_eq!(asset_kind(Some("GeometryCollection"), None), "other");
        assert_eq!(
            tag_asset("/Game/C/SKM_Hero", Some("SkeletalMesh")).kind,
            "skeletalmesh"
        );
        assert!(has(
            &tag_asset("/Game/C/SKM_Hero", Some("SkeletalMesh")).tags,
            "type:skeletalmesh"
        ));
        assert_eq!(asset_kind(None, Some("T")), "texture");
        assert_eq!(asset_kind(None, Some("A")), "other");
        assert_eq!(asset_kind(None, None), "other");
    }

    #[test]
    fn folder_words_become_tags_and_mount_is_dropped() {
        let t = tag_asset("/PluginX/Castle/Walls/Wall_Tall", Some("StaticMesh"));
        assert!(has(&t.tags, "castle"));
        assert!(has(&t.tags, "walls"));
        assert!(has(&t.tags, "plugin"));
        let g = tag_asset("/Game/Castle/Wall", Some("StaticMesh"));
        assert!(!has(&g.tags, "game"));
        assert_eq!(g.folder_tokens, ["castle"]);
    }

    #[test]
    fn pack_tags() {
        let t = tag_pack(
            "Medieval Village Pack",
            "medieval-village-pack",
            Kind::Content,
            &["5.3".to_string(), "5.5".to_string()],
        );
        assert!(has(&t, "pack:medieval-village-pack"));
        assert!(has(&t, "kind:content"));
        assert!(has(&t, "engine:5.3"));
        assert!(has(&t, "engine:5.5"));
        assert!(has(&t, "medieval"));
        let p = tag_pack("Demo", "demo", Kind::Project, &[]);
        assert!(has(&p, "kind:project"));
        assert_eq!(slugify("Sci-Fi  Props!"), "sci-fi-props");
        assert_eq!(slugify("!!!"), "pack");
    }

    #[test]
    fn type_filters_accept_the_mesh_umbrella() {
        assert!(type_matches("mesh", "staticmesh"));
        assert!(type_matches("mesh", "skeletalmesh"));
        assert!(!type_matches("mesh", "material"));
        assert!(type_matches("Static Mesh", "staticmesh"));
        assert!(!type_matches("staticmesh", "skeletalmesh"));
        assert!(is_type_filter("skeletal_mesh") && is_type_filter("mesh"));
        assert!(!is_type_filter("banana"));
        assert_eq!(TYPE_NAMES[0], "staticmesh");
        assert_eq!(TYPE_NAMES[TYPE_NAMES.len() - 1], "other");
    }
}
