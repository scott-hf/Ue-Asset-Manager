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

## Hosted, read-only HTTP mode

```
ue_asset_mcp --library F:\ue_assets_library\library --http 192.168.12.201:8433 \
  --token-file C:\HF\ue_asset_mcp\token.txt --cache C:\HF\ue_asset_mcp\catalog.json \
  --no-generated --no-install-log
```

One shared service serves the read tools to LAN clients (pattern: DOC-0192, code refs DOC-0232;
hand-rolled `std::net` listener, still no HTTP crate or async runtime).

* `--http <ip:port>` replaces stdio. It must be a concrete interface address: `0.0.0.0` and `::` are
  refused. `--http` implies `--read-only`.
* `--token-file <file>` is required; a missing, unreadable or empty file exits with code 2. The token
  (trimmed) is never logged, echoed or put in an error. Every route except `GET /health` needs
  `Authorization: Bearer <token>` (constant-time compare, else 401).
* `--allowed-host <host:port>` (repeatable) adds accepted `Host` values (default: the `--http`
  address); a wrong `Host` or any `Origin` header is 403.
* `--read-only` (also usable on stdio) hides and refuses `import_assets`, `plan_import`,
  `list_projects`, `refresh_catalog` and, unless `--allow-generation`, `request_generation` and
  `generation_status`: `isError` text `error: READ_ONLY: ...`. It cannot be combined with
  `--write-overlays`; the server never writes into the library. Only the `--cache` file and the
  thumbnail cache are written.
* Routes: `POST /mcp` (one JSON-RPC message per request: 200 with the reply, 202 for a
  notification, 400 for a batch or broken JSON or an unknown `MCP-Protocol-Version`, 411 without
  `Content-Length`, 413 over 1 MiB, 405 on other methods), `GET /health` (open; status, version,
  `read_only`, packs, assets, `catalog_generation`, `catalog_loaded_at`), `GET /bundles/<id>` (404
  until the bundle store exists). Every response is `Connection: close`; read deadline 10 s, write
  timeout 60 s, header block <= 16 KiB; worker pools 8 (`/mcp`, `/health`) and 2 (`/bundles`), a full
  pool answers 503 `BUSY`.
* The catalog loads on a background thread after the port is bound: `/health` reports
  `"status":"loading"` and tool calls answer `BUSY` until the first load ends (about 55 s without a
  cache for the real library, under a second with `--cache`). The thread then rescans every 30 s and
  swaps in a changed library; an empty load never replaces a good catalog. The catalog lock is held
  only while a tool runs.
* Thumbnails: `get_thumbnail` keeps its MCP result (a text block plus an `image` block with base64
  data and `mimeType`); a client that cannot show image blocks saves the decoded bytes to a file.
* Access log (stderr): method, path, status and milliseconds only.
* Accepted residual risk: plain HTTP with one shared bearer token on a trusted LAN (no TLS). An
  attacker on the LAN could read the token. Reopen if an untrusted device joins, the service leaves
  the LAN, or per-client revocation is needed. Never bind a wildcard address; the token plus the
  interface binding are the only access control.

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
