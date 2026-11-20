# Selective import: pick individual assets out of a pack

Goal: a pack ships 20 grass types, the user wants 3. Browse the pack's assets (with the editor
thumbnails UE saved inside the `.uasset`), tick the ones wanted, and import only those plus the files
they need. Idea lifted from Hoard (`E:/gitprojects/Hoard/crates/hoard-unreal/src/package.rs`, MIT-0): a real
header parser (names, imports, thumbnail) instead of the 256 KB string scan, and a link graph between
assets. We keep our own install path (byte-exact `.uasset`, layout rewrite, backups, engine rules).

## Decisions

* **Index is a derived overlay**, `<slug>.<sha12>.zip.assets.json`, next to the sidecar. The sidecar stays
  immutable (commit marker, one writer). The index can always be regenerated from the zip, so it is
  never authoritative. Writer: the sanitizer after the sidecar commit (best effort, a failure never
  fails the job) and the app on demand when missing/stale (temp file + rename; if the share is read-only
  the app keeps it in memory only).
* **Staleness**: index records `sidecar_sha256` and `entry_count`; a mismatch means regenerate.
* **Unit of pick** = a *package* (`.uasset`/`.umap` plus companions `.uexp`, `.ubulk`, `.uptnl` with the
  same stem in the same folder). Loose non-package files (ini, json, .ufont, media) are listed under
  "Other files" and are not auto-selected.
* **Dependency closure**: for each package, deps = imports (non `/Script/`, non `/Engine/`) + soft
  package references (summary table, if the version has it) + any name-table entry that looks like a
  package path (`/Game/...` or `/<PluginName>/...`; soft object paths live in the name table). Deps are
  mapped to zip entries via the sidecar rewrite rule (`game_prefix` -> `zip_prefix`, i.e. output
  `Content/<ContentFolder>/...`) and kept only when they resolve to an entry in this pack; the rest are
  recorded per package as `external` (not in this pack).
* **Closure is automatic but visible**: selecting an asset auto-checks its transitive deps, shown as
  "required by X" (greyed, cannot be unchecked while a parent is checked). The user can untick the
  parent. Selecting nothing is not allowed. Cycles are fine (visited set).
* **Safety is unchanged**: whole-zip `validate` (hardening, counts vs sidecar) still runs; only the
  extraction/apply set is filtered. Never `ZipArchive::extract`. Conflicts/backups/policy behave as
  for a full import, over the subset. Empty selection is an error.
* **Header reads are bounded**: read only `TotalHeaderSize` bytes (cap 16 MiB; larger -> package listed
  without deps/thumbnail and flagged `header_unreadable`, and treated as "needs everything it might
  reference": the UI warns). Cooked packages (no editor data) parse names/imports but have no thumbnail.
* **Thumbnail**: PNG/JPEG bytes from the thumbnail table, stored once in a sibling cache, not in the
  JSON: `<zip>.thumbs/<index>.png` is NOT written to the share. The app extracts a thumbnail lazily from
  the zip when a row becomes visible (function `read_thumbnail(entry, package)`), caching under the app's
  user cache dir keyed by `sha12/package`.

## API contract (crate `ue_asset_library`, new modules `uasset`, `asset_index`; `import.rs` extended)

```rust
// asset_index.rs
pub const INDEX_SCHEMA: u32 = 1;
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AssetIndex {
    pub schema: u32,
    pub sidecar_sha256: String,
    pub entry_count: u64,
    pub packages: Vec<PackageInfo>,      // sorted by game_path
    pub other_files: Vec<OtherFile>,     // non-package files, zip-relative path + size
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PackageInfo {
    pub game_path: String,               // "/Game/Grass/Meshes/SM_Grass_03" or "/PluginName/..."
    pub zip_path: String,                // main file entry, e.g. "Content/Grass/Meshes/SM_Grass_03.uasset"
    pub companions: Vec<String>,         // zip paths of .uexp/.ubulk/.uptnl
    pub class: Option<String>,           // main export class short name: "StaticMesh", "Material", ...
    pub size: u64,                       // uncompressed bytes of zip_path + companions
    pub deps: Vec<String>,               // game_paths of packages in this pack it needs
    pub external: Vec<String>,           // referenced packages NOT in this pack
    pub has_thumbnail: bool,
    pub header_unreadable: bool,
}
pub fn build_index(entry: &LibraryEntry) -> Result<AssetIndex, ImportError>;
pub fn load_or_build_index(entry: &LibraryEntry) -> Result<AssetIndex, ImportError>; // reads overlay, validates, else builds (+ best-effort write)
pub fn index_path(entry: &LibraryEntry) -> PathBuf;
/// Transitive closure over deps. `selected` = game_paths. Returns (all packages needed incl. selected, sorted).
pub fn closure(index: &AssetIndex, selected: &[String]) -> Vec<String>;
pub fn read_thumbnail(entry: &LibraryEntry, game_path: &str) -> Result<Option<Vec<u8>>, ImportError>; // PNG/JPEG bytes

// import.rs (existing fns keep their behaviour and signature)
pub struct Selection { pub game_paths: Vec<String>, pub include_deps: bool }   // include_deps = apply closure()
pub fn preview_import_subset(entry, project_root, index: &AssetIndex, sel: &Selection) -> Result<Vec<PathBuf>, ImportError>;
pub fn safe_import_subset(entry, project_root, policy, index: &AssetIndex, sel: &Selection) -> Result<ImportReport, ImportError>;
// ImportReport gains: pub external_missing: Vec<String>  (external deps of the imported packages)
```

`ImportError` gains `EmptySelection` and `UnknownPackage(String)`.

## UI (app)

Local entries only (Fab/Epic downloads are not indexable). A **"Choose assets..."** button on the import
dialog (`logged_in/local_library/import_dialog.rs`) and on the shared download dialog
(`logged_in/download_dialog.rs`) when the selected release is Local and kind is Content/Plugin. Opens an
`AdwDialog` picker: search box, class filter (StaticMesh, Material, Texture2D, ...), folder-grouped
list with lazy thumbnails, check rows, a footer "N selected + M required, X MB", and an "Import
selection" action that feeds the existing project/overwrite flow with a `Selection`. "Import everything"
stays the default. The index is built on a worker thread (progress spinner), results via `async_channel`.

## Out of scope (first cut)

Cooked-only packs' thumbnails, family auto-grouping by name stem, picking inside `.umap` sub-levels,
rewriting any `.uasset` bytes, Fab-sourced entries, changing the sanitizer's sidecar schema.

---

# Phase 2: cross-pack asset catalog, auto-tags, MCP server for agents

Requirement (Scott): no need to open an archive to see its contents. One derived, auto-tagged, searchable
library of every asset in every pack, and an MCP server (like Hoard's `POST /mcp` tool set, see
`E:/gitprojects/Hoard/README.md` ~L1108-1152 and `crates/hoardd/src/tagger/`) so agents can find an
existing asset and pull it into a project themselves.

## Decisions

* **New workspace crate `crates/ue_asset_catalog`** (lib + bin `ue_asset_mcp`), depends on
  `ue_asset_library` only. No GTK. The app can later reuse the lib for a global "Assets" page.
* **Catalog is derived, never authoritative.** Source = per-zip `.assets.json` overlays (phase 1) +
  sidecars. Missing/stale overlay -> `load_or_build_index` (read-only share: in-memory only). A local cache
  file (`<user cache dir>/ue_asset_catalog/catalog.json`, keyed by zip sha12 + index mtime) makes startup
  fast over SMB; corrupt cache is ignored.
* **Search is in-memory** (hundreds of packs x thousands of assets is small): tokenised inverted index,
  weighted fields (name > tags > folder > pack name), AND of query terms with prefix match on the last
  term, filters (class, tag, pack, kind, engine version <=, has_thumbnail). Isolated behind a `Catalog`
  type so SQLite/FTS can replace it later. No new heavy deps.
* **Auto-tags (offline, deterministic, in `tags.rs`)**, computed at catalog load from `PackageInfo` +
  `Sidecar`, not stored in the index (so improving the tagger needs no re-index): class tag
  (`class:staticmesh`, plus friendly kind `mesh|material|texture|blueprint|niagara|animation|sound|
  level|other`), name tokens (split on `_`, camelCase, digits; drop UE prefixes `SM_ SK_ T_ M_ MI_ MF_ BP_
  NS_ A_ ...` and texture suffixes `_BC _D _N _R _ORM _AO _H ...` but map suffixes to tags like
  `map:normal`), folder words, pack display name tokens, `pack:<slug>`, `kind:content|plugin`,
  `engine:5.3`, `lod`/`variant:03` hints, simple synonym table (grass~foliage~plant, rock~stone, ...).
  Keep the table small and data-driven (a static array). Embeddings / image tags / LLM tagging are out of
  scope for now (hook point only).
* **Duplicate packs/releases**: entries with the same `name_key` (see `name::clean_name`) are one *pack
  family*; an asset is listed once per family with `releases: [engine versions]`. When importing for a
  project, pick the release with the existing no-downgrade rule (exact, else highest not above the
  project, else lowest + warning). If that rule lives only in the GTK app (`src/models/...choose_release`),
  port the pure function into `ue_asset_library` (leave the app's copy; do not edit `src/`).
* **MCP server = stdio, runs locally per PC** (`ue_asset_mcp --library <dir> [--library ...]
  [--project <uproject-or-root> ...]`), because import must write into the project on the agent's own
  machine and each PC mounts the share. JSON-RPC 2.0 hand-rolled (initialize, tools/list, tools/call,
  ping; protocol version negotiation; newline-delimited stdio), no async runtime needed. HTTP mode is a
  later option (not now).
* **Tools** (names stable, descriptions written for an LLM, results compact JSON text; thumbnails as MCP
  `image` content):
  `search_assets{query, kind?, class?, tags?, pack?, engine_version?, limit?, offset?}`,
  `get_asset{id}` (details, deps in-pack, external deps, pack, releases, tags),
  `list_packs{query?, limit?}`, `get_pack{id}`, `list_tags{prefix?, limit?}` (facet counts),
  `get_thumbnail{id}`,
  `plan_import{project, asset_ids[], include_deps=true}` (dry run: files, conflicts, engine-version
  verdict, external deps, size; this is the default path),
  `import_assets{project, asset_ids[], overwrite=false, confirm_plan_id}` which only runs a plan created by
  `plan_import` in the same session (prevents blind writes).
  Asset id = `<pack sha12>:<game_path>`, stable across restarts.
* **Import guardrails**: `project` must be a registered project root passed via `--project` (or listed in a
  config file `--config`), contain a `.uproject`; policy defaults to SkipExisting, `overwrite=true` uses
  OverwriteWithBackup; never touches anything outside the project; multi-pack selections are grouped per
  pack and applied through `safe_import_subset` per pack; each applied import is appended to the app's
  install log format (`<user data dir>/epic_asset_manager/install_log.json`) if the existing writer is
  reachable from the crate, else skipped with a note. Read-only tools never write anything.
* **Refresh**: `refresh_catalog` tool plus automatic rescan when a library dir's mtime/entry list changed
  (poll at most every 30 s on tool calls, not a background thread).

## Scope amendment (Scott)

* Catalog, index and subset import cover **Content packs and Project packs** (zip with a `.uproject`;
  only its `Content/` tree is indexed and pickable, written into an existing project's `Content/` with
  the same relative paths). **Plugins are out**: never indexed, never in search results.
* Scott's existing **mesh and sound generation pipeline** (NocoBase `plugin-automation-pipeline`, Trellis/
  ComfyUI meshes, audio jobs) is to be reused: planned as a second catalog source (`Source::Generated`,
  ids `gen:<jobId>`) fed from finished job rows, plus a `request_generation` fallback tool, and possibly
  the Ollama box for tagging. Pending a scout report on the pipeline's API, auth and metadata.

## Phase 3 (decided by Scott)

* **Both** a read-only `Source::Generated` (finished rows of `automation_pipeline_queue_mesh/_audio`,
  public list reads, tag `source:generated`, pending audio-review jobs are not entries) **and** a
  `request_generation` tool (audio by prompt; mesh from an image path via `meshJobs:uploadImage` +
  `submitBatch`; portal token read at runtime only, explicit `targetProject` required, opt-in flag).
* **Importing = "add to project"**: the byte-exact copy of `safe_import_subset`, nothing else. Pack
  assets are pre-authored and are never resized, simplified or given new collision. The earlier idea of
  routing imports through the automation pipeline (new editor capabilities) was **dropped by Scott**. The
  import is VCS-agnostic: it writes files and reports the exact paths written, and any version control
  step is done by the user or an agent through the provider-neutral `vcs_*` tools (the project uses Lore).
* **Vision tagging** reuses the mesh pipeline's `estimate_smart_size` pattern (Ollama vision model on
  `scott-zap`): the image is the asset's own editor thumbnail from the `.uasset` (no snapshot needed when
  present). Opt-in separate bin (`ue_asset_enrich`), results cached in a derived overlay
  `<zip>.vision.json` (keyed by game_path; model + prompt version recorded), merged into tags at catalog
  load. Never run from the MCP server startup path.
