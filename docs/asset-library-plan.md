# Asset library plan: from packs to a searchable, agent-usable asset catalog

Status as of 2026-11-19. Detailed design and API contracts: `selective-import-design.md`. Existing
local library / sanitizer: `local-library-architecture.md`. Idea source: Hoard
(`E:/gitprojects/Hoard`, MIT-0; compared and rejected as a drop-in, see section 8).

## 1. Goal

Stop thinking in archives. Every asset inside every **content pack** and **project pack** (plugins
excluded) is individually known, tagged and searchable. A person (GTK app) or an agent (MCP server)
finds what they need, picks individual assets (3 grasses out of 20), and clicks / calls **Add to project**.
Assets that do not exist can optionally be generated through the existing mesh and sound pipeline.

## 2. Non-goals

* Plugins in the catalog. Rewriting `.uasset` bytes. Resizing or altering pre-authored pack assets.
* Replacing the sanitizer, the library layout or the Fab/Epic flows (Hoard rejected).
* New editor capabilities or a new import path through the automation pipeline (dropped by Scott).
* Version-control logic inside the import (see section 6).

## 3. Architecture

```
 packs on share  ->  ue_asset_sanitizer (scott-server)  ->  library/<slug>.<sha12>.zip + .zip.json (sidecar)
                                 |                                   + <zip>.assets.json   (asset index, derived)
                                 |                                   + <zip>.vision.json   (vision tags, derived, optional)
                                 v
        crates/ue_asset_library   header reader (uasset.rs), index, closure, subset import   [DONE]
                                 v
        crates/ue_asset_catalog   catalog merge + auto-tags + in-memory search + pack families [IN PROGRESS]
             |        |    \__ sources: packs (indexes) | generated (pipeline job rows, read-only)
             |        \_ bin ue_asset_mcp     stdio MCP for agents, runs per PC            [IN PROGRESS]
             \_ bin ue_asset_enrich           vision tagging via Ollama on scott-zap       [QUEUED]
        GTK app: per-pack asset picker [DONE, unverified in GUI]   global Assets page [NOT STARTED]
```

Derived data is never authoritative: indexes and vision overlays can always be rebuilt from the zips;
the sidecar stays immutable.

## 4. Work breakdown

| # | Item | Owner | Status |
|---|------|-------|--------|
| 1 | `.uasset` header reader (names, imports, soft refs, class, thumbnail); asset index overlay; dependency closure; subset import for Content and Project packs; sanitizer writes the index | crate worker | Done, tests green; real library run: 12,672 packages, 10,226 thumbnails |
| 2 | Per-pack picker in the app (search, class filter, thumbnails, required rows, footer), Content and Project packs | UI worker | Done, compiles; GUI not exercised |
| 3 | Catalog lib: merge indexes, auto-tags, search, pack families + release choice (no-downgrade) | catalog worker | In progress |
| 4 | MCP server `ue_asset_mcp`: search/get/list/tags/thumbnail, `plan_import`, `import_assets` (plan-gated), refresh | catalog worker | In progress |
| 5 | Generated source: finished mesh/audio pipeline jobs as read-only catalog entries (`source:generated`) | catalog worker | Queued |
| 6 | `request_generation` tool: audio by prompt, mesh from image; opt-in, explicit target project, session cap | catalog worker | Queued |
| 7 | `ue_asset_enrich`: thumbnail -> Ollama vision (same call shape as `estimate_smart_size`) -> `.vision.json`, merged into tags | catalog worker | Queued |
| 8 | Global **Assets** page in the app on top of the catalog lib: search, filters, thumbnails, multi-select across packs, "Add to project" per pack | UI worker | Not started (needs 3) |
| 9 | Diagnose 505 `header_unreadable` packages in Post_Apocalyptic_NYC_Environment_Megapack (likely cooked/unversioned) | crate worker | Not started |
| 10 | Verification: scratch-project import end to end, open in the editor; real-library catalog + MCP smoke test; MCP wired into Claude Code via `.mcp.json` | primary | After 3-8 |
| 11 | Deploy: sanitizer exe update on scott-server (writes indexes), schedule enrich as a low-priority batch | primary | After 10 |

Order: 3+4 -> 5,6,7 -> 8 -> 9 (anytime) -> 10 -> 11. Test once per body of work, not between edits.

## 5. Key decisions

* **Unit of pick = a package** (`.uasset`/`.umap` plus `.uexp/.ubulk/.uptnl`); dependencies in the same
  pack are auto-included and shown as "required by"; references outside the pack are reported, never
  guessed. Whole-zip hardening and sidecar checks still run before anything is extracted.
* **Index is a derived overlay**, one writer per file; stale detection by sidecar hash + entry count.
* **Tags are computed at catalog load** (class/kind, name tokens minus UE prefixes, texture-suffix maps,
  folder and pack words, engine version, synonyms) so tagger improvements need no re-index; vision tags
  are an optional overlay.
* **Search is in memory** behind a `Catalog` type (hundreds of packs x thousands of assets); SQLite/FTS can
  replace it later.
* **Pack families**: the same pack at several engine versions lists each asset once; the release is
  chosen per project (exact, else highest not above the project, else lowest with a warning).
* **MCP runs per PC over stdio** (import writes into the local project). Import tools are plan-gated:
  `import_assets` only runs a plan made by `plan_import` in the same session, only into projects
  registered at startup, default skip-existing, overwrite = backup.
* **Generation is opt-in**: `--allow-generation`, portal token read at runtime and never logged, explicit
  target project, per-session cap; tool text tells the agent to search first. Mesh generation is
  image-to-3D only. Generated entries are discoverable, not importable, in the first cut.
* **Vision tagging is serial and opt-in** (scott-zap runs one request at a time and is shared); it uses
  the asset's own editor thumbnail, so no snapshots are needed when one exists.

## 6. Version control stance (Lore, behind `vcs`)

The import is VCS-agnostic. It writes files into `<Project>/Content/` and returns the exact written
paths (and backups). Nothing in the app, crates or MCP tools names a specific VCS. Any staging or
committing is a separate step done by the user or by an agent through the provider-neutral `vcs_*`
tools (the project uses Lore). `import_assets` therefore returns `written_paths` shaped for a follow-up
`vcs_stage`. Docs that mentioned Perforce in this repo were corrected on 2026-11-19; the earlier
Perforce wording came from an old verification note in `local-library-architecture.md` and from the
automation pipeline's own stage names (`p4_reconcile`), which this feature does not use.

## 7. How "Add to project" lands in Unreal

Files are copied into the project's `Content/` on disk; the editor is not involved. Editor closed: assets
appear at next launch. Editor open: whether the directory watcher picks them up is unverified (item 10
checks it; fallback is restart or a Content Browser folder fix-up). Pack-version older or equal to the
project: fine (Unreal upgrades on resave); newer: flagged, not the default. Missing external
dependencies are listed in the result.

## 8. Why not Hoard as-is (summary)

Hoard converts Unreal content out to FBX/PNG/WAV and replaces silently on import: no layout planning,
no engine-version rule, no conflict preview or backups, no inbox state machine, deletes original zips,
no Epic/Fab account. Lifted instead: the header-reader approach, the thumbnail-from-uasset idea, the MCP
tool vocabulary, tag normalisation ideas. Possible later: its archive-set naming logic, SigLIP image
embeddings for "more like this".

## 9. Risks and open items

* Large packs index slowly over SMB (106 s for the biggest); indexing is on worker threads, results
  cached; the sanitizer pre-builds indexes so most users never wait.
* 505 unreadable headers in one pack (item 9). Their dependencies are unknown, so the picker warns.
* The picker and Assets page have not been exercised in a display session; closure/refresh behaviour on
  tens of thousands of rows is unmeasured in the GUI.
* Project packs removed the "never Project" filter in the Fab/Epic dialogs: a name-matching local project
  pack now shows as a `[Local]` release there (selection required). Revert if unwanted.
* Portal and Ollama are LAN services: catalog and MCP must degrade gracefully (cached last result,
  short timeouts).
* Index schema or tagger changes must keep old overlays readable (schema field, unknown fields tolerated).
* Nothing here is committed; `data/schemas/` and `.claude/` stay uncommitted per repo notes.

## 10. Verification plan

Unit/integration tests per crate (done for the library; catalog and MCP pending); a real-library
smoke run (index, catalog load, searches "grass" / "rock" / "door"); scratch-project subset import
opened in the editor; MCP exercised from a Claude Code session via `.mcp.json`; GUI click-through of the
picker and Assets page; then sanitizer redeploy to scott-server.
