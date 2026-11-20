//! `request_generation`: ask Scott's mesh/sound pipeline (NocoBase `plugin-automation-pipeline`)
//! to make something that the library does not have.
//!
//! Opt-in (`--allow-generation`), capped per server session, and always pinned to an explicit
//! `targetProject` (an unpinned row would be claimed by any project's editor). The bearer token is
//! read at call time from `HF_PORTAL_TOKEN` or a token file and is never logged or returned.
//! Mesh generation is image-to-3D only. Payload building and validation are pure functions.

use crate::generated::DEFAULT_PORTAL_URL;
use crate::http;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const TOKEN_ENV: &str = "HF_PORTAL_TOKEN";
pub const DEFAULT_TOKEN_FILE: &str =
    "E:/git/claude-plugin-hf-infrastructure/tokens/nocobase-portal-api-token.txt";
pub const DEFAULT_MAX_SUBMISSIONS: usize = 5;
pub const AUDIO_CATEGORIES: [&str; 4] = ["Music", "Instrument", "SFX", "One-shot"];
pub const MESH_QUALITIES: [&str; 3] = ["fast", "balanced", "high"];
const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct GenerationConfig {
    /// `--allow-generation`. Off: the tool refuses.
    pub allowed: bool,
    pub portal_url: String,
    /// `--portal-token-file` (overrides the env var and the default file).
    pub token_file: Option<PathBuf>,
    /// Submissions (jobs) per server session.
    pub max_submissions: usize,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        GenerationConfig {
            allowed: false,
            portal_url: DEFAULT_PORTAL_URL.to_string(),
            token_file: None,
            max_submissions: DEFAULT_MAX_SUBMISSIONS,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioRequest {
    pub prompt: String,
    pub category: String,
    pub name: String,
    pub duration: Option<f64>,
    pub variations: u32,
    pub use_reprompt: bool,
    pub profile: Option<String>,
    pub target_project: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MeshRequest {
    pub image_path: PathBuf,
    pub output_name: String,
    pub quality: String,
    pub smart_resize: bool,
    pub target_project: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Audio(AudioRequest),
    Mesh(MeshRequest),
}

impl Request {
    /// How many jobs this request creates (counted against the session cap).
    pub fn job_count(&self) -> usize {
        match self {
            Request::Audio(a) => a.variations as usize,
            Request::Mesh(_) => 1,
        }
    }

    pub fn target_project(&self) -> &str {
        match self {
            Request::Audio(a) => &a.target_project,
            Request::Mesh(m) => &m.target_project,
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Some(_) => Err(format!("argument `{key}` must be a string")),
    }
}

/// Letters, digits, `_`, `-`, `.`; at most 64 chars.
fn safe_token(what: &str, s: &str) -> Result<String, String> {
    if s.is_empty()
        || s.len() > 64
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!(
            "`{what}` must be 1-64 characters of letters, digits, `_`, `-` or `.` (got {s:?})"
        ));
    }
    Ok(s.to_string())
}

/// A file-name-safe version of free text.
fn name_from(text: &str) -> String {
    let mut out = String::new();
    for w in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
    {
        if !out.is_empty() {
            out.push('_');
        }
        out.push_str(w);
    }
    out.truncate(48);
    if out.is_empty() {
        "generated".to_string()
    } else {
        out
    }
}

/// Validate the tool arguments. Never touches the network.
pub fn parse_request(args: &Value) -> Result<Request, String> {
    let kind = str_arg(args, "kind")?
        .ok_or("missing required argument `kind` (audio or mesh)")?
        .to_lowercase();
    let target = str_arg(args, "target_project")?.ok_or(
        "missing required argument `target_project`: the Unreal project (name, as in list_projects) whose editor should pick the job up; generation is never unpinned",
    )?;
    let target_project = safe_token("target_project", &target)?;
    match kind.as_str() {
        "audio" => {
            let prompt =
                str_arg(args, "prompt")?.ok_or("audio needs a `prompt` describing the sound")?;
            if prompt.chars().count() > 1000 {
                return Err("`prompt` is too long (max 1000 characters)".into());
            }
            let cat_raw = str_arg(args, "category")?.unwrap_or_else(|| "SFX".to_string());
            let category = AUDIO_CATEGORIES
                .iter()
                .find(|c| c.eq_ignore_ascii_case(&cat_raw))
                .ok_or_else(|| {
                    format!("`category` must be one of {}", AUDIO_CATEGORIES.join(", "))
                })?
                .to_string();
            let name = match str_arg(args, "name")? {
                Some(n) => safe_token("name", &n)?,
                None => name_from(&prompt),
            };
            let duration = match args.get("duration") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let d = v.as_f64().ok_or("`duration` must be a number of seconds")?;
                    if !(0.1..=120.0).contains(&d) {
                        return Err("`duration` must be between 0.1 and 120 seconds".into());
                    }
                    Some(d)
                }
            };
            let variations = match args.get("variations") {
                None | Some(Value::Null) => 1,
                Some(v) => {
                    let n = v.as_u64().ok_or("`variations` must be an integer 1-12")?;
                    if !(1..=12).contains(&n) {
                        return Err("`variations` must be between 1 and 12".into());
                    }
                    n as u32
                }
            };
            let use_reprompt = match args.get("use_reprompt") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("`use_reprompt` must be true or false".into()),
            };
            let profile = match str_arg(args, "profile")? {
                Some(p) => Some(safe_token("profile", &p)?),
                None => None,
            };
            Ok(Request::Audio(AudioRequest {
                prompt,
                category,
                name,
                duration,
                variations,
                use_reprompt,
                profile,
                target_project,
            }))
        }
        "mesh" => {
            let path = str_arg(args, "image_path")?.ok_or(
                "mesh generation is image-to-3D only: `image_path` (a png/jpg/webp file on this machine) is required",
            )?;
            let image_path = PathBuf::from(&path);
            let ext = image_path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                return Err("`image_path` must be a .png, .jpg, .jpeg or .webp file".into());
            }
            let meta = std::fs::metadata(&image_path)
                .map_err(|e| format!("cannot read image {}: {e}", image_path.display()))?;
            if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
                return Err("`image_path` must be a non-empty file of at most 25 MB".into());
            }
            let output_name = match str_arg(args, "output_name")? {
                Some(n) => safe_token("output_name", &n)?,
                None => name_from(
                    &image_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                ),
            };
            let quality = str_arg(args, "quality")?
                .unwrap_or_else(|| "balanced".to_string())
                .to_lowercase();
            if !MESH_QUALITIES.contains(&quality.as_str()) {
                return Err(format!(
                    "`quality` must be one of {}",
                    MESH_QUALITIES.join(", ")
                ));
            }
            let smart_resize = match args.get("smart_resize") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("`smart_resize` must be true or false".into()),
            };
            Ok(Request::Mesh(MeshRequest {
                image_path,
                output_name,
                quality,
                smart_resize,
                target_project,
            }))
        }
        other => Err(format!("`kind` must be `audio` or `mesh`, not `{other}`")),
    }
}

/// Body of `POST audioJobs:submit`.
pub fn audio_payload(r: &AudioRequest) -> Value {
    let mut v = json!({
        "prompt": r.prompt,
        "category": r.category,
        "name": r.name,
        "variations": r.variations,
        "use_reprompt": r.use_reprompt,
        "targetProject": r.target_project,
    });
    if let Some(d) = r.duration {
        v["duration"] = json!(d);
    }
    if let Some(p) = &r.profile {
        v["profile"] = json!(p);
    }
    v
}

/// Body of `POST meshJobs:submitBatch` once the image is uploaded (`source_image` is what
/// `uploadImage` returned).
pub fn mesh_batch_payload(r: &MeshRequest, source_image: &str) -> Value {
    json!({
        "images": [{"source_image": source_image, "output_name": r.output_name}],
        "quality": r.quality,
        "smart_resize": r.smart_resize,
        "targetProject": r.target_project,
    })
}

/// The bearer token: the explicit token file, else `HF_PORTAL_TOKEN`, else the default file.
/// CR/LF/whitespace stripped. The error text never contains the token.
pub fn read_token(cfg: &GenerationConfig) -> Result<String, String> {
    read_token_from(
        cfg,
        std::env::var(TOKEN_ENV).ok().as_deref(),
        Path::new(DEFAULT_TOKEN_FILE),
    )
}

fn read_token_from(
    cfg: &GenerationConfig,
    env_value: Option<&str>,
    default_file: &Path,
) -> Result<String, String> {
    let clean = |s: &str| s.trim().replace(['\r', '\n'], "");
    if let Some(f) = &cfg.token_file {
        let t = std::fs::read_to_string(f)
            .map_err(|e| format!("cannot read the portal token file {}: {e}", f.display()))?;
        let t = clean(&t);
        return if t.is_empty() {
            Err(format!("the portal token file {} is empty", f.display()))
        } else {
            Ok(t)
        };
    }
    if let Some(v) = env_value {
        let t = clean(v);
        if !t.is_empty() {
            return Ok(t);
        }
    }
    match std::fs::read_to_string(default_file) {
        Ok(t) if !clean(&t).is_empty() => Ok(clean(&t)),
        _ => Err(format!(
            "no portal token: set {TOKEN_ENV} or pass --portal-token-file"
        )),
    }
}

/// Every job id mentioned in a submit response (`jobId`, `job_id`, `jobIds`, nested in `data`).
pub fn job_ids(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    let key = k.to_ascii_lowercase();
                    match (key.as_str(), x) {
                        ("jobid" | "job_id", Value::String(s)) => out.push(s.clone()),
                        ("jobids" | "job_ids", Value::Array(a)) => {
                            out.extend(a.iter().filter_map(|j| j.as_str().map(str::to_string)));
                        }
                        _ => walk(x, out),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out.sort();
    out.dedup();
    out
}

/// What `uploadImage` called the stored file (the field name is not fixed).
fn uploaded_name(v: &Value) -> Option<String> {
    let data = v.get("data").unwrap_or(v);
    if let Some(s) = data.as_str() {
        return Some(s.to_string());
    }
    for key in [
        "source_image",
        "sourceImage",
        "path",
        "filePath",
        "file",
        "url",
        "filename",
        "name",
    ] {
        if let Some(s) = data.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

fn short(s: &str) -> String {
    s.chars().take(300).collect()
}

fn api(cfg: &GenerationConfig, rpc: &str) -> String {
    format!("{}/api/{rpc}", cfg.portal_url.trim_end_matches('/'))
}

/// Send a validated request to the portal. The token is only used inside the Authorization
/// header and never appears in the returned value or in errors.
pub fn submit(cfg: &GenerationConfig, req: &Request, token: &str) -> Result<Value, String> {
    let auth = [("Authorization", format!("Bearer {token}"))];
    match req {
        Request::Audio(a) => {
            let resp = http::post_json(
                &api(cfg, "audioJobs:submit"),
                &auth,
                &audio_payload(a),
                SUBMIT_TIMEOUT,
            )?;
            if !resp.is_success() {
                return Err(format!(
                    "portal answered HTTP {}: {}",
                    resp.status,
                    short(&resp.text())
                ));
            }
            let body = resp.json().unwrap_or(Value::Null);
            Ok(json!({"job_ids": job_ids(&body), "portal_response": body}))
        }
        Request::Mesh(m) => {
            let bytes = std::fs::read(&m.image_path)
                .map_err(|e| format!("cannot read image {}: {e}", m.image_path.display()))?;
            let filename = m
                .image_path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "image.png".to_string());
            let ctype = match m
                .image_path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .as_deref()
            {
                Some("jpg" | "jpeg") => "image/jpeg",
                Some("webp") => "image/webp",
                _ => "image/png",
            };
            let part = http::Part {
                name: "image",
                filename: &filename,
                content_type: ctype,
                data: &bytes,
            };
            let up = http::post_multipart(
                &api(cfg, "meshJobs:uploadImage"),
                &auth,
                &[part],
                SUBMIT_TIMEOUT,
            )?;
            if !up.is_success() {
                return Err(format!(
                    "image upload failed: HTTP {}: {}",
                    up.status,
                    short(&up.text())
                ));
            }
            let up_body = up.json().unwrap_or(Value::Null);
            let source = uploaded_name(&up_body).ok_or_else(|| {
                format!(
                    "image upload answered without a file name: {}",
                    short(&up_body.to_string())
                )
            })?;
            let resp = http::post_json(
                &api(cfg, "meshJobs:submitBatch"),
                &auth,
                &mesh_batch_payload(m, &source),
                SUBMIT_TIMEOUT,
            )?;
            if !resp.is_success() {
                return Err(format!(
                    "portal answered HTTP {}: {}",
                    resp.status,
                    short(&resp.text())
                ));
            }
            let body = resp.json().unwrap_or(Value::Null);
            Ok(json!({"job_ids": job_ids(&body), "portal_response": body}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake;
    use crate::testutil::Tmp;

    #[test]
    fn audio_requests_are_validated_and_shaped() {
        let r = parse_request(&json!({
            "kind": "audio", "target_project": "PrjEfz", "prompt": "a heavy wooden door creaking open",
            "category": "sfx", "variations": 3, "duration": 4.5
        }))
        .unwrap();
        let Request::Audio(a) = &r else { panic!() };
        assert_eq!(a.category, "SFX");
        assert_eq!(a.name, "a_heavy_wooden_door_creaking_open");
        assert_eq!(r.job_count(), 3);
        let p = audio_payload(a);
        assert_eq!(p["targetProject"], "PrjEfz");
        assert_eq!(p["variations"], 3);
        assert_eq!(p["duration"], 4.5);
        assert_eq!(p["use_reprompt"], false);
        assert!(p.get("profile").is_none());

        for (bad, why) in [
            (json!({"kind": "audio", "prompt": "x"}), "target_project"),
            (json!({"kind": "audio", "target_project": "P"}), "prompt"),
            (
                json!({"kind": "audio", "target_project": "P", "prompt": "x", "category": "Opera"}),
                "category",
            ),
            (
                json!({"kind": "audio", "target_project": "P", "prompt": "x", "variations": 13}),
                "variations",
            ),
            (
                json!({"kind": "audio", "target_project": "P", "prompt": "x", "variations": 0}),
                "variations",
            ),
            (
                json!({"kind": "audio", "target_project": "P", "prompt": "x", "duration": 500}),
                "duration",
            ),
            (
                json!({"kind": "audio", "target_project": "bad name!", "prompt": "x"}),
                "target_project",
            ),
            (
                json!({"kind": "audio", "target_project": "P", "prompt": "x", "name": "../evil"}),
                "name",
            ),
            (json!({"kind": "video", "target_project": "P"}), "kind"),
            (json!({"target_project": "P"}), "kind"),
        ] {
            let e = parse_request(&bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn mesh_requests_need_an_image() {
        let t = Tmp::new();
        let png = t.path().join("Mossy Lantern.png");
        std::fs::write(&png, b"\x89PNGdata").unwrap();
        let r = parse_request(&json!({"kind": "mesh", "target_project": "PrjEfz", "image_path": png.to_string_lossy()})).unwrap();
        let Request::Mesh(m) = &r else { panic!() };
        assert_eq!(m.quality, "balanced");
        assert!(m.smart_resize);
        assert_eq!(m.output_name, "Mossy_Lantern");
        let p = mesh_batch_payload(m, "uploads/abc.png");
        assert_eq!(p["images"][0]["source_image"], "uploads/abc.png");
        assert_eq!(p["images"][0]["output_name"], "Mossy_Lantern");
        assert_eq!(p["quality"], "balanced");
        assert_eq!(p["targetProject"], "PrjEfz");
        assert_eq!(r.job_count(), 1);

        let no_image =
            parse_request(&json!({"kind": "mesh", "target_project": "P", "prompt": "a lantern"}))
                .unwrap_err();
        assert!(no_image.contains("image-to-3D"), "{no_image}");
        let missing = parse_request(&json!({"kind": "mesh", "target_project": "P", "image_path": t.path().join("nope.png").to_string_lossy()})).unwrap_err();
        assert!(missing.contains("cannot read"), "{missing}");
        let txt = t.path().join("a.txt");
        std::fs::write(&txt, "x").unwrap();
        assert!(parse_request(
            &json!({"kind": "mesh", "target_project": "P", "image_path": txt.to_string_lossy()})
        )
        .is_err());
        let q = parse_request(&json!({"kind": "mesh", "target_project": "P", "image_path": png.to_string_lossy(), "quality": "ultra"})).unwrap_err();
        assert!(q.contains("quality"));
    }

    #[test]
    fn token_sources_and_stripping() {
        let t = Tmp::new();
        let file = t.path().join("tok.txt");
        std::fs::write(&file, "  abc123\r\n").unwrap();
        let default = t.path().join("default.txt");
        std::fs::write(&default, "fromdefault\n").unwrap();
        let mut cfg = GenerationConfig::default();
        // Nothing but the default file.
        assert_eq!(
            read_token_from(&cfg, None, &default).unwrap(),
            "fromdefault"
        );
        // Env wins over the default file; the explicit file wins over both.
        assert_eq!(
            read_token_from(&cfg, Some("envtok\r\n"), &default).unwrap(),
            "envtok"
        );
        cfg.token_file = Some(file);
        assert_eq!(
            read_token_from(&cfg, Some("envtok"), &default).unwrap(),
            "abc123"
        );
        cfg.token_file = None;
        let e = read_token_from(&cfg, None, &t.path().join("missing")).unwrap_err();
        assert!(e.contains(TOKEN_ENV));
        let empty = t.path().join("empty.txt");
        std::fs::write(&empty, "\r\n").unwrap();
        cfg.token_file = Some(empty);
        assert!(read_token_from(&cfg, None, &default).is_err());
    }

    #[test]
    fn extracts_job_ids() {
        assert_eq!(job_ids(&json!({"data": {"jobId": "a"}})), ["a"]);
        assert_eq!(
            job_ids(&json!({"data": {"jobIds": ["b", "c"], "x": {"job_id": "d"}}})),
            ["b", "c", "d"]
        );
        assert!(job_ids(&json!({"ok": true})).is_empty());
    }

    #[test]
    fn audio_submit_against_a_fake_portal_never_leaks_the_token() {
        let srv = fake::serve(vec![fake::ok_json(
            r#"{"data":{"jobIds":["aud-9","aud-10"]}}"#,
        )]);
        let cfg = GenerationConfig {
            allowed: true,
            portal_url: srv.url.clone(),
            ..GenerationConfig::default()
        };
        let req = parse_request(
            &json!({"kind": "audio", "target_project": "PrjEfz", "prompt": "pop", "variations": 2}),
        )
        .unwrap();
        let out = submit(&cfg, &req, "SECRET-TOKEN").unwrap();
        assert_eq!(out["job_ids"], json!(["aud-10", "aud-9"]));
        assert!(!out.to_string().contains("SECRET-TOKEN"));
        let raw = srv.requests.lock().unwrap()[0].clone();
        assert!(raw.starts_with("POST /api/audioJobs:submit HTTP/1.1"));
        assert!(raw.contains("Authorization: Bearer SECRET-TOKEN"));
        assert!(raw.contains("\"targetProject\":\"PrjEfz\""));
    }

    #[test]
    fn mesh_submit_uploads_then_submits_a_batch() {
        let t = Tmp::new();
        let png = t.path().join("lamp.png");
        std::fs::write(&png, b"\x89PNGdata").unwrap();
        let srv = fake::serve(vec![
            fake::ok_json(r#"{"data":{"path":"uploads/lamp-1.png"}}"#),
            fake::ok_json(r#"{"data":{"jobId":"mesh-5"}}"#),
        ]);
        let cfg = GenerationConfig {
            allowed: true,
            portal_url: srv.url.clone(),
            ..GenerationConfig::default()
        };
        let req = parse_request(&json!({"kind": "mesh", "target_project": "PrjEfz", "image_path": png.to_string_lossy(), "quality": "high", "smart_resize": false})).unwrap();
        let out = submit(&cfg, &req, "tok").unwrap();
        assert_eq!(out["job_ids"], json!(["mesh-5"]));
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(reqs[0].starts_with("POST /api/meshJobs:uploadImage HTTP/1.1"));
        assert!(reqs[0].contains("name=\"image\""));
        assert!(reqs[1].starts_with("POST /api/meshJobs:submitBatch HTTP/1.1"));
        assert!(reqs[1].contains("\"source_image\":\"uploads/lamp-1.png\""));
        assert!(reqs[1].contains("\"quality\":\"high\""));
        assert!(reqs[1].contains("\"smart_resize\":false"));
        assert!(reqs[1].contains("\"targetProject\":\"PrjEfz\""));
    }

    #[test]
    fn portal_errors_are_reported_without_secrets() {
        let srv = fake::serve(vec!["HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunauthorized".to_string()]);
        let cfg = GenerationConfig {
            allowed: true,
            portal_url: srv.url.clone(),
            ..GenerationConfig::default()
        };
        let req = parse_request(&json!({"kind": "audio", "target_project": "P", "prompt": "pop"}))
            .unwrap();
        let e = submit(&cfg, &req, "SECRET-TOKEN").unwrap_err();
        assert!(e.contains("401"));
        assert!(!e.contains("SECRET-TOKEN"));
    }
}
