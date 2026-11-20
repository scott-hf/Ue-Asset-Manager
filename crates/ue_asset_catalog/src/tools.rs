//! The MCP tools: what an agent can ask of the catalog, and the guarded import path.
//!
//! [`App`] owns the catalog, the registered projects and the import plans of this session.
//! `call_tool` never panics on bad input: argument problems and failures come back as an error
//! [`ToolOutput`] (the protocol layer turns that into an `isError` result).

use crate::b64;
use crate::bundles::{BundleConfig, PrepareJob};
use crate::catalog::{Asset, Catalog, LoadOptions, LoadReport, Pack, PackStatus, SourceKind};
use crate::generated::{self, FetchReport, PortalConfig};
use crate::generation::{self, GenerationConfig};
use crate::import::{self, ImportPlan};
use crate::release::{format_version, parse_version};
use crate::search::SearchQuery;
use crate::tags;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use ue_asset_library::{closure, Kind};

pub const SERVER_NAME: &str = "ue_asset_mcp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Automatic rescan check interval (on tool calls, never a background thread).
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_PLANS: usize = 32;
const MAX_IMPORT_IDS: usize = 500;
/// Cap of the absolute path lists in plan/import results.
const MAX_PATHS: usize = 2000;

/// Absolute, forward-slashed path of a project-relative file.
fn abs_path(root: &Path, rel: &str) -> String {
    fwd(&root.join(rel))
}

/// Forward-slashed text of a path.
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

pub const INSTRUCTIONS: &str = "\
This server is an offline catalog of every asset inside a library of Unreal Engine asset packs \
(zip archives that you never need to open). Workflow: (1) search_assets with plain words \
(\"grass\", \"stone wall\", \"wooden door\") to find existing meshes, materials, textures, \
blueprints, Niagara systems, animations and sounds; (2) get_asset / get_thumbnail to inspect a \
hit (ids look like `<packId>:<gamePath>`, e.g. `1a2b3c4d5e6f:/Game/Grass/SM_Grass_03`); \
(3) to use it in an Unreal project call plan_import (a dry run: files, conflicts, engine-version \
verdict, missing external dependencies), then import_assets with the returned plan id. \
Imports only write into projects that were registered when the server started (list_projects) \
and never overwrite files unless you pass overwrite=true (then files are backed up). \
list_packs / get_pack / list_tags help you browse; refresh_catalog rescans the library.";

/// The `initialize` instructions of the read-only (hosted) profile.
pub const INSTRUCTIONS_READ_ONLY: &str = "\
This server is a read-only offline catalog of every asset inside a library of Unreal Engine asset \
packs (zip archives that you never need to open). Workflow: (1) search_assets with plain words \
(\"grass\", \"stone wall\", \"wooden door\") to find existing meshes, materials, textures, \
blueprints, Niagara systems, animations and sounds; (2) get_asset / get_thumbnail to inspect a \
hit (ids look like `<packId>:<gamePath>`); list_packs / get_pack / list_tags help you browse. \
This server never writes anything: it cannot import into a project.";

/// The `initialize` instructions of the hosted profile (read-only plus bundles).
pub const INSTRUCTIONS_HOSTED: &str = "\
This server is a read-only offline catalog of every asset inside a library of Unreal Engine asset \
packs (zip archives that you never need to open). Workflow: (1) search_assets with plain words \
(\"grass\", \"stone wall\", \"wooden door\") to find existing meshes, materials, textures, \
blueprints, Niagara systems, animations and sounds; (2) get_asset / get_thumbnail to inspect a \
hit (ids look like `<packId>:<gamePath>`); list_packs / get_pack / list_tags help you browse; \
(3) to use assets in your own Unreal project call prepare_import with the asset ids and your \
project's engine version: the server builds one verified bundle (a zip plus a manifest with a \
SHA-256 per file) that your client downloads from the returned `download` path and applies to \
your project. This server never writes into any project and cannot import by itself.";

/// A project the server is allowed to write into.
pub use crate::import::ProjectTarget as Registered;

#[derive(Clone, Debug, Default)]
pub struct AppConfig {
    pub libraries: Vec<PathBuf>,
    /// `.uproject` files or project roots.
    pub projects: Vec<PathBuf>,
    pub load: LoadOptions,
    /// Where installs are logged (`None` = not logged).
    pub install_log: Option<PathBuf>,
    /// The pipeline portal for the read-only generated-assets source (`None` = off).
    pub portal: Option<PortalConfig>,
    /// Pack family slugs hidden from every `search_assets` call (`--exclude-pack`).
    pub exclude_packs: Vec<String>,
    /// `request_generation` settings (off unless `allowed`).
    pub generation: GenerationConfig,
    /// Hosted profile: tools that write or change state are hidden and refused (`READ_ONLY`).
    pub read_only: bool,
    /// Never rescan from the request path (the HTTP server rescans on a background thread).
    pub no_auto_refresh: bool,
    /// Hosted bundles (`prepare_import` + `GET /bundles/<id>`); `None` = the tool is hidden.
    pub bundle: Option<BundleConfig>,
}

/// Tools refused (and hidden) in the read-only profile. The generation pair stays available only
/// when `--allow-generation` was given.
const WRITE_TOOLS: [&str; 4] = [
    "import_assets",
    "plan_import",
    "list_projects",
    "refresh_catalog",
];
const GENERATION_TOOLS: [&str; 2] = ["request_generation", "generation_status"];

impl AppConfig {
    /// Is `name` unavailable in this configuration?
    pub fn tool_blocked(&self, name: &str) -> bool {
        (name == "prepare_import" && self.bundle.is_none())
            || (self.read_only
                && (WRITE_TOOLS.contains(&name)
                    || (GENERATION_TOOLS.contains(&name) && !self.generation.allowed)))
    }
}

/// Load the catalog plus the optional generated-assets source (no `App` needed, so a background
/// thread can do this without any lock).
pub fn load_catalog(cfg: &AppConfig) -> (Catalog, LoadReport, FetchReport) {
    let (mut catalog, report) = Catalog::load(&cfg.libraries, &cfg.load);
    let mut fetch = FetchReport::default();
    if let Some(portal) = &cfg.portal {
        let (items, rep) = generated::load(portal);
        catalog.add_generated(&items);
        fetch = rep;
    }
    (catalog, report, fetch)
}

pub enum Content {
    Text(String),
    Image { data: String, mime: String },
}

pub struct ToolOutput {
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl ToolOutput {
    fn json(v: &Value) -> ToolOutput {
        ToolOutput {
            content: vec![Content::Text(v.to_string())],
            is_error: false,
        }
    }

    pub fn error(msg: impl Into<String>) -> ToolOutput {
        ToolOutput {
            content: vec![Content::Text(format!("error: {}", msg.into()))],
            is_error: true,
        }
    }

    /// The text of the first text block (tests and the self test).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .find_map(|c| match c {
                Content::Text(t) => Some(t.clone()),
                Content::Image { .. } => None,
            })
            .unwrap_or_default()
    }
}

/// A plan made by `plan_import` in this session, kept until applied (single use).
struct Plan {
    id: String,
    plan: ImportPlan,
}

pub struct App {
    pub catalog: Catalog,
    pub cfg: AppConfig,
    pub projects: Vec<Registered>,
    pub load_report: LoadReport,
    pub generated_report: FetchReport,
    pub warnings: Vec<String>,
    submissions_used: usize,
    plans: Vec<Plan>,
    plan_counter: u64,
    last_check: Instant,
}

// ---------------------------------------------------------------------------------------------
// argument helpers
// ---------------------------------------------------------------------------------------------

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}

fn opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match arg(args, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone()).filter(|s| !s.trim().is_empty())),
        Some(_) => Err(format!("argument `{key}` must be a string")),
    }
}

fn req_str(args: &Value, key: &str) -> Result<String, String> {
    opt_str(args, key)?.ok_or_else(|| format!("missing required argument `{key}`"))
}

fn opt_usize(args: &Value, key: &str) -> Result<Option<usize>, String> {
    match arg(args, key) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| format!("argument `{key}` must be a non-negative integer")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("argument `{key}` must be a non-negative integer")),
        Some(_) => Err(format!("argument `{key}` must be a non-negative integer")),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match arg(args, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(_) => Err(format!("argument `{key}` must be true or false")),
    }
}

/// An array of strings, or one string (comma separated allowed).
fn str_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match arg(args, key) {
        None => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .ok_or_else(|| format!("argument `{key}` must be an array of strings"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.into_iter().filter(|s| !s.is_empty()).collect()),
        Some(_) => Err(format!("argument `{key}` must be an array of strings")),
    }
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Content => "content",
        Kind::Plugin => "plugin",
        Kind::Project => "project",
    }
}

fn status_str(s: &PackStatus) -> String {
    match s {
        PackStatus::Indexed => "indexed".to_string(),
        PackStatus::NotIndexable(w) => format!("not_indexable: {w}"),
        PackStatus::Failed(e) => format!("failed: {e}"),
    }
}

pub use crate::import::norm_path;

/// Validate a `--project` value (a `.uproject` file or a project root) and read its engine.
pub fn register_project(path: &Path) -> Result<Registered, String> {
    Registered::from_path(path)
}

// ---------------------------------------------------------------------------------------------
// tool definitions
// ---------------------------------------------------------------------------------------------

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
        }
    })
}

/// The `tools/list` payload. Descriptions are written for a model that has never seen the library.
pub fn tool_definitions() -> Vec<Value> {
    let id_desc = "Asset id exactly as returned by search_assets/get_asset: `<packId>:<gamePath>`, \
                   e.g. `1a2b3c4d5e6f:/Game/Grass/Meshes/SM_Grass_03`. The packId is the first 12 hex \
                   characters of the pack's hash; the gamePath is the asset's Unreal path.";
    vec![
        tool(
            "search_assets",
            "START HERE. Search every asset in the offline Unreal Engine asset library (hundreds of \
             purchased or downloaded asset packs: static/skeletal meshes, materials, textures, \
             blueprints, Niagara VFX, animations, sounds, levels) without opening any archive. Use it \
             to find an existing asset before creating or buying one. `query` is plain words matched \
             against asset names, auto-generated tags, folder names and pack names; ALL words must \
             match and the last word may be a prefix (`grass pat` finds `SM_GrassPatch`). Common \
             synonyms are tagged, so `foliage` or `stone` also find grass or rocks. Words like \
             `map:normal` or `class:staticmesh` filter by tag (see list_tags for the vocabulary). \
             Results are ranked (exact name first) and each has an `id` to pass to get_asset, \
             get_thumbnail or plan_import. An asset that exists in several releases of the same pack \
             (different engine versions) is listed once with all `engine_versions`. Read-only.",
            json!({
                "query": {"type": "string", "description": "Free text, e.g. `wooden door`, `grass`, `rock map:normal`. Empty = only filters."},
                "kind": {"type": "string", "description": "Asset type: staticmesh, skeletalmesh (`mesh` = both), material, texture, blueprint, niagara, animation, sound, level, other; or pack kind: content, project."},
                "class": {"type": "string", "description": "Exact Unreal class, e.g. StaticMesh, SkeletalMesh, Material, MaterialInstanceConstant, Texture2D, NiagaraSystem, SoundWave, Blueprint."},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags that must all be present, e.g. [\"map:normal\", \"engine:5.3\"]."},
                "exclude_packs": {"type": "array", "items": {"type": "string"}, "description": "Pack slugs (see list_packs) to leave out of the results."},
                "pack": {"type": "string", "description": "Restrict to a pack: its id (hex prefix), slug, or part of its name (see list_packs)."},
                "engine_version": {"type": "string", "description": "Only assets that have a release for this Unreal version or older, e.g. `5.4`."},
                "has_thumbnail": {"type": "boolean", "description": "Only assets with (true) or without (false) an editor thumbnail."},
                "limit": {"type": "integer", "description": "Max results, default 20, max 100."},
                "offset": {"type": "integer", "description": "Skip this many results (paging)."}
            }),
            &[],
        ),
        tool(
            "get_asset",
            "Full details of one asset: Unreal class, kind, size, all tags, the pack it comes from and \
             every release (engine version) that contains it, the other assets in the same pack it \
             directly depends on (`dependencies_in_pack`, with their ids), how many assets a plan_import \
             with include_deps would pull in, and `external_dependencies` (referenced assets that are NOT \
             in the pack and would be missing in your project). Read-only.",
            json!({"id": {"type": "string", "description": id_desc}}),
            &["id"],
        ),
        tool(
            "list_packs",
            "List the asset packs in the library (a pack = one product bought/downloaded, e.g. `Medieval \
             Village`). Releases of the same pack for different engine versions are grouped. Each entry \
             shows pack ids (use them in the `pack` filter of search_assets or in get_pack), engine \
             versions, asset counts and sizes. `plugin` packs are listed but flagged `indexable: false` \
             (their contents cannot be browsed or imported here). Read-only.",
            json!({
                "query": {"type": "string", "description": "Only packs whose name contains this text."},
                "kind": {"type": "string", "description": "content, project or plugin."},
                "limit": {"type": "integer", "description": "Max packs, default 50, max 200."}
            }),
            &[],
        ),
        tool(
            "get_pack",
            "Details of one pack: its releases per engine version, marketplace link, how many assets of \
             each kind and class it holds, its top-level folders, and packages it references outside \
             itself. `id` is a pack id from list_packs/search_assets, a slug, or an unambiguous part of \
             the name. Read-only.",
            json!({"id": {"type": "string", "description": "Pack id (hex prefix), slug or name."}}),
            &["id"],
        ),
        tool(
            "list_tags",
            "The tag vocabulary with counts (facets), most common first. Tags are generated offline from \
             asset names, folders and classes: plain words (`grass`, `foliage`), `class:staticmesh`, \
             `type:staticmesh`, `type:skeletalmesh`, `map:normal` (texture maps), `lod`, `variant:03`, `engine:5.3`, `kind:content`, \
             `pack:<slug>`. Use a prefix such as `class:` or `map:` to list one namespace, or the first \
             letters of a word. Read-only.",
            json!({
                "prefix": {"type": "string", "description": "Only tags starting with this, e.g. `class:` or `gra`."},
                "limit": {"type": "integer", "description": "Max tags, default 50, max 500."}
            }),
            &[],
        ),
        tool(
            "get_thumbnail",
            "The editor thumbnail Unreal saved inside the asset (a small PNG/JPEG), returned as an image, \
             read lazily from the pack. Only for assets with `has_thumbnail: true`; cooked assets have \
             none. Read-only.",
            json!({"id": {"type": "string", "description": id_desc}}),
            &["id"],
        ),
        tool(
            "list_projects",
            "The Unreal projects this server may import into (registered at server start with \
             --project), with their engine version. `project` arguments of plan_import/import_assets must \
             name one of these (by name or path). Read-only.",
            json!({}),
            &[],
        ),
        tool(
            "plan_import",
            "DRY RUN of copying assets into one of your Unreal projects; writes nothing. Picks, for each \
             pack, the release that fits the project's engine (exact, else the highest not above it, \
             else the lowest with a warning: assets can be upgraded but never downgraded), adds the \
             assets they depend on (include_deps, default true), and reports the files, total size, \
             existing project files that clash, the engine-version verdict, the absolute `paths_to_write`, \
             and referenced assets that are missing from the pack. Returns a `plan_id`; show the plan to the user, then call \
             import_assets with that id to apply it.",
            json!({
                "project": {"type": "string", "description": "A registered project: its name or its .uproject/root path (see list_projects)."},
                "asset_ids": {"type": "array", "items": {"type": "string"}, "description": "Asset ids from search_assets (any mix of packs)."},
                "include_deps": {"type": "boolean", "description": "Also import the in-pack assets these depend on. Default true."}
            }),
            &["project", "asset_ids"],
        ),
        tool(
            "import_assets",
            "APPLY a plan made by plan_import in this session: copies the planned files from the library \
             into the project (byte-exact, only inside the project folder). It refuses to run without a \
             valid `confirm_plan_id`, or when `project`/`asset_ids` differ from the plan. Existing files \
             are skipped unless overwrite=true, in which case they are first moved to \
             `<project>/Saved/EAM_Backups/<timestamp>/`. The plan id is single use. The result lists \
             `written_paths` and `backup_paths` (absolute, forward-slashed): after importing, stage \
             these paths with your version-control tool if the project is under version control (this \
             server never runs version-control commands).",
            json!({
                "project": {"type": "string", "description": "Same project as in the plan."},
                "asset_ids": {"type": "array", "items": {"type": "string"}, "description": "Same asset ids as in the plan."},
                "overwrite": {"type": "boolean", "description": "Replace existing files (with backup). Default false = skip existing."},
                "confirm_plan_id": {"type": "string", "description": "The plan_id returned by plan_import."}
            }),
            &["project", "asset_ids", "confirm_plan_id"],
        ),
        tool(
            "prepare_import",
            "Hosted server only. Build ONE verified bundle of existing library assets for use in YOUR \
             Unreal project: the selected assets, their companion files (.uexp/.ubulk) and, with \
             include_deps (default true), the assets they depend on inside the same pack, as a zip \
             with a manifest (SHA-256 per file). Picks for each pack the release that fits \
             `project_engine_version` (exact, else the highest not above it, else the lowest with a \
             warning: assets can be upgraded but never downgraded). Writes nothing into any project: \
             the result has a `download` path (`/bundles/<bundle_id>`, same bearer token) that your \
             client fetches within `expires_at` and applies to the project. The result also lists \
             `files`, `verdict`, `warnings`, `omitted_deps` (in-pack dependencies not included) and \
             `external_missing` (referenced packages that are not in the pack and would be missing). \
             Errors: CAP_EXCEEDED (too big; names size and cap), CONFLICT (two packs fill one path \
             with different bytes), BUSY (another bundle is being built; retry), NOT_FOUND.",
            json!({
                "asset_ids": {"type": "array", "items": {"type": "string"}, "description": "Asset ids from search_assets (any mix of packs), 1 to 500."},
                "project_engine_version": {"type": "string", "description": "Engine version of your project, e.g. `5.4`."},
                "include_deps": {"type": "boolean", "description": "Also bundle the in-pack assets these depend on. Default true."}
            }),
            &["asset_ids", "project_engine_version"],
        ),
        tool(
            "request_generation",
            "LAST RESORT: ask the mesh/sound generation pipeline to make an asset the library does not \
             have. ALWAYS run search_assets first (generated and pack assets are both searched) and only \
             generate when nothing fits. Disabled unless the server was started with --allow-generation \
             (it then says so). `target_project` is REQUIRED: the registered Unreal project whose editor \
             will import the result. kind=audio needs `prompt` (what the sound should be), optional \
             category (Music, Instrument, SFX, One-shot; default SFX), name, duration (seconds), \
             variations (1-12, each is one job), use_reprompt. kind=mesh is image-to-3D only: \
             `image_path` (a png/jpg/webp on this machine) is required, optional output_name, quality \
             (fast, balanced, high) and smart_resize. Costs GPU time and is capped per session. Returns \
             job ids; poll generation_status.",
            json!({
                "kind": {"type": "string", "enum": ["audio", "mesh"]},
                "target_project": {"type": "string", "description": "Unreal project name that will receive the result (required)."},
                "prompt": {"type": "string", "description": "audio: what the sound is."},
                "category": {"type": "string", "description": "audio: Music, Instrument, SFX or One-shot."},
                "name": {"type": "string", "description": "audio: output name (letters, digits, _ - .)."},
                "duration": {"type": "number", "description": "audio: seconds."},
                "variations": {"type": "integer", "description": "audio: 1-12 variations (each counts as a job)."},
                "use_reprompt": {"type": "boolean"},
                "image_path": {"type": "string", "description": "mesh: source image file (required for mesh)."},
                "output_name": {"type": "string", "description": "mesh: output name."},
                "quality": {"type": "string", "enum": ["fast", "balanced", "high"]},
                "smart_resize": {"type": "boolean", "description": "mesh: let the pipeline estimate real-world size (default true)."}
            }),
            &["kind", "target_project"],
        ),
        tool(
            "generation_status",
            "State of a generation job submitted with request_generation (queued, running, completed, \
             failed), read from the pipeline queue. Read-only, needs no token.",
            json!({"job_id": {"type": "string", "description": "A job id returned by request_generation."}}),
            &["job_id"],
        ),
        tool(
            "refresh_catalog",
            "Rescan the library folders now (new packs dropped into the library, rebuilt indexes). The \
             server also checks for changes automatically, at most every 30 seconds, when a tool is \
             called. Returns the catalog counts.",
            json!({}),
            &[],
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// the app
// ---------------------------------------------------------------------------------------------

impl App {
    /// Load the catalog from the configured library dirs and register the projects.
    pub fn new(cfg: AppConfig) -> App {
        let (catalog, report) = Catalog::load(&cfg.libraries, &cfg.load);
        let mut app = App::with_catalog(catalog, cfg);
        app.load_report = report;
        app.load_generated();
        app
    }

    /// The `tools/list` payload of this configuration (blocked tools are hidden).
    pub fn visible_tools(&self) -> Vec<Value> {
        tool_definitions()
            .into_iter()
            .filter(|t| {
                t.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| !self.cfg.tool_blocked(n))
            })
            .collect()
    }

    /// The `initialize` instructions of this configuration.
    pub fn instructions(&self) -> &'static str {
        if self.cfg.read_only && self.cfg.bundle.is_some() {
            INSTRUCTIONS_HOSTED
        } else if self.cfg.read_only {
            INSTRUCTIONS_READ_ONLY
        } else {
            INSTRUCTIONS
        }
    }

    /// Swap in a freshly loaded catalog. An empty load never replaces a non-empty one (keep the
    /// last good snapshot); returns whether the swap happened.
    pub fn install_catalog(
        &mut self,
        catalog: Catalog,
        report: LoadReport,
        generated: FetchReport,
    ) -> bool {
        if catalog.assets.is_empty() && !self.catalog.assets.is_empty() {
            return false;
        }
        self.catalog = catalog;
        self.load_report = report;
        self.generated_report = generated;
        self.last_check = Instant::now();
        true
    }

    /// Add the finished pipeline jobs (read-only source) when a portal is configured. A portal
    /// that cannot be reached falls back to the cached list and never fails the load.
    pub fn load_generated(&mut self) {
        let Some(portal) = self.cfg.portal.clone() else {
            return;
        };
        let (items, report) = generated::load(&portal);
        self.catalog.add_generated(&items);
        self.generated_report = report;
    }

    /// Use a prebuilt catalog (tests).
    pub fn with_catalog(catalog: Catalog, cfg: AppConfig) -> App {
        let mut projects = Vec::new();
        let mut warnings = Vec::new();
        for p in &cfg.projects {
            match register_project(p) {
                Ok(r) => {
                    if !projects.iter().any(|x: &Registered| {
                        norm_path(&x.uproject.to_string_lossy())
                            == norm_path(&r.uproject.to_string_lossy())
                    }) {
                        projects.push(r);
                    }
                }
                Err(e) => warnings.push(format!("project not registered: {e}")),
            }
        }
        App {
            catalog,
            cfg,
            projects,
            load_report: LoadReport::default(),
            generated_report: FetchReport::default(),
            warnings,
            submissions_used: 0,
            plans: Vec::new(),
            plan_counter: 0,
            last_check: Instant::now(),
        }
    }

    /// Reload the catalog from the library dirs (cache + overlays make this cheap).
    pub fn reload(&mut self) {
        let (catalog, report) = Catalog::load(&self.cfg.libraries, &self.cfg.load);
        self.catalog = catalog;
        self.load_report = report;
        self.load_generated();
        self.last_check = Instant::now();
    }

    fn maybe_refresh(&mut self) {
        if self.cfg.no_auto_refresh || self.last_check.elapsed() < REFRESH_INTERVAL {
            return;
        }
        self.last_check = Instant::now();
        if self.catalog.needs_reload() {
            eprintln!("[ue_asset_mcp] library changed, reloading catalog");
            self.reload();
        }
    }

    /// Run a tool. `None` = no such tool (a protocol error for the caller).
    pub fn call_tool(&mut self, name: &str, args: &Value) -> Option<ToolOutput> {
        let known = [
            "search_assets",
            "get_asset",
            "list_packs",
            "get_pack",
            "list_tags",
            "get_thumbnail",
            "list_projects",
            "plan_import",
            "import_assets",
            "refresh_catalog",
            "request_generation",
            "generation_status",
            "prepare_import",
        ];
        if !known.contains(&name) {
            return None;
        }
        if name == "prepare_import" {
            // Built by the HTTP server outside the catalog lock (`App::prepare_job`).
            return Some(ToolOutput::error(
                "NOT_AVAILABLE: prepare_import is served only by `ue_asset_mcp --http` with --bundle-dir",
            ));
        }
        if self.cfg.tool_blocked(name) {
            return Some(ToolOutput::error(format!(
                "READ_ONLY: `{name}` is not available on this read-only server; it never writes or changes state. Use search_assets, get_asset, list_packs, get_pack, list_tags or get_thumbnail."
            )));
        }
        if name != "refresh_catalog" {
            self.maybe_refresh();
        }
        let empty = Value::Object(Map::new());
        let args = if args.is_null() { &empty } else { args };
        if !args.is_object() {
            return Some(ToolOutput::error("`arguments` must be a JSON object"));
        }
        let r = match name {
            "search_assets" => self.search_assets(args),
            "get_asset" => self.get_asset(args),
            "list_packs" => self.list_packs(args),
            "get_pack" => self.get_pack(args),
            "list_tags" => self.list_tags(args),
            "get_thumbnail" => return Some(self.get_thumbnail(args)),
            "list_projects" => Ok(self.list_projects()),
            "plan_import" => self.plan_import(args),
            "import_assets" => self.import_assets(args),
            "refresh_catalog" => Ok(self.refresh_catalog()),
            "request_generation" => self.request_generation(args),
            "generation_status" => self.generation_status(args),
            _ => return None,
        };
        Some(match r {
            Ok(v) => ToolOutput::json(&v),
            Err(e) => ToolOutput::error(e),
        })
    }

    /// Resolve a `prepare_import` request against the current catalog (cheap, no file access).
    /// The caller runs [`crate::bundles::prepare`] with the result after releasing its lock.
    /// Errors are the tool error text (`CODE: message` or a plain argument message).
    pub fn prepare_job(&self, args: &Value, catalog_generation: u64) -> Result<PrepareJob, String> {
        let ids = str_list(args, "asset_ids")?;
        if ids.is_empty() || ids.len() > MAX_IMPORT_IDS {
            return Err(format!(
                "`asset_ids` needs 1 to {MAX_IMPORT_IDS} ids (see search_assets)"
            ));
        }
        let ver = req_str(args, "project_engine_version")?;
        let engine = parse_version(&ver)
            .ok_or_else(|| format!("project_engine_version `{ver}` is not a version like 5.4"))?;
        let include_deps = opt_bool(args, "include_deps")?.unwrap_or(true);
        let assets = import::resolve_assets(&self.catalog, &ids, MAX_IMPORT_IDS)
            .map_err(|e| format!("NOT_FOUND: {e}"))?;
        let plan = import::plan_bundle(&self.catalog, &assets, engine, include_deps)?;
        Ok(PrepareJob {
            plan,
            catalog_generation,
        })
    }

    // ----- lookups ---------------------------------------------------------------------------

    fn resolve_asset(&self, id: &str) -> Result<usize, String> {
        import::resolve_asset(&self.catalog, id)
    }

    fn resolve_project(&self, s: &str) -> Result<&Registered, String> {
        if self.projects.is_empty() {
            return Err(
                "no projects are registered: start ue_asset_mcp with --project <path-to-.uproject>"
                    .to_string(),
            );
        }
        let key = norm_path(s);
        let by_path: Vec<&Registered> = self
            .projects
            .iter()
            .filter(|p| {
                norm_path(&p.root.to_string_lossy()) == key
                    || norm_path(&p.uproject.to_string_lossy()) == key
            })
            .collect();
        if let Some(p) = by_path.first() {
            return Ok(p);
        }
        let by_name: Vec<&Registered> = self
            .projects
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(s.trim()))
            .collect();
        match by_name.len() {
            1 => Ok(by_name[0]),
            0 => Err(format!(
                "project `{s}` is not a registered project, so nothing will be written there. Registered: {}",
                self.projects
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.uproject.display()))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
            _ => Err(format!(
                "project name `{s}` is ambiguous; use the full .uproject path"
            )),
        }
    }

    // ----- JSON shapes -----------------------------------------------------------------------

    fn engine_versions(&self, a: &Asset) -> Vec<String> {
        let mut v: Vec<String> = a
            .releases
            .iter()
            .filter_map(|r| self.catalog.packs[r.pack].engine_label())
            .collect();
        v.dedup();
        let mut seen = BTreeSet::new();
        v.retain(|x| seen.insert(x.clone()));
        v
    }

    fn hit_json(&self, a: &Asset, score: Option<f32>) -> Value {
        let fam = &self.catalog.families[a.family];
        let tags: Vec<&String> = a
            .tags
            .iter()
            .filter(|t| {
                !t.starts_with("type:") && !t.starts_with("class:") && !t.starts_with("engine:")
            })
            .take(12)
            .collect();
        let mut v = json!({
            "id": a.id,
            "name": a.name,
            "game_path": a.game_path,
            "class": a.class,
            "kind": a.kind,
            "pack": fam.name,
            "pack_kind": kind_str(fam.kind),
            "engine_versions": self.engine_versions(a),
            "tags": tags,
            "size_bytes": a.size,
            "has_thumbnail": a.has_thumbnail,
        });
        if let Some(s) = score {
            v["score"] = json!((s * 10.0).round() / 10.0);
        }
        if let Some(g) = &a.generated {
            v["source"] = json!("generated");
            v["project"] = json!(g.target_project);
            v["object_path"] = json!(g.unreal_object_path);
            v["output_path"] = json!(g.output_path);
            v["pack_kind"] = json!("generated");
            v["size_bytes"] = Value::Null;
        } else if let Some(d) = &a.description {
            v["description"] = json!(d);
        }
        v
    }

    fn pack_json(p: &Pack) -> Value {
        json!({
            "id": p.id,
            "file": p.entry.zip_path.file_name().map(|f| f.to_string_lossy().to_string()),
            "engine": p.engine_label(),
            "engine_source": p.engine_source,
            "status": status_str(&p.status),
            "indexable": !matches!(p.status, PackStatus::NotIndexable(_)),
            "packages": p.index.as_ref().map(|i| i.packages.len()),
            "size_bytes": p.entry.sidecar.total_uncompressed,
        })
    }

    // ----- read-only tools ---------------------------------------------------------------------

    fn search_assets(&self, args: &Value) -> Result<Value, String> {
        let engine_version = match opt_str(args, "engine_version")? {
            None => None,
            Some(s) => Some(
                parse_version(&s)
                    .ok_or_else(|| format!("engine_version `{s}` is not a version like 5.4"))?,
            ),
        };
        let q = SearchQuery {
            text: opt_str(args, "query")?.unwrap_or_default(),
            kind: opt_str(args, "kind")?,
            class: opt_str(args, "class")?,
            tags: str_list(args, "tags")?,
            pack: opt_str(args, "pack")?,
            exclude_packs: {
                let mut v = self.cfg.exclude_packs.clone();
                v.extend(str_list(args, "exclude_packs")?);
                v
            },
            engine_version,
            has_thumbnail: opt_bool(args, "has_thumbnail")?,
            limit: opt_usize(args, "limit")?.unwrap_or(20).clamp(1, 100),
            offset: opt_usize(args, "offset")?.unwrap_or(0),
        };
        let out = self.catalog.search(&q);
        let hits: Vec<Value> = out
            .hits
            .iter()
            .map(|h| self.hit_json(&self.catalog.assets[h.asset], Some(h.score)))
            .collect();
        let end = q.offset + hits.len();
        let mut v = json!({
            "total": out.total,
            "offset": q.offset,
            "returned": hits.len(),
            "hits": hits,
        });
        if end < out.total {
            v["next_offset"] = json!(end);
        }
        if !out.notes.is_empty() {
            v["notes"] = json!(out.notes);
        }
        if out.total == 0 && out.notes.is_empty() {
            v["notes"] = json!([
                "no matches; try fewer or simpler words, a synonym, or remove filters. list_tags shows the vocabulary."
            ]);
        }
        Ok(v)
    }

    fn get_asset(&self, args: &Value) -> Result<Value, String> {
        let id = req_str(args, "id")?;
        let ai = self.resolve_asset(&id)?;
        let a = &self.catalog.assets[ai];
        if let Some(g) = &a.generated {
            let mut v = self.hit_json(a, None);
            v["tags"] = json!(self.catalog.all_tags(a));
            v["job_id"] = json!(g.job_id);
            v["description"] = json!(g.description);
            v["category"] = json!(g.category);
            v["completed_at"] = json!(g.completed_at);
            v["details"] = g.details.clone();
            v["importable"] = json!(false);
            v["note"] = json!(
                "A finished job of the generation pipeline. It already lives in the project named in `project` (object_path) and on disk at `output_path`; import_assets does not handle generated assets."
            );
            return Ok(v);
        }
        let fam = &self.catalog.families[a.family];
        let rep = a.releases[0];
        let rep_pack = &self.catalog.packs[rep.pack];
        let pk = self
            .catalog
            .package(rep)
            .ok_or("asset index is inconsistent")?;
        let index = rep_pack.index.as_ref().ok_or("pack has no index")?;
        let classes: HashMap<&str, Option<&str>> = index
            .packages
            .iter()
            .map(|p| (p.game_path.as_str(), p.class.as_deref()))
            .collect();
        let deps: Vec<Value> = pk
            .deps
            .iter()
            .take(100)
            .map(|d| {
                let class = classes.get(d.as_str()).copied().flatten();
                json!({
                    "id": format!("{}:{}", rep_pack.id, d),
                    "name": tags::name_of(d),
                    "class": class,
                    "kind": tags::asset_kind(class, tags::split_prefix(tags::name_of(d)).0),
                })
            })
            .collect();
        let closure_n = closure(index, std::slice::from_ref(&pk.game_path))
            .len()
            .saturating_sub(1);
        let releases: Vec<Value> = a
            .releases
            .iter()
            .filter_map(|r| {
                let p = &self.catalog.packs[r.pack];
                let q = self.catalog.package(*r)?;
                Some(json!({
                    "pack_id": p.id,
                    "pack": p.display_name,
                    "engine": p.engine_label(),
                    "asset_id": format!("{}:{}", p.id, q.game_path),
                    "size_bytes": q.size,
                    "has_thumbnail": q.has_thumbnail,
                    "header_unreadable": q.header_unreadable,
                    "unreadable_reason": q.unreadable_reason,
                }))
            })
            .collect();
        let mut v = self.hit_json(a, None);
        v["tags"] = json!(self.catalog.all_tags(a));
        v["description"] = json!(a.description);
        v["pack_id"] = json!(rep_pack.id);
        v["pack_slug"] = json!(fam.slug);
        v["releases"] = json!(releases);
        v["dependencies_in_pack"] = json!(deps);
        v["dependencies_in_pack_total"] = json!(pk.deps.len());
        v["transitive_dependencies_in_pack"] = json!(closure_n);
        v["external_dependencies"] = json!(pk.external.iter().take(100).collect::<Vec<_>>());
        v["external_dependencies_total"] = json!(pk.external.len());
        v["companion_files"] = json!(pk.companions.len());
        // Any release with an unreadable header counts: never hidden.
        let bad = a
            .releases
            .iter()
            .filter_map(|r| self.catalog.package(*r))
            .find(|q| q.header_unreadable);
        v["header_unreadable"] = json!(bad.is_some());
        v["unreadable_reason"] = json!(bad.and_then(|q| q.unreadable_reason.clone()));
        if let Some(q) = bad {
            v["warnings"] = json!([format!(
                "the package header could not be read (reason: {}); its dependencies are unknown",
                q.unreadable_reason.as_deref().unwrap_or("unknown")
            )]);
        }
        v["file_in_pack"] = json!(pk.zip_path);
        v["next"] = json!(
            "get_thumbnail shows it; plan_import {project, asset_ids:[id]} previews copying it into a project."
        );
        Ok(v)
    }

    fn list_packs(&self, args: &Value) -> Result<Value, String> {
        let query = opt_str(args, "query")?.map(|s| s.to_lowercase());
        let kind = opt_str(args, "kind")?.map(|s| s.to_lowercase());
        let limit = opt_usize(args, "limit")?.unwrap_or(50).clamp(1, 200);
        let mut counts = vec![0usize; self.catalog.families.len()];
        for a in &self.catalog.assets {
            counts[a.family] += 1;
        }
        let mut rows: Vec<Value> = Vec::new();
        for (fi, f) in self.catalog.families.iter().enumerate() {
            if let Some(k) = &kind {
                let fk = if f.source == SourceKind::Generated {
                    "generated"
                } else {
                    kind_str(f.kind)
                };
                if fk != k.as_str() {
                    continue;
                }
            }
            if let Some(q) = &query {
                if !f.name.to_lowercase().contains(q.as_str()) && !f.slug.contains(q.as_str()) {
                    continue;
                }
            }
            let releases: Vec<Value> = f
                .packs
                .iter()
                .map(|&p| Self::pack_json(&self.catalog.packs[p]))
                .collect();
            rows.push(json!({
                "name": f.name,
                "slug": f.slug,
                "kind": if f.source == SourceKind::Generated { "generated" } else { kind_str(f.kind) },
                "source": if f.source == SourceKind::Generated { "generated" } else { "pack" },
                "indexable": f.kind != Kind::Plugin,
                "asset_count": counts[fi],
                "releases": releases,
            }));
        }
        let total = rows.len();
        rows.truncate(limit);
        let st = self.catalog.stats();
        Ok(json!({
            "total_packs": total,
            "returned": rows.len(),
            "library": {
                "pack_files": st.packs,
                "indexed": st.indexed_packs,
                "failed": st.failed_packs,
                "plugins_skipped": st.skipped_plugins,
                "assets": st.assets,
            },
            "packs": rows,
        }))
    }

    fn get_pack(&self, args: &Value) -> Result<Value, String> {
        let id = req_str(args, "id")?;
        let gen_family = self.catalog.families.iter().position(|f| {
            f.source == SourceKind::Generated && f.slug == id.trim().to_ascii_lowercase()
        });
        let fam = if let Some(p) = self.catalog.pack_by_id(&id) {
            self.catalog.packs[p].family
        } else if let Some(g) = gen_family {
            g
        } else {
            let mut fams: Vec<usize> = self
                .catalog
                .match_packs(&id)
                .into_iter()
                .map(|p| self.catalog.packs[p].family)
                .collect();
            fams.sort_unstable();
            fams.dedup();
            match fams.len() {
                0 => return Err(format!("no pack matches `{id}`; see list_packs")),
                1 => fams[0],
                _ => {
                    let names: Vec<String> = fams
                        .iter()
                        .take(10)
                        .map(|&f| {
                            format!(
                                "{} ({})",
                                self.catalog.families[f].name, self.catalog.families[f].slug
                            )
                        })
                        .collect();
                    return Err(format!(
                        "`{id}` matches {} packs: {}; use a pack id or the exact slug",
                        fams.len(),
                        names.join(", ")
                    ));
                }
            }
        };
        let f = &self.catalog.families[fam];
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        let mut by_class: BTreeMap<String, usize> = BTreeMap::new();
        let mut folders: BTreeMap<String, usize> = BTreeMap::new();
        for a in self.catalog.assets.iter().filter(|a| a.family == fam) {
            *by_kind.entry(a.kind).or_insert(0) += 1;
            *by_class
                .entry(a.class.clone().unwrap_or_else(|| "unknown".to_string()))
                .or_insert(0) += 1;
            let comps: Vec<&str> = a.game_path.split('/').filter(|c| !c.is_empty()).collect();
            let top = comps
                .iter()
                .take(comps.len().saturating_sub(1).min(2))
                .copied()
                .collect::<Vec<_>>()
                .join("/");
            *folders.entry(format!("/{top}")).or_insert(0) += 1;
        }
        let top_n = |m: BTreeMap<String, usize>, n: usize| -> Value {
            let mut v: Vec<(String, usize)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v.truncate(n);
            Value::Object(v.into_iter().map(|(k, c)| (k, json!(c))).collect())
        };
        let releases: Vec<Value> = f
            .packs
            .iter()
            .map(|&p| {
                let pk = &self.catalog.packs[p];
                let sc = &pk.entry.sidecar;
                let mut v = Self::pack_json(pk);
                v["entry_count"] = json!(sc.entry_count);
                v["marketplace_url"] = json!(sc.marketplace_url);
                v["friendly_name"] = json!(sc.friendly_name);
                v["plugin_name"] = json!(sc.plugin_name);
                v["content_folder"] = json!(sc.content_folder);
                v["sanitized_at"] = json!(sc.sanitized_at);
                v["external_refs_outside_pack"] = json!(sc.external_refs.len());
                v["other_files"] = json!(pk.index.as_ref().map(|i| i.other_files.len()));
                v
            })
            .collect();
        Ok(json!({
            "name": f.name,
            "slug": f.slug,
            "kind": kind_str(f.kind),
            "tags": f.tags,
            "releases": releases,
            "assets_by_kind": by_kind,
            "assets_by_class": top_n(by_class, 20),
            "top_folders": top_n(folders, 20),
        }))
    }

    fn list_tags(&self, args: &Value) -> Result<Value, String> {
        let prefix = opt_str(args, "prefix")?.map(|s| s.to_lowercase());
        let limit = opt_usize(args, "limit")?.unwrap_or(50).clamp(1, 500);
        let mut rows: Vec<(&String, u32)> = match &prefix {
            Some(p) => self
                .catalog
                .tag_counts
                .range(p.clone()..)
                .take_while(|(k, _)| k.starts_with(p.as_str()))
                .map(|(k, c)| (k, *c))
                .collect(),
            None => self
                .catalog
                .tag_counts
                .iter()
                .map(|(k, c)| (k, *c))
                .collect(),
        };
        let total = rows.len();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        rows.truncate(limit);
        Ok(json!({
            "total_matching_tags": total,
            "tags": rows.iter().map(|(t, c)| json!({"tag": t, "count": c})).collect::<Vec<_>>(),
            "namespaces": ["class:", "type:", "map:", "variant:", "engine:", "kind:", "pack:"],
        }))
    }

    fn get_thumbnail(&self, args: &Value) -> ToolOutput {
        let r = (|| -> Result<ToolOutput, String> {
            let id = req_str(args, "id")?;
            let ai = self.resolve_asset(&id)?;
            let a = &self.catalog.assets[ai];
            if a.source != SourceKind::Pack {
                return Ok(ToolOutput {
                    content: vec![Content::Text(format!(
                        "{} is a generated asset and has no stored thumbnail",
                        a.name
                    ))],
                    is_error: false,
                });
            }
            if let Some(bytes) = self.catalog.thumbnail(&a.id)? {
                return Ok(ToolOutput {
                    content: vec![
                        Content::Text(format!(
                            "Editor thumbnail of {} ({} bytes)",
                            a.name,
                            bytes.len()
                        )),
                        Content::Image {
                            data: b64::encode(&bytes),
                            mime: b64::image_mime(&bytes).to_string(),
                        },
                    ],
                    is_error: false,
                });
            }
            Ok(ToolOutput {
                content: vec![Content::Text(format!(
                    "{} has no stored thumbnail (cooked or never opened in the editor)",
                    a.name
                ))],
                is_error: false,
            })
        })();
        r.unwrap_or_else(ToolOutput::error)
    }

    fn list_projects(&self) -> Value {
        json!({
            "projects": self.projects.iter().map(|p| json!({
                "name": p.name,
                "uproject": p.uproject.to_string_lossy(),
                "root": p.root.to_string_lossy(),
                "engine": p.engine.map(format_version),
                "engine_association": p.engine_raw,
            })).collect::<Vec<_>>(),
            "warnings": self.warnings,
            "note": "Only these projects can be imported into.",
        })
    }

    fn refresh_catalog(&mut self) -> Value {
        self.reload();
        let st = self.catalog.stats();
        json!({
            "stats": st,
            "load": self.load_report,
            "generated": {
                "meshes": self.generated_report.meshes,
                "sounds": self.generated_report.sounds,
                "from_cache": self.generated_report.from_cache,
                "warnings": self.generated_report.warnings,
            },
        })
    }

    // ----- generation -------------------------------------------------------------------------

    fn request_generation(&mut self, args: &Value) -> Result<Value, String> {
        let cfg = self.cfg.generation.clone();
        if !cfg.allowed {
            return Err(
                "generation is disabled on this server (it was not started with --allow-generation). Use search_assets to find an existing asset instead, or ask the user to enable generation."
                    .to_string(),
            );
        }
        let req = generation::parse_request(args)?;
        let n = req.job_count();
        if self.submissions_used + n > cfg.max_submissions {
            return Err(format!(
                "this session may submit at most {} generation job(s); {} already used and this request needs {}",
                cfg.max_submissions, self.submissions_used, n
            ));
        }
        let token = generation::read_token(&cfg)?;
        let out = generation::submit(&cfg, &req, &token)?;
        self.submissions_used += n;
        Ok(json!({
            "ok": true,
            "target_project": req.target_project(),
            "job_ids": out["job_ids"],
            "portal_response": out["portal_response"],
            "submissions_used": self.submissions_used,
            "submissions_left": cfg.max_submissions.saturating_sub(self.submissions_used),
            "next": "Generation takes minutes. Poll generation_status with a job id; once finished the asset appears in search_assets as `gen:<jobId>` after refresh_catalog.",
        }))
    }

    fn generation_status(&self, args: &Value) -> Result<Value, String> {
        let job = req_str(args, "job_id")?;
        let portal = self
            .cfg
            .portal
            .clone()
            .unwrap_or_else(|| PortalConfig::new(&self.cfg.generation.portal_url));
        match generated::fetch_job(&portal, &job)? {
            Some(row) => Ok(json!({"found": true, "job": row})),
            None => Ok(
                json!({"found": false, "note": "no such job (it may not be queued yet, or the id is wrong)"}),
            ),
        }
    }

    // ----- import ----------------------------------------------------------------------------

    fn next_plan_id(&mut self) -> String {
        use std::hash::{Hash, Hasher};
        self.plan_counter += 1;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            .hash(&mut h);
        self.plan_counter.hash(&mut h);
        std::process::id().hash(&mut h);
        format!("plan-{}-{:08x}", self.plan_counter, h.finish() as u32)
    }

    fn plan_import(&mut self, args: &Value) -> Result<Value, String> {
        let project = req_str(args, "project")?;
        let ids = str_list(args, "asset_ids")?;
        let include_deps = opt_bool(args, "include_deps")?.unwrap_or(true);
        let proj = self.resolve_project(&project)?.clone();
        let assets = import::resolve_assets(&self.catalog, &ids, MAX_IMPORT_IDS)?;
        let plan = import::plan_import(&self.catalog, &proj, &assets, include_deps)?;
        let totals = plan.totals();
        let pack_rows: Vec<Value> = plan
            .packs
            .iter()
            .map(|p| {
                json!({
                    "pack_id": p.pack_id,
                    "pack": p.pack_name,
                    "kind": kind_str(p.kind),
                    "pack_engine": p.pack_engine,
                    "release_choice": p.verdict,
                    "selected_assets": p.selected.len(),
                    "added_as_dependencies": p.dependencies.len(),
                    "dependency_ids": p.dependencies.iter().take(50).collect::<Vec<_>>(),
                    "files": p.files.len(),
                    "files_sample": p.files.iter().take(20).collect::<Vec<_>>(),
                    "paths_to_write": p.files.iter().take(MAX_PATHS).map(|f| abs_path(&proj.root, f)).collect::<Vec<_>>(),
                    "size_bytes": p.size_bytes,
                    "existing_files_in_project": p.existing_files.iter().take(50).collect::<Vec<_>>(),
                    "existing_files_total": p.existing_files.len(),
                    "external_dependencies_total": p.external.len(),
                    "external_dependencies": p.external.iter().take(50).collect::<Vec<_>>(),
                })
            })
            .collect();
        let plan_id = self.next_plan_id();
        let v = json!({
            "plan_id": plan_id,
            "project": {
                "name": proj.name,
                "uproject": proj.uproject.to_string_lossy(),
                "engine": proj.engine.map(format_version),
            },
            "include_deps": include_deps,
            "packs": pack_rows,
            "totals": {
                "assets_including_dependencies": totals.assets,
                "files": totals.files,
                "size_bytes": totals.size_bytes,
                "existing_files_in_project": totals.existing_files,
                "external_dependencies": totals.external,
            },
            "warnings": plan.warnings,
            "next": format!(
                "Nothing was written. To apply, call import_assets with the same project and asset_ids and confirm_plan_id=\"{plan_id}\". Existing files are skipped unless overwrite=true."
            ),
        });
        self.plans.push(Plan { id: plan_id, plan });
        if self.plans.len() > MAX_PLANS {
            self.plans.remove(0);
        }
        Ok(v)
    }

    fn import_assets(&mut self, args: &Value) -> Result<Value, String> {
        let project = req_str(args, "project")?;
        let ids = str_list(args, "asset_ids")?;
        let overwrite = opt_bool(args, "overwrite")?.unwrap_or(false);
        let Some(plan_id) = opt_str(args, "confirm_plan_id")? else {
            return Err(
                "import_assets requires `confirm_plan_id`: call plan_import first, review the plan, then pass its plan_id here"
                    .to_string(),
            );
        };
        let proj = self.resolve_project(&project)?.clone();
        let Some(pos) = self.plans.iter().position(|p| p.id == plan_id) else {
            return Err(format!(
                "unknown plan id `{plan_id}` (plans exist only for this server session and are single use); call plan_import again"
            ));
        };
        let assets = import::resolve_assets(&self.catalog, &ids, MAX_IMPORT_IDS)?;
        {
            let plan = &self.plans[pos].plan;
            if norm_path(&plan.project.root.to_string_lossy())
                != norm_path(&proj.root.to_string_lossy())
            {
                return Err(
                    "the plan was made for a different project; call plan_import for this project"
                        .into(),
                );
            }
            if plan.assets != import::canonical_ids(&self.catalog, &assets) {
                return Err("`asset_ids` differ from the plan; pass exactly the ids that were planned, or make a new plan".into());
            }
        }
        // Single use, whatever happens next.
        let Plan { plan, .. } = self.plans.remove(pos);
        let outcomes = import::apply_plan(&self.catalog, &plan, overwrite);
        let mut rows: Vec<Value> = Vec::new();
        let mut any_failed = false;
        let mut log_notes: Vec<String> = Vec::new();
        for o in &outcomes {
            match &o.result {
                Ok(rep) => {
                    if !rep.written.is_empty() {
                        match &self.cfg.install_log {
                            Some(path) => {
                                if let Err(e) =
                                    import::log_install(path, &self.catalog, &o.pack_id, &proj)
                                {
                                    log_notes.push(format!("install log not written: {e}"));
                                }
                            }
                            None => log_notes.push("install log disabled".to_string()),
                        }
                    }
                    let rel = |p: &Path| import::relative_to(&proj, p);
                    rows.push(json!({
                        "pack_id": o.pack_id,
                        "pack": o.pack_name,
                        "ok": true,
                        "written": rep.written.len(),
                        "written_files": rep.written.iter().take(100).map(|p| rel(p)).collect::<Vec<_>>(),
                        "written_paths": rep.written.iter().take(MAX_PATHS).map(|p| fwd(p)).collect::<Vec<_>>(),
                        "backup_paths": rep.backed_up.iter().take(MAX_PATHS).map(|p| fwd(p)).collect::<Vec<_>>(),
                        "skipped_existing": rep.skipped.iter().take(100).map(|p| rel(p)).collect::<Vec<_>>(),
                        "backed_up": rep.backed_up.len(),
                        "backup_dir": rep.backed_up.first().map(|p| rel(p)),
                        "warnings": rep.warnings,
                        "external_missing": rep.external_missing,
                    }));
                }
                Err(e) => {
                    any_failed = true;
                    rows.push(
                        json!({"pack_id": o.pack_id, "pack": o.pack_name, "ok": false, "error": e}),
                    );
                }
            }
        }
        let v = json!({
            "ok": !any_failed,
            "project": proj.name,
            "overwrite": overwrite,
            "packs": rows,
            "notes": log_notes,
            "next": "Open the project in the editor (or let it hot-reload); new assets appear under the Content Browser folders listed in written_files.",
        });
        if any_failed {
            // Report details but flag the failure.
            return Err(v.to_string());
        }
        Ok(v)
    }
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
    fn argument_helpers() {
        let a = json!({"s": "x", "n": 5, "b": true, "l": ["a", " b "], "c": "x, y", "bad": 3});
        assert_eq!(req_str(&a, "s").unwrap(), "x");
        assert!(req_str(&a, "missing").is_err());
        assert!(opt_str(&a, "bad").is_err());
        assert_eq!(opt_usize(&a, "n").unwrap(), Some(5));
        assert!(opt_usize(&a, "s").is_err());
        assert_eq!(opt_bool(&a, "b").unwrap(), Some(true));
        assert!(opt_bool(&a, "n").is_err());
        assert_eq!(str_list(&a, "l").unwrap(), ["a", "b"]);
        assert_eq!(str_list(&a, "c").unwrap(), ["x", "y"]);
        assert!(str_list(&a, "n").is_err());
        assert!(str_list(&a, "none").unwrap().is_empty());
    }

    #[test]
    fn every_tool_has_a_schema_and_description() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        for n in [
            "search_assets",
            "get_asset",
            "list_packs",
            "get_pack",
            "list_tags",
            "get_thumbnail",
            "plan_import",
            "import_assets",
            "refresh_catalog",
            "prepare_import",
        ] {
            assert!(names.contains(&n), "{n}");
        }
        for d in &defs {
            assert!(d["description"].as_str().unwrap().len() > 40);
            assert_eq!(d["inputSchema"]["type"], "object");
        }
    }
}
