//! Vision tagging: an Ollama vision model describes an asset's own editor thumbnail; the result
//! is cached in a derived overlay `<zip>.vision.json` next to the library zip and merged into the
//! catalog's tags and descriptions at load.
//!
//! The call mirrors `estimate_smart_size` of the mesh pipeline
//! (`hf_automation_comfy_worker.py`): `POST /api/generate` with `format: json`, `think: false`,
//! temperature 0, the answer read from `response` and, failing that, from `thinking`
//! (FND-00085: qwen3-vl sometimes puts the JSON there). Never used by the MCP server's startup
//! path; only the separate `ue_asset_enrich` binary calls the model.

use crate::b64;
use crate::http;
use crate::tags::{is_stopword, tokenize};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use ue_asset_library::LibraryEntry;

pub const SCHEMA: u32 = 1;
/// Bump when [`build_prompt`] changes meaningfully: items are re-done for a new version.
pub const PROMPT_VERSION: u32 = 1;
pub const DEFAULT_MODEL: &str = "qwen3-vl:8b";
pub const DEFAULT_OLLAMA_URL: &str = "http://192.168.12.136:11434";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TAGS: usize = 8;
const MAX_DESCRIPTION: usize = 240;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct VisionItem {
    pub description: String,
    pub tags: Vec<String>,
    pub category: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VisionOverlay {
    pub schema: u32,
    pub model: String,
    pub prompt_version: u32,
    /// Keyed by `game_path`.
    pub items: BTreeMap<String, VisionItem>,
}

impl VisionOverlay {
    pub fn new(model: &str) -> VisionOverlay {
        VisionOverlay {
            schema: SCHEMA,
            model: model.to_string(),
            prompt_version: PROMPT_VERSION,
            items: BTreeMap::new(),
        }
    }

    /// Does this overlay hold work that is valid for `model` and the current prompt?
    pub fn is_current(&self, model: &str) -> bool {
        self.schema == SCHEMA && self.model == model && self.prompt_version == PROMPT_VERSION
    }
}

/// `<zip>.vision.json`.
pub fn vision_path(entry: &LibraryEntry) -> PathBuf {
    let mut s = entry.zip_path.clone().into_os_string();
    s.push(".vision.json");
    PathBuf::from(s)
}

/// Read the overlay; missing, corrupt or another schema = `None`.
pub fn load(entry: &LibraryEntry) -> Option<VisionOverlay> {
    let text = fs::read_to_string(vision_path(entry)).ok()?;
    let o: VisionOverlay = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    (o.schema == SCHEMA).then_some(o)
}

/// Write the overlay atomically (temp file next to it, then rename).
pub fn save(entry: &LibraryEntry, overlay: &VisionOverlay) -> io::Result<()> {
    let path = vision_path(entry);
    let mut tmp = path.clone().into_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, serde_json::to_vec_pretty(overlay)?)?;
    fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The prompt for one asset. The name and class are hints so the model anchors on identity.
pub fn build_prompt(name: &str, class: Option<&str>) -> String {
    let mut p = String::from(
        "This is the editor thumbnail of a single game asset from an Unreal Engine asset pack. \
         Reply with ONLY a JSON object (no prose, no markdown fences) with these keys: \
         \"description\" (string: one short sentence saying what it is), \
         \"tags\" (array of up to 8 lowercase single words: the object type, its material, \
         dominant colours, style, and the biome or theme it fits), \
         \"category\" (string: one lowercase word, e.g. prop, foliage, rock, building, vehicle, \
         character, weapon, texture, material, effect).",
    );
    let name_hint = name.trim();
    if !name_hint.is_empty() {
        p.push_str(&format!(" The asset is named \"{name_hint}\""));
        match class {
            Some(c) if !c.is_empty() => p.push_str(&format!(" (class {c}).")),
            _ => p.push('.'),
        }
    }
    p
}

/// The Ollama `/api/generate` request body.
pub fn build_request(model: &str, prompt: &str, image: &[u8]) -> Value {
    json!({
        "model": model,
        "prompt": prompt,
        "images": [b64::encode(image)],
        "stream": false,
        "format": "json",
        "think": false,
        "options": {"temperature": 0},
    })
}

/// Lower-case, single-word, de-duplicated tags (at most 8): drops stopwords and junk.
pub fn clean_tags(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let t: String = r
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let t = t.trim_matches('-').to_string();
        if t.len() < 2 || t.len() > 24 || is_stopword(&t) || out.contains(&t) {
            continue;
        }
        out.push(t);
        if out.len() >= MAX_TAGS {
            break;
        }
    }
    out
}

/// Parse the model's text into an item. Tolerates markdown fences and prose around the JSON.
pub fn parse_response(text: &str) -> Option<VisionItem> {
    let t = text.trim();
    let (a, b) = (t.find('{')?, t.rfind('}')?);
    if b <= a {
        return None;
    }
    let v: Value = serde_json::from_str(&t[a..=b]).ok()?;
    let description: String = v
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_DESCRIPTION)
        .collect();
    let raw_tags: Vec<String> = match v.get("tags") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => s
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let tags = clean_tags(&raw_tags);
    let category = v
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>();
    if description.is_empty() && tags.is_empty() {
        return None;
    }
    Some(VisionItem {
        description,
        tags,
        category,
    })
}

/// Read the item from an Ollama reply: `response` first, then `thinking`.
pub fn parse_ollama_reply(reply: &Value) -> Result<VisionItem, String> {
    for key in ["response", "thinking"] {
        if let Some(item) = reply
            .get(key)
            .and_then(Value::as_str)
            .and_then(parse_response)
        {
            return Ok(item);
        }
    }
    Err("the model's answer had no usable JSON".to_string())
}

/// Describe one thumbnail with the vision model (blocking, up to [`REQUEST_TIMEOUT`]).
pub fn describe_image(
    ollama_url: &str,
    model: &str,
    prompt: &str,
    image: &[u8],
) -> Result<VisionItem, String> {
    let url = format!("{}/api/generate", ollama_url.trim_end_matches('/'));
    let resp = http::post_json(
        &url,
        &[],
        &build_request(model, prompt, image),
        REQUEST_TIMEOUT,
    )?;
    if !resp.is_success() {
        return Err(format!(
            "Ollama answered HTTP {}: {}",
            resp.status,
            resp.text().chars().take(200).collect::<String>()
        ));
    }
    parse_ollama_reply(&resp.json()?)
}

/// Words of a description for the search index (stopwords and digits dropped).
pub fn description_tokens(description: &str) -> Vec<String> {
    tokenize(description)
        .into_iter()
        .filter(|t| {
            t.chars().count() >= 2 && !is_stopword(t) && !t.chars().all(|c| c.is_ascii_digit())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{entry, Tmp};
    use ue_asset_library::Kind;

    #[test]
    fn parses_plain_json() {
        let i = parse_response(
            r#"{"description":"A mossy grey boulder.","tags":["Rock","moss","GREY","nature","the"],"category":"Rock"}"#,
        )
        .unwrap();
        assert_eq!(i.description, "A mossy grey boulder.");
        assert_eq!(i.tags, ["rock", "moss", "grey", "nature"]);
        assert_eq!(i.category, "rock");
    }

    #[test]
    fn parses_fenced_json_and_prose() {
        let i = parse_response(
            "Sure!\n```json\n{\"description\": \"x y\", \"tags\": \"wood, brown stool\"}\n```",
        )
        .unwrap();
        assert_eq!(i.tags, ["wood", "brown", "stool"]);
        assert!(parse_response("no json here").is_none());
        assert!(parse_response("{}").is_none());
        assert!(parse_response("{\"description\": \"\", \"tags\": []}").is_none());
        assert!(parse_response("} oops {").is_none());
    }

    #[test]
    fn falls_back_to_thinking() {
        // FND-00085: the JSON lands in `thinking` and `response` is empty.
        let reply = json!({"response": "", "thinking": "{\"description\": \"A red barrel.\", \"tags\": [\"barrel\", \"red\"], \"category\": \"prop\"}"});
        let i = parse_ollama_reply(&reply).unwrap();
        assert_eq!(i.category, "prop");
        assert_eq!(i.tags, ["barrel", "red"]);
        // `response` wins when both have JSON.
        let reply = json!({"response": "{\"description\":\"a\",\"tags\":[\"one\"]}", "thinking": "{\"description\":\"b\",\"tags\":[\"two\"]}"});
        assert_eq!(parse_ollama_reply(&reply).unwrap().tags, ["one"]);
        assert!(parse_ollama_reply(&json!({"response": "nope"})).is_err());
    }

    #[test]
    fn limits_and_cleans_tags() {
        let raw: Vec<String> = [
            "A",
            "Rock!",
            "rock",
            "x",
            "very-long-tag-that-is-way-too-long-to-keep",
            "ok-tag",
            "1",
            "2a",
            "b2",
            "c3",
            "d4",
            "e5",
            "f6",
            "g7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let t = clean_tags(&raw);
        assert!(t.len() <= 8);
        assert_eq!(&t[..2], ["rock", "ok-tag"]);
        assert!(!t.contains(&"x".to_string()));
    }

    #[test]
    fn request_body_follows_the_pipeline_convention() {
        let b = build_request("qwen3-vl:8b", "p", b"IMG");
        assert_eq!(b["model"], "qwen3-vl:8b");
        assert_eq!(b["stream"], false);
        assert_eq!(b["format"], "json");
        assert_eq!(b["think"], false);
        assert_eq!(b["options"]["temperature"], 0);
        assert_eq!(b["images"][0], b64::encode(b"IMG"));
        let p = build_prompt("SM_Rock_01", Some("StaticMesh"));
        assert!(p.contains("SM_Rock_01") && p.contains("StaticMesh") && p.contains("ONLY a JSON"));
    }

    #[test]
    fn overlay_roundtrip_and_currency() {
        let t = Tmp::new();
        let mut e = entry("P", "aaaaaaaaaaaa0000", Some("5.3"), Kind::Content);
        e.zip_path = t.path().join("p.aaaaaaaaaaaa.zip");
        assert!(load(&e).is_none());
        let mut o = VisionOverlay::new("m1");
        o.items.insert(
            "/Game/A/SM_A".into(),
            VisionItem {
                description: "d".into(),
                tags: vec!["t".into()],
                category: "prop".into(),
            },
        );
        save(&e, &o).unwrap();
        assert!(vision_path(&e)
            .to_string_lossy()
            .ends_with(".zip.vision.json"));
        let back = load(&e).unwrap();
        assert_eq!(back.items.len(), 1);
        assert!(back.is_current("m1"));
        assert!(!back.is_current("other-model"));
        fs::write(vision_path(&e), "junk").unwrap();
        assert!(load(&e).is_none());
    }

    #[test]
    fn describes_through_a_local_fake_ollama() {
        let srv = http::fake::serve(vec![http::fake::ok_json(
            r#"{"response":"","thinking":"{\"description\":\"A crate.\",\"tags\":[\"crate\",\"wood\"],\"category\":\"prop\"}","done":true}"#,
        )]);
        let i = describe_image(&srv.url, "qwen3-vl:8b", "p", b"IMG").unwrap();
        assert_eq!(i.tags, ["crate", "wood"]);
        let req = srv.requests.lock().unwrap()[0].clone();
        assert!(req.starts_with("POST /api/generate HTTP/1.1"));
        assert!(req.contains("\"think\":false"));
    }

    /// Needs the real Ollama box on scott-zap; run with `-- --ignored`.
    #[test]
    #[ignore]
    fn real_ollama_describes_a_tiny_image() {
        // A 1x1 PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99,
            0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let url = std::env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
        let r = describe_image(&url, DEFAULT_MODEL, &build_prompt("Test", None), png);
        println!("{r:?}");
    }
}
