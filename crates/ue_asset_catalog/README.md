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
  `read_only`, packs, assets, `catalog_generation`, `catalog_loaded_at`, `bundles_pending`),
  `GET /bundles/<id>` (see "Prepared bundles" below). Every response is `Connection: close`; read deadline 10 s, write
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

### Prepared bundles (`prepare_import`)

With `--bundle-dir <dir>` the hosted server also offers `prepare_import`, the one tool that stays
registered under `--read-only`. It builds a verified zip on the server; the client downloads it and
applies it to its own project. The server never touches a project and never writes into the library.

```
ue_asset_mcp ... --http 192.168.12.201:8433 --token-file ... --bundle-dir C:\HF\ue_asset_mcp\bundles
```

* Arguments: `asset_ids` (1 to 500 catalog ids), `project_engine_version` (e.g. `5.4`), and
  `include_deps` (default true: the in-pack dependency closure is added). There is no plan id and no
  server-side plan state. The release of each pack follows the catalog's no-downgrade rule.
* The zip holds `manifest.json` plus the selected packages, their companions (`.uexp`, `.ubulk`, ...)
  and the in-pack dependencies as byte-exact copies of the source entries under `Content/<path>`
  (`.uexp` and `.ubulk` stored, everything else deflated). `manifest.json` lists every file with
  `path`, `size` and `sha256` (over the uncompressed bytes), plus `bundle_id`, `catalog_generation`,
  `project_engine_version`, `engine_verdict`, `omitted_deps`, `external_missing`, `warnings` and
  `total_uncompressed_bytes`. Entry names are validated like an import (no `..`, drive letters,
  reserved names or case clashes); two packs that put different bytes at one path are a `CONFLICT`
  and identical bytes are written once.
* Result: `bundle_id`, `size_bytes`, `uncompressed_bytes`, `bundle_sha256`, `files` (first 200) and
  `files_total`, `verdict`, `packs`, `omitted_deps` and `external_missing` (each up to 200, with a
  `_total`), `warnings`, `expires_at` and `download` (`/bundles/<id>`).
* Errors (`isError`, text `error: <CODE>: ...`): `NOT_FOUND` (an unknown asset id), `CONFLICT`,
  `BUSY` (the build slot or the cache budget is taken; retry shortly), `CAP_EXCEEDED` (the bundle or
  the hourly budget is over its cap; the text names the size and the cap), `NOT_AVAILABLE` (the server
  has no `--bundle-dir`).
* `GET /bundles/<id>` needs the same bearer token and answers 200 with `Content-Length`,
  `X-Bundle-Sha256`, `X-Bundle-Id` and `Content-Type: application/zip`. It can be repeated until the
  bundle expires, 15 minutes after it was published; then 410 `BUNDLE_EXPIRED`. An id that never
  existed (or is not 32 hex digits) is 404. A bundle is published by an atomic rename, so a
  download never sees a partial file.
* The build and the download never hold the catalog lock: a search answers while a bundle builds or
  streams. One build runs at a time.
* `import_assets` and the install log are not involved: nothing is recorded server-side.
* Flags (all need `--http`): `--bundle-dir <dir>` (server-local; refused when it is inside, or
  contains, a library folder), `--bundle-ttl-secs` (900), `--max-bundle-bytes` (256 MiB),
  `--max-cache-bytes` (2 GiB on disk), `--max-builds` (1) and `--max-hour-bytes` (1 GiB of bundles
  per rolling hour; alias `--max-session-bytes`). Expired bundles, interrupted builds and stale
  temporary files in the folder are swept at startup and every minute; only files named
  `<32 hex>.zip` and `.tmp-<32 hex>` are ever removed.

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
