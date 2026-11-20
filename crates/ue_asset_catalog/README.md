# ue_asset_catalog

Cross-pack asset catalog, offline auto-tagger and a stdio MCP server (`ue_asset_mcp`) over the local
Unreal asset library, so an agent (or the GTK app) can find an asset without opening any archive and
copy it into a project. Depends on `ue_asset_library` only; no GTK, no async runtime, no HTTP crate.

* The catalog is **derived**: the library zips, sidecars and `<zip>.assets.json` overlays stay the source of
  truth. Plugins are never listed or indexed; content and project packs are.
* Releases of one product (same cleaned name and kind) are one pack *family*; an asset is listed once with
  all its engine versions. Import picks the release by the no-downgrade rule (exact, else the highest not
  above the project, else the lowest with a warning).
* Tags are computed at load (`tags.rs`): class, type (`type:staticmesh`, `type:skeletalmesh`; the `kind` filter also accepts `mesh` for both), name words, folders, synonyms,
  `map:normal`, `lod`, `variant:03`, `engine:5.3`, `kind:content|project`, `pack:<slug>`, `source:generated`,
  plus optional vision tags from `<zip>.vision.json`.
* Asset id: `<pack sha12>:<game_path>` (stable; any release's pack id resolves) or `gen:<jobId>`.

## Run

```
ue_asset_mcp --library \\scott-server\ue_assets_library\library --project E:\Projects\MyGame\MyGame.uproject
ue_asset_mcp --library <dir> --selftest          # stats + sample searches ("grass", "rock", "door")
```

Flags: `--library <dir>` (repeat), `--project <.uproject|root>` (repeat; the only places imports may
write), `--config <json {libraries, projects}>`, `--cache <file>` / `--no-cache`, `--no-build` (overlays and
cache only), `--write-overlays` (also write built indexes next to the zips; default off, the server never
writes into the library), `--install-log <file>` / `--no-install-log`, `--portal-url <url>` (generated
assets, default `http://192.168.12.201:13000`) / `--no-generated`, `--allow-generation`,
`--portal-token-file <file>`, `--max-generations <n>` (default 5), `--selftest`.

Startup is slow only the first time (indexes are built from the zips and cached in
`%LOCALAPPDATA%\ue_asset_catalog\catalog.json`); the sanitizer writes the overlays for new packs.

## Claude Code (`.mcp.json`)

```json
{
  "mcpServers": {
    "ue-assets": {
      "command": "E:/gitprojects/Ue-Asset-Manager/target/release/ue_asset_mcp.exe",
      "args": [
        "--library", "//scott-server/ue_assets_library/library",
        "--project", "E:/Projects/MyGame/MyGame.uproject"
      ]
    }
  }
}
```

Build it with `cargo build --release -p ue_asset_catalog` (Windows: MSYS2 UCRT64 on `PATH`). Add
`"--allow-generation"` to the args only if the agent may submit generation jobs.

## Tools

`search_assets` (start here), `get_asset`, `list_packs`, `get_pack`, `list_tags`, `get_thumbnail`
(MCP image content), `list_projects`, `plan_import` (dry run, returns a plan id), `import_assets`
(refuses without a valid single-use `confirm_plan_id`, project/ids must match the plan; skip existing unless
`overwrite=true`, then backed up under `Saved/EAM_Backups`), `refresh_catalog`, `request_generation`
(opt-in, capped, `target_project` required; audio by prompt, mesh from an image path only),
`generation_status`. `search_assets` takes `exclude_packs` (pack slugs); the server flag
`--exclude-pack <slug>` (repeatable) hides packs from every search. Imports are appended to the app's `install_log.json`.

Generated assets (finished rows of `automation_pipeline_queue_mesh/_audio`, read from the portal's public
list endpoints, cached when the portal is down) are searchable and read-only: they report where they live
and cannot be imported here.

## Vision tags (opt-in)

```
ue_asset_enrich --library <dir> [--pack <id|name>] [--limit 3] [--dry-run]
```

Describes each asset's own editor thumbnail with Ollama (`qwen3-vl:8b` on scott-zap, serial requests,
`think:false`, 120 s timeout, failures skipped) into `<zip>.vision.json`; resumable; merged into tags and
a searchable description when the catalog loads. Never run by the MCP server.

## Library API (for the GTK "Assets" page)

`Catalog::load` / `from_entries` / `search(&SearchQuery)` / `find_asset` / `choose_pack`,
`import::plan_import` (dry run) and `import::apply_plan` (wraps `safe_import_subset`),
`release::choose_release`. See the crate docs (`cargo doc -p ue_asset_catalog --open`).
