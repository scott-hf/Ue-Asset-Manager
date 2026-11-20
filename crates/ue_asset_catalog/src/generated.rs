//! Read-only second asset source: finished rows of the HF automation pipeline queues
//! (`automation_pipeline_queue_mesh` / `_audio`, public NocoBase list reads, no token). They
//! appear in the catalog as `SourceKind::Generated` assets with ids `gen:<jobId>`, tag
//! `source:generated`, and are never importable here: the entry just says where the result lives.
//!
//! The last good result is cached on disk so the catalog still works when the portal is
//! unreachable. All requests use short timeouts and plain http.

use crate::http;
use crate::tags::{is_stopword, tokenize};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_PORTAL_URL: &str = "http://192.168.12.201:13000";
pub const MESH_COLLECTION: &str = "automation_pipeline_queue_mesh";
pub const AUDIO_COLLECTION: &str = "automation_pipeline_queue_audio";
pub const TIMEOUT: Duration = Duration::from_secs(5);
const PAGE_SIZE: usize = 200;
const MAX_PAGES: usize = 50;
const CACHE_SCHEMA: u32 = 1;

const MESH_FIELDS: &str =
    "jobId,title,state,stage,targetProject,outputPath,unrealObjectPath,resultData,customData,completedAt";
const AUDIO_FIELDS: &str = "jobId,title,state,stage,targetProject,outputPath,unrealObjectPath,resultData,customData,audioType,prompt,durationSeconds,completedAt";

#[derive(Clone, Debug)]
pub struct PortalConfig {
    pub base_url: String,
    pub timeout: Duration,
    /// Where the last good list is kept.
    pub cache_path: Option<PathBuf>,
}

impl PortalConfig {
    pub fn new(base_url: &str) -> PortalConfig {
        PortalConfig {
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout: TIMEOUT,
            cache_path: None,
        }
    }
}

/// A finished generated asset (mesh or sound).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GeneratedAsset {
    pub job_id: String,
    /// `mesh` or `sound`.
    pub kind: String,
    pub title: String,
    pub target_project: Option<String>,
    pub unreal_object_path: Option<String>,
    pub output_path: Option<String>,
    pub class: Option<String>,
    /// The prompt (audio); meshes mostly have none.
    pub description: Option<String>,
    pub category: Option<String>,
    pub completed_at: Option<String>,
    /// Size / complexity / duration buckets (`size:small`, `complexity:simple`, ...).
    pub extra_tags: Vec<String>,
    /// Compact facts for `get_asset` (triangles, dimensions, duration, seed, ...).
    pub details: Value,
}

#[derive(Clone, Debug, Default)]
pub struct FetchReport {
    pub meshes: usize,
    pub sounds: usize,
    pub from_cache: bool,
    pub warnings: Vec<String>,
}

fn as_obj(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn num_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn tail(path: &str) -> &str {
    let t = path.rsplit(['/', '\\']).next().unwrap_or(path);
    t.split('.').next().unwrap_or(t)
}

fn size_bucket(cm: f64) -> &'static str {
    if cm < 30.0 {
        "small"
    } else if cm < 150.0 {
        "medium"
    } else if cm < 500.0 {
        "large"
    } else {
        "huge"
    }
}

fn triangle_bucket(tris: f64) -> &'static str {
    if tris < 2_000.0 {
        "low"
    } else if tris < 20_000.0 {
        "mid"
    } else {
        "high"
    }
}

fn duration_bucket(sec: f64) -> &'static str {
    if sec < 3.0 {
        "short"
    } else if sec < 15.0 {
        "medium"
    } else {
        "long"
    }
}

/// One mesh row -> asset. `None` when it has no job id.
fn parse_mesh(row: &Value) -> Option<GeneratedAsset> {
    let job_id = str_of(row, "jobId")?.to_string();
    let result = as_obj(row.get("resultData"));
    let custom = as_obj(row.get("customData"));
    let manifest = result
        .get("assetManifest")
        .or_else(|| result.get("importedAssets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The main asset is the row that names a class; companions only have a package file.
    let first = manifest
        .iter()
        .find(|m| m.get("class").is_some())
        .or_else(|| manifest.first());
    let object_path = str_of(row, "unrealObjectPath")
        .or_else(|| first.and_then(|f| str_of(f, "objectPath")))
        .map(str::to_string);
    let class = first
        .and_then(|f| str_of(f, "class"))
        .map(str::to_string)
        .or_else(|| Some("StaticMesh".to_string()));
    let title = str_of(row, "title")
        .map(str::to_string)
        .or_else(|| object_path.as_deref().map(|p| tail(p).to_string()))
        .unwrap_or_else(|| job_id.clone());

    let stats = result.get("meshStats").cloned().unwrap_or(Value::Null);
    let smart = result.get("smartSize").cloned().unwrap_or(Value::Null);
    let tris = num_of(&stats, "trianglesAfter");
    let longest = num_of(&stats, "finalLongestCm").or_else(|| num_of(&smart, "longest_cm"));
    let complexity = str_of(&custom, "geometryComplexity")
        .or_else(|| str_of(&smart, "geometry_complexity"))
        .map(str::to_lowercase);
    let mut extra_tags = Vec::new();
    if let Some(l) = longest {
        extra_tags.push(format!("size:{}", size_bucket(l)));
    }
    if let Some(c) = &complexity {
        extra_tags.push(format!("complexity:{c}"));
    }
    if let Some(t) = tris {
        extra_tags.push(format!("tris:{}", triangle_bucket(t)));
    }
    let destination = str_of(&custom, "destination")
        .or_else(|| str_of(&result, "destination"))
        .map(str::to_string);
    Some(GeneratedAsset {
        job_id,
        kind: "mesh".into(),
        title,
        target_project: str_of(row, "targetProject").map(str::to_string),
        unreal_object_path: object_path,
        output_path: str_of(row, "outputPath").map(str::to_string),
        class,
        description: None,
        category: None,
        completed_at: str_of(row, "completedAt").map(str::to_string),
        extra_tags,
        details: json!({
            "triangles": tris,
            "longest_cm": longest,
            "dimensions_cm": stats.get("finalDimensionsCm"),
            "geometry_complexity": complexity,
            "destination": destination,
            "files": manifest.iter().filter_map(|m| str_of(m, "packageFile")).take(10).collect::<Vec<_>>(),
        }),
    })
}

/// One audio row -> asset. Rows parked at `audio_review` are pending, not entries.
fn parse_audio(row: &Value) -> Option<GeneratedAsset> {
    let job_id = str_of(row, "jobId")?.to_string();
    if str_of(row, "stage") == Some("audio_review") {
        return None;
    }
    let result = as_obj(row.get("resultData"));
    let custom = as_obj(row.get("customData"));
    let object_path = str_of(row, "unrealObjectPath").map(str::to_string);
    let category = str_of(row, "audioType")
        .or_else(|| str_of(&custom, "category"))
        .map(str::to_string);
    let duration = num_of(row, "durationSeconds");
    let artifact = result
        .get("artifacts")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .map(|a| match a {
            Value::String(s) => s.clone(),
            other => other
                .get("path")
                .or_else(|| other.get("file"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .filter(|s| !s.is_empty());
    let title = str_of(row, "title")
        .map(str::to_string)
        .or_else(|| object_path.as_deref().map(|p| tail(p).to_string()))
        .unwrap_or_else(|| job_id.clone());
    let mut extra_tags = Vec::new();
    if let Some(c) = &category {
        extra_tags.push(format!("audio:{}", c.to_lowercase()));
    }
    if let Some(d) = duration {
        extra_tags.push(format!("duration:{}", duration_bucket(d)));
    }
    Some(GeneratedAsset {
        job_id,
        kind: "sound".into(),
        title,
        target_project: str_of(row, "targetProject").map(str::to_string),
        unreal_object_path: object_path,
        output_path: str_of(row, "outputPath").map(str::to_string).or(artifact),
        class: Some("SoundWave".to_string()),
        description: str_of(row, "prompt").map(str::to_string),
        category,
        completed_at: str_of(row, "completedAt").map(str::to_string),
        extra_tags,
        details: json!({
            "duration_seconds": duration,
            "seed": custom.get("seed"),
            "variation": custom.get("variation"),
            "use_reprompt": custom.get("useReprompt"),
        }),
    })
}

/// Parse the `data` array of a list response. `kind` is `mesh` or `sound`.
pub fn parse_rows(kind: &str, rows: &[Value]) -> Vec<GeneratedAsset> {
    rows.iter()
        .filter_map(|r| {
            if kind == "mesh" {
                parse_mesh(r)
            } else {
                parse_audio(r)
            }
        })
        .collect()
}

/// `<base>/api/<collection>:list?filter=...&fields=...&sort=-completedAt&pageSize=200&page=N`.
pub fn list_url(base: &str, collection: &str, fields: &str, page: usize) -> String {
    let filter = json!({"state": "completed"}).to_string();
    format!(
        "{}/api/{collection}:list?filter={}&fields={}&sort=-completedAt&pageSize={PAGE_SIZE}&page={page}",
        base.trim_end_matches('/'),
        http::urlencode(&filter),
        http::urlencode(fields).replace("%2C", ","),
    )
}

fn fetch_collection(
    cfg: &PortalConfig,
    collection: &str,
    fields: &str,
    kind: &str,
) -> Result<Vec<GeneratedAsset>, String> {
    let mut out = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = list_url(&cfg.base_url, collection, fields, page);
        let resp = http::get(&url, &[], cfg.timeout)?;
        if !resp.is_success() {
            return Err(format!("{collection}: HTTP {}", resp.status));
        }
        let v = resp.json()?;
        let rows = v
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{collection}: no `data` array in the response"))?;
        out.extend(parse_rows(kind, rows));
        let total_pages = v
            .pointer("/meta/totalPage")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        if rows.len() < PAGE_SIZE || page >= total_pages {
            break;
        }
    }
    Ok(out)
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    schema: u32,
    assets: Vec<GeneratedAsset>,
}

fn read_cache(cfg: &PortalConfig) -> Option<Vec<GeneratedAsset>> {
    let text = fs::read_to_string(cfg.cache_path.as_ref()?).ok()?;
    let c: CacheFile = serde_json::from_str(&text).ok()?;
    (c.schema == CACHE_SCHEMA).then_some(c.assets)
}

fn write_cache(cfg: &PortalConfig, assets: &[GeneratedAsset]) -> Result<(), String> {
    let Some(path) = &cfg.cache_path else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec(&json!({"schema": CACHE_SCHEMA, "assets": assets}))
        .map_err(|e| e.to_string())?;
    let mut tmp = path.clone().into_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Fetch finished mesh and sound rows. When the portal is unreachable (or answers badly) the last
/// good cache is used; with neither, the result is empty and the report says why.
pub fn load(cfg: &PortalConfig) -> (Vec<GeneratedAsset>, FetchReport) {
    let mut report = FetchReport::default();
    let meshes = fetch_collection(cfg, MESH_COLLECTION, MESH_FIELDS, "mesh");
    let sounds = fetch_collection(cfg, AUDIO_COLLECTION, AUDIO_FIELDS, "sound");
    match (meshes, sounds) {
        (Ok(mut m), Ok(s)) => {
            report.meshes = m.len();
            report.sounds = s.len();
            m.extend(s);
            if let Err(e) = write_cache(cfg, &m) {
                report
                    .warnings
                    .push(format!("could not write the generated-assets cache: {e}"));
            }
            (m, report)
        }
        (m, s) => {
            for e in [m.as_ref().err(), s.as_ref().err()].into_iter().flatten() {
                report.warnings.push(format!("portal unavailable: {e}"));
            }
            if let Some(cached) = read_cache(cfg) {
                report.from_cache = true;
                report.meshes = cached.iter().filter(|a| a.kind == "mesh").count();
                report.sounds = cached.len() - report.meshes;
                report
                    .warnings
                    .push("using the cached list of generated assets".to_string());
                return (cached, report);
            }
            // No cache: keep whatever half succeeded.
            let mut all = m.unwrap_or_default();
            all.extend(s.unwrap_or_default());
            report.meshes = all.iter().filter(|a| a.kind == "mesh").count();
            report.sounds = all.len() - report.meshes;
            (all, report)
        }
    }
}

/// Word tokens of a title/prompt for tagging and search (stopwords and digits dropped).
pub fn words(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|t| {
            t.chars().count() >= 2 && !is_stopword(t) && !t.chars().all(|c| c.is_ascii_digit())
        })
        .collect()
}

/// Read one job row (any state) from either queue by job id: used by `generation_status`.
pub fn fetch_job(cfg: &PortalConfig, job_id: &str) -> Result<Option<Value>, String> {
    for (collection, fields) in [
        (MESH_COLLECTION, "jobId,title,state,stage,targetProject,outputPath,unrealObjectPath,completedAt,error,lastError"),
        (AUDIO_COLLECTION, "jobId,title,state,stage,targetProject,outputPath,unrealObjectPath,completedAt,audioType,prompt,durationSeconds,error,lastError"),
    ] {
        let filter = json!({"jobId": job_id}).to_string();
        let url = format!(
            "{}/api/{collection}:list?filter={}&fields={}&pageSize=1",
            cfg.base_url.trim_end_matches('/'),
            http::urlencode(&filter),
            http::urlencode(fields).replace("%2C", ","),
        );
        let resp = http::get(&url, &[], cfg.timeout)?;
        if !resp.is_success() {
            return Err(format!("{collection}: HTTP {}", resp.status));
        }
        let v = resp.json()?;
        if let Some(row) = v.get("data").and_then(Value::as_array).and_then(|a| a.first()) {
            let mut row = row.clone();
            row["queue"] = json!(if collection == MESH_COLLECTION { "mesh" } else { "audio" });
            return Ok(Some(row));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake;
    use crate::testutil::Tmp;

    fn mesh_row() -> Value {
        json!({
            "jobId": "mesh-123",
            "title": "Mossy Stone Lantern",
            "state": "completed",
            "stage": "done",
            "targetProject": "PrjEfz",
            "outputPath": "F:/out/mossy_lantern",
            "unrealObjectPath": "/Game/Generated/SM_MossyStoneLantern.SM_MossyStoneLantern",
            "completedAt": "2026-11-10T12:00:00Z",
            "resultData": {
                "importedAssets": [
                    {"class": "StaticMesh", "objectPath": "/Game/Generated/SM_MossyStoneLantern", "packageFile": "Content/Generated/SM_MossyStoneLantern.uasset"},
                    {"packageFile": "Content/Generated/SM_MossyStoneLantern.uexp"}
                ],
                "meshStats": {"trianglesAfter": 8400, "finalDimensionsCm": [40, 40, 120], "finalLongestCm": 120.0},
                "smartSize": {"geometry_complexity": "moderate"}
            },
            "customData": "{\"geometryComplexity\": \"Detailed\", \"destination\": \"/Game/Generated\"}"
        })
    }

    #[test]
    fn parses_a_mesh_row() {
        let a = parse_mesh(&mesh_row()).unwrap();
        assert_eq!(a.job_id, "mesh-123");
        assert_eq!(a.kind, "mesh");
        assert_eq!(a.class.as_deref(), Some("StaticMesh"));
        assert_eq!(a.target_project.as_deref(), Some("PrjEfz"));
        assert!(a
            .unreal_object_path
            .as_deref()
            .unwrap()
            .ends_with("SM_MossyStoneLantern"));
        assert!(a.extra_tags.contains(&"size:medium".to_string()));
        assert!(a.extra_tags.contains(&"complexity:detailed".to_string()));
        assert!(a.extra_tags.contains(&"tris:mid".to_string()));
        assert_eq!(a.details["triangles"], 8400.0);
        assert_eq!(a.details["destination"], "/Game/Generated");
        assert_eq!(a.details["files"].as_array().unwrap().len(), 2);
        // No job id: not an entry.
        assert!(parse_mesh(&json!({"title": "x"})).is_none());
    }

    #[test]
    fn parses_audio_rows_and_skips_pending_review() {
        let row = json!({
            "jobId": "aud-1", "title": "Wooden door creak", "state": "completed", "stage": "done",
            "audioType": "SFX", "prompt": "a heavy wooden door creaking open slowly",
            "durationSeconds": 4.5, "targetProject": "PrjEfz",
            "unrealObjectPath": "/Game/Audio/SW_DoorCreak",
            "customData": {"seed": 7, "variation": 2, "category": "SFX", "useReprompt": true},
            "resultData": {"artifacts": ["F:/out/door.wav"]}
        });
        let a = parse_audio(&row).unwrap();
        assert_eq!(a.kind, "sound");
        assert_eq!(a.class.as_deref(), Some("SoundWave"));
        assert_eq!(
            a.description.as_deref(),
            Some("a heavy wooden door creaking open slowly")
        );
        assert!(a.extra_tags.contains(&"audio:sfx".to_string()));
        assert!(a.extra_tags.contains(&"duration:medium".to_string()));
        assert_eq!(a.output_path.as_deref(), Some("F:/out/door.wav"));
        assert_eq!(a.details["seed"], 7);
        let mut pending = row.clone();
        pending["stage"] = json!("audio_review");
        assert!(parse_audio(&pending).is_none());
        assert_eq!(parse_rows("sound", &[row, pending]).len(), 1);
    }

    #[test]
    fn builds_the_list_url() {
        let u = list_url("http://h:1/", MESH_COLLECTION, "a,b", 2);
        assert!(u.starts_with("http://h:1/api/automation_pipeline_queue_mesh:list?filter=%7B%22state%22%3A%22completed%22%7D&fields=a,b&sort=-completedAt&pageSize=200&page=2"), "{u}");
    }

    #[test]
    fn loads_from_a_fake_portal_and_falls_back_to_the_cache() {
        let t = Tmp::new();
        let mesh_body = json!({"data": [mesh_row()], "meta": {"totalPage": 1}}).to_string();
        let audio_body = json!({"data": [
            {"jobId": "aud-1", "title": "Pop", "stage": "done", "audioType": "SFX", "durationSeconds": 1.0},
            {"jobId": "aud-2", "title": "Parked", "stage": "audio_review", "audioType": "Music"}
        ]})
        .to_string();
        let srv = fake::serve(vec![fake::ok_json(&mesh_body), fake::ok_json(&audio_body)]);
        let mut cfg = PortalConfig::new(&srv.url);
        cfg.cache_path = Some(t.path().join("generated.json"));
        let (items, rep) = load(&cfg);
        assert_eq!((rep.meshes, rep.sounds, rep.from_cache), (1, 1, false));
        assert_eq!(items.len(), 2);
        assert!(cfg.cache_path.as_ref().unwrap().exists());
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(reqs[0].contains("automation_pipeline_queue_mesh:list"));
        assert!(reqs[1].contains("automation_pipeline_queue_audio:list"));
        assert!(!reqs.iter().any(|r| r.contains("meshJobs:list")));

        // Portal gone: the cache answers.
        let dead = PortalConfig {
            base_url: "http://127.0.0.1:1".into(),
            timeout: Duration::from_millis(300),
            cache_path: cfg.cache_path.clone(),
        };
        let (items2, rep2) = load(&dead);
        assert!(rep2.from_cache);
        assert_eq!(items2.len(), 2);
        assert!(!rep2.warnings.is_empty());
        // Neither portal nor cache: empty, with a warning.
        let none = PortalConfig {
            cache_path: Some(t.path().join("missing.json")),
            ..dead
        };
        let (items3, rep3) = load(&none);
        assert!(items3.is_empty());
        assert!(!rep3.warnings.is_empty());
    }

    #[test]
    fn fetches_one_job_row() {
        let body =
            json!({"data": [{"jobId": "j1", "state": "running", "stage": "unreal_postprocess"}]})
                .to_string();
        let srv = fake::serve(vec![fake::ok_json(&body)]);
        let row = fetch_job(&PortalConfig::new(&srv.url), "j1")
            .unwrap()
            .unwrap();
        assert_eq!(row["state"], "running");
        assert_eq!(row["queue"], "mesh");
    }
}
