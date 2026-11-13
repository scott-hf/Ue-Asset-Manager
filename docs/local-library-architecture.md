# Local library, sanitizer and Windows build: architecture and work log

Scope: everything added on top of upstream Epic Asset Manager 3.9.4 in the `scott-hf` fork
(work item WI-1744 plus follow-ups). Upstream docs: `AGENTS.md`, `docs/api-map.md`.

## 1. Goals

1. Build and run the GTK4/libadwaita app on **Windows** (upstream is Linux/Flatpak only).
2. Use assets bought **outside Fab/Epic** (arbitrary zips of unknown layout) as if they were library
   assets: find them on a share, install them into a project, optionally enrich them from Fab.
3. Make **Download** explicit: choose the project and engine version instead of silently downloading
   the first release.
4. Know which **engine version a project uses**, including custom engine forks.

## 2. Components

```
                 \\scott-server\ue_assets_library              (SMB share, F:\ue_assets_library on host)
 you drop zips -> import\            inbox (base folder; unprocessed\ also accepted)
                  import\processing\ claimed, being sanitized
                  import\processed\  originals after success (+ .duplicate.txt for re-drops)
                  import\needs-review\ ambiguous layout (+ <name>.review.json with diagnostic codes)
                  import\failed\     errors (+ .error.txt)
                  library\           sanitized <slug>.<sha12>.zip + <slug>.<sha12>.zip.json (sidecar)
                                     + optional <zip>.fab.json overlay (app-owned)
       ^                               |
       | ue_asset_sanitizer --watch    | scan_library()
       | (scott-server, scheduled task)v
  crates/ue_asset_library  <------  epic_asset_manager (GTK app): Local Library page, import, Fab match
        (single owner of the sidecar format, layout planning, safe import)
```

### 2.1 `crates/ue_asset_library` (workspace crate, no GTK)

Single owner of the cross-process contract; the app and the `ue_asset_sanitizer` binary use the same
functions as the tests. Dependencies: zip, serde(+json), regex, sha2.

| Module | Responsibility |
| --- | --- |
| `sidecar` | Versioned manifest `Sidecar` (schema 1): kind, display name, sha256 of source zip, engine version guess, content folder / plugin name, rewrite rule (`zip_prefix`->`game_prefix`), `allowed_roots`, entry count, uncompressed size, `external_refs`, plugin metadata. Unknown fields tolerated. |
| `name` | `clean_name`: strips trailing engine version (`Camping Animations 5.5` -> `Camping Animations`, 5.5; only real UE versions 4.0-4.27 / 5.x) and a trailing "Plugin". |
| `layout` | `plan_layout`: decides how the zip must be rearranged so import = extract over the project root. |
| `harden` | Zip-slip and path hygiene checks (`..`, absolute, `:`/ADS, reserved Windows names, case-fold collisions, duplicates, symlinks, depth, entry/size caps). |
| `sanitize` | `sanitize_zip`: streams entries into `library/` with `raw_copy_file_rename` (no recompression); temp file -> rename, sidecar written last as the commit marker. |
| `pipeline` | Folder state machine: claim, stability polling, reconcile on start, dedupe by sha256, retries, `run_pass`. |
| `config` | `sanitizer.json` loader (multiple jobs, isolation between jobs). |
| `library` | `scan_library`: reads `*.zip.json` whose zip exists; drops orphans; never panics on bad JSON. |
| `import` | `preview_import` (conflict list) and `safe_import` / `safe_import_new_project`. |

#### Layout planning (the core idea)

* **`.uproject` in zip** -> kind `project`: copied through, never importable into an existing project;
  the app offers "Create new project".
* **`.uplugin` in zip** -> kind `plugin`: output entries `Plugins/<PluginName>/...`; `Binaries/`,
  `Intermediate/`, `Saved/`, `DerivedDataCache/` are dropped. Plugin assets use the `/PluginName/` mount
  (verified on the sample), so `/Game/` is not required. `EngineVersion`, `FriendlyName`,
  `MarketplaceURL` come from the `.uplugin`.
* **Otherwise content**: every `.uasset`/`.umap` header (first 256 KB) is scanned (ASCII and UTF-16LE) for
  `/Game/...` strings, cut at the first `.`, `'`, `"`, `,`. The candidate whose last component equals
  the file stem is the asset's *own* package path. The longest directory-suffix match between the
  own path and the entry's zip path gives a prefix rewrite `zip_prefix -> game_prefix`
  (`OperatingRoomPack -> OperatingRoom`; Camping Animations already matches). Every sampled asset
  must yield the same rule; otherwise the zip goes to `needs-review/` with a diagnostic code
  (`NO_GAME_MATCH`, `MULTI_ROOT`, `RULE_DISAGREE`, ...). We **never modify uasset bytes**, only zip entry
  paths. `/Game/` references outside the pack are recorded as `external_refs` (dependency warning).
* Output entries are `Content/<ContentFolder>/...` or `Plugins/<Name>/...`, so importing is
  "extract over `<ProjectRoot>/`".

#### Concurrency and crash safety

* Claim = exclusive create of `processing/<name>.claim` (host, pid, start) then rename; NotFound =
  already taken, sharing violation = busy (retry), truncated zip = retry (up to 3), else `failed/`.
  Size+mtime must be stable across two polls (SMB uploads); `.part/.tmp/.claim` ignored.
* Identity is the SHA-256 of the source zip; library files are named `<slug>.<sha256[..12]>.zip`.
  Same hash again = duplicate (archived, noted); same name different hash = distinct entry.
* Order: library zip temp -> rename, sidecar temp -> rename (**commit marker**), then move the original
  to `processed/`. Startup reconcile removes orphan temp files and quarantines sidecar-less zips.
  Temp files live in the destination folder (same filesystem).
* Retryable errors (truncated/unreadable/missing volume) are counted per item in
  `processing/<name>.attempts`; after `max_attempts` (5) and `retry_min_age_secs` (600) the set goes to
  `failed/` with `.review.json` (`RETRIES_EXCEEDED`). Busy/unstable never count.
* Partial extraction: rar/7z entries that fail are skipped when >= `min_extract_success_ratio` (0.95) of
  entries are fine; recorded in the sidecar (`skipped_entries`, `extract_success_ratio`) with a WARN log,
  else `ARCHIVE_CORRUPT` -> `failed/`.
* Lease/heartbeat claims: the claim file carries owner (host, pid, process start, run id), the volume
  list and a lease expiry (`lease_secs`, default 300) that a heartbeat thread renews every
  `heartbeat_secs` (default 60) during long jobs. Stale claims (lease + skew margin expired, or owner
  provably dead on the same host; a live same-host owner is never reclaimed) are reclaimed automatically
  at startup and on every watch pass: exclusive `create_new` lock, partial `.tmp-*` outputs removed, all
  volumes back to the inbox. After `max_reclaims` (default 3) the set goes to `failed/` with
  `STALE_RETRIES_EXCEEDED`. Legacy lease-less claims still use `stale_mins`/`--requeue`.

#### Safe import

Only entries under the sidecar's `allowed_roots`; re-validates hardening, checks counts against the
sidecar; never `ZipArchive::extract`. Extracts to `<project>/Saved/EAM_Import_Tmp/<ts>/`, previews
conflicts, then applies `SkipExisting` or `OverwriteWithBackup` (backups go to
`<project>/Saved/EAM_Backups/<ts>/`, not `.bak` beside assets). Plugin imports warn when the plugin has
`Source/` but the project does not.

### 2.2 Sanitizer binary and config

`ue_asset_sanitizer --config sanitizer.json [--once|--watch]` (also `--import/--library` for one ad-hoc
job; mixing with `--config` is an error). Config:

```json
{ "jobs": [ { "import": "F:/ue_assets_library/import", "library": "F:/ue_assets_library/library" } ],
  "poll_secs": 30, "stable_secs": 5, "stale_mins": 120 }
```

Any number of jobs; an unreachable/unwritable job is logged and retried, others keep running. Exit
codes: 0 ok, 1 usage, 2 needs-review/failed, 3 a job unavailable (`--once`). `--watch` polls (no inotify
on SMB). See `crates/ue_asset_library/README.md`.

### 2.3 App side (`epic_asset_manager`)

* **Settings**: `local-library-directories` (`as`) - register any number of library paths
  (Preferences, same add/remove rows as vault dirs, `DirectoryConfigType::LocalLibrary`).
* **Local Library page** (`logged_in/local_library/`, own sidebar button): scans all directories on a
  thread pool (results via `async_channel`; unreachable dirs reported, never blocking), search filter,
  kind badge, detail pane with engine guess and external-reference warnings. New lightweight GObject
  `LocalAssetData` (`models/local_library.rs`), deliberately not faked `AssetInfo`/`FabData`.
* **Import dialog**: pick a project, conflict preview, `safe_import` on a worker thread; "Create new
  project" for `project` kind. Overwrite switch maps to `OverwriteWithBackup`.
* **Offline use**: "Open the local library without logging in" on the login screen.
* **Match on Fab** (optional, logged-in only): searches the existing Fab marketplace search with the
  cleaned name (plugins prefer the `.uplugin` friendly name), you pick a candidate, the choice is
  stored in the **app-owned overlay** `<zip>.fab.json` (temp+rename). The sanitizer's sidecar is never
  modified after commit, so a re-sanitize cannot wipe a match. "Clear match" deletes only the overlay.
  Never automatic.
* **Download dialog** (`logged_in/download_dialog.rs`, `EpicDownloadDialog`): every Download entry point
  (library row/tile, detail pane, Add to Project, Fab) opens one shared dialog: project (default = project
  selected on the Projects page, else last choice, else vault-only), engine version (default = release
  matching the project's engine, else newest, labelled "latest"), Fab platform, overwrite switch, and a
  hint on version mismatch. "Don't add to a project (vault only)" keeps plain downloads possible. The
  download manager item shows `Title (UE x.y)`. Replaces the silent "first release" download, the old Fab
  version dialog and the old add-to-project dialog.
* **Registered projects and engine versions**: see section 4.

### 2.4 Windows port

Upstream code assumed Linux. Fixes: `gettextrs` import Linux-gated (`main.rs`), `std::os::unix`
`MetadataExt` replaced by `.len()` (`epic_file.rs`), Linux-only Docker engine calls and Secret Service
items cfg-gated (`docker_download.rs`, `models/mod.rs`). `src/config.rs` (normally generated by Meson) is
hand-written by the installer and stays gitignored. Build toolchain: MSYS2 UCRT64 (GTK4, libadwaita) +
Rust `stable-x86_64-pc-windows-gnu`.

### 2.5 Installer

`windows/install.ps1`: builds release (or `-SkipBuild`), compiles gresource and schemas, bundles the
UCRT64 DLL closure (found with `ldd`), gdk-pixbuf loaders, icon themes and the app schema into
`%LOCALAPPDATA%\Programs\EpicAssetManager`, and creates a Start Menu shortcut. Self-contained (verified
with MSYS2 off `PATH`). `-Uninstall` removes it. App id stays `...epic_asset_manager.Devel`.

### 2.6 Deployment of the sanitizer

`scott-server` has Windows OpenSSH off (port 22 refused); WSL sshd on `:2222` is reachable and can run
`powershell.exe` on the host. The exe and `sanitizer.json` live in `C:\HF\ue_asset_sanitizer\`; a per-user
logon task `HFUeAssetSanitizer` runs `--watch`. Deploy/update = copy the new exe to
`F:\ue_assets_library\_deploy\` and run `_deploy\install.ps1` through
`ssh scott-server '/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -ExecutionPolicy Bypass -File F:/ue_assets_library/_deploy/install.ps1'`
(forward slashes; backslashes are eaten by the shell).

## 3. Design decisions and why

* **Separate sanitizer binary, shared crate**: the server needs no GTK; one owner for the format and
  layout logic; tests call the same API as callers. A workspace member (not a second bin in the app package)
  so the sanitizer builds without GTK dev libraries; Meson/Flatpak still call plain `cargo build`.
* **Design gate (opus) + Codex review** failed the first outline; accepted: prefix-rewrite rule,
  all-asset sampling, single format owner, create-new claim files, sha256, stable-size polling, staged
  import with backups outside the project, hardened extraction, Fab overlay file. Rejected: Codex's
  "byte patching is unsafe" (we only rename zip entries), lease/heartbeat claims.
* **Base folder is the inbox** (the first design wrongly used only `unprocessed/`; corrected after a
  real run).
* **Overlay instead of editing the sidecar**: one writer per file.
* **Local library entries are their own type**: faking Epic structs would leak into download paths.
* **No in-place asset rewriting**: only archive paths change; ambiguity is quarantined, never guessed.

## 4. Registered projects and engine versions

Projects were only discovered by scanning `unreal-projects-directories`; a custom engine fork (GUID
`EngineAssociation`, e.g. PrjEfz `{B9B25D9F-...}`) gave no version, so the download dialog fell back to
the newest release. Added: register a single `.uproject`, a per-project engine-version override stored in
settings, shown and editable in the project detail pane. Resolution order everywhere: override >
numeric `EngineAssociation` > installed-engine lookup by GUID > unknown. (Final details: see the
commit for this change.)

Storage: GSettings keys `registered-projects` (`as`) and `project-engine-versions` (`a{ss}`), logic in
`src/models/project_registry.rs`. The installed app's compiled schema must be rebuilt (installer does it).

### 4.1 Version selection rule
Assets can go up in engine version, never down. Only (major, minor) counts. `choose_release` picks the
exact match, else the highest release not above the project's engine, else the lowest (with a warning).
Newer releases stay selectable, labelled "newer than the project".

### 4.2 Plugin download target
Fab plugin releases are laid out as `Engine/Plugins/Marketplace/<ReleaseId>/...`. `download_manager/
plugin_layout.rs` maps that (and `Engine/Plugins/<X>/`, `Plugins/<X>/`) to `<project>/Plugins/<PluginName>/`,
name taken from the `.uplugin` stem; ambiguous layouts abort. Plugins always download to the vault first.
End-to-end: "UI Effect Function Library" (5.8 release) -> `PrjEfz/Plugins/ExtendBlueprintNode/`, 20 files,
nothing else in the project touched, no Perforce commands (files are new/unopened in P4).

### 4.3 Fab library cache
`src/models/fab_cache.rs`: versioned `index.json`, atomic writes, corrupt cache ignored. Startup shows
cached data, merges only added/changed assets; removals only on explicit full refresh. The Fab API has no
"since" parameter, so the listing is still fetched each start; only processing is delta.

### 4.4 Archive intake (.rar/.7z/multi-volume)
Accepted: `.rar`, `.7z`, `.7z.001`/`.zip.001` splits, `.partN.rar` and `.rar`+`.r00` sets (deps: `sevenz-rust2`,
`unrar` with a `raw-dylib` advapi32 link fix on windows-gnu). A volume set is processed only when every volume is
present and stable; all volumes move together between folders. Identity = sha256 over all volumes. Extraction uses
the zip hardening rules, then repacks into a normal library zip. RAR entries are extracted to short sequential temp
names (`f000001`...) because unrar can't create long/non-ASCII paths; the real name is applied in the zip. Entry path
cap is 400 chars (Windows long paths are enabled on both machines). Names like `(5.0 – 5.8)` / `[5.0-5.3]` are
stripped; `engine_version_guess` = lowest, full range in sidecar `extra` (`engine_version_min/max`).
Encrypted -> needs-review `ENCRYPTED`; corrupt -> `ARCHIVE_CORRUPT`; incomplete RAR set -> retried.

### 4.5 Layout planner additions
* `__ExternalActors__` / `__ExternalObjects__` (OFPA) are excluded from root voting and kept under `Content/`.
* A rule held by >= 95% of resolved assets tolerates outliers (instead of `RULE_DISAGREE`).
* Own-path matching tolerates renamed assets (`<stem>` minus trailing `_<digits>`) when the directory rule agrees.
* Nested archives (outer archive with no assets, only inner archives): choose via sanitizer.json
  `prefer_engine_versions` (default `["5.8"]`), else highest version; different base names -> `NESTED_AMBIGUOUS`;
  depth <= 2. Sidecar records `nested_chosen`/`nested_skipped`.
* Server deploy: `_deploy/install.ps1` copies `*.exe` and `*.dll` (the exe needs `libwinpthread-1.dll`).

### 4.6 Merged Library page, install log and filters
Code: `src/models/library_merge.rs` (pure rules and tests), `src/models/install_log.rs`, `asset_data.rs` (merged
item), `library/mod.rs` (`add_asset`, `reconcile_local`, `dialog_parts`), `library/filter_bar.rs`,
`logged_in/download_dialog.rs`.

**Why every asset appeared twice.** Epic's library returns, for a migrated asset, a legacy `ue` catalog item (with
images) and a separate Fab-flavoured catalog item with the same title and no images. Measured on a real 2013-item
cache: 775 such pairs. In 145 pairs the Fab copy has more releases than the legacy item, so releases are unioned,
never dropped. Developer ids are not reliable enough to guard on. The group "Unreal Engine" (11 legacy items,
no Fab copy) must never merge.

**Merge rules.**
* Twin rule: items with the same `name_key` (clean_name, lower case, alphanumerics only) merge when the group has
  exactly one legacy item and one Fab copy (`can_join`). The item that has a thumbnail becomes the primary one; the
  other contributes its releases, categories and ids.
* Local zip to Library entry: a shared Fab listing id (Epic custom attribute `ListingIdentifier`, the Fab URL's last
  segment, or the `<zip>.fab.json` overlay `listing_uid`) matches. If both sides have listing ids and they share none,
  they do not match. Without ids a shared `name_key` matches. Local zips with no match show as their own entry
  (source Local only). Matching is re-run on every change (`reconcile_local`), so the result does not depend on
  whether the scan or the Epic listing finished first.
* Thumbnail: the entry uses whichever is populated (Epic image, Fab library image, overlay `thumbnail_url`).
* Version dropdown: all releases of all sources, each labelled `[Fab]` or `[Local]`, sorted newest engine version
  first. At the same version Fab sorts before Local (stable sort, `Source::Fab < Source::Local`), and index 0 is the
  default. The existing `choose_release` rule is then applied on top for a selected project (exact, else the highest
  not above the project, else the lowest); it keeps the earlier index on a tie, so Fab wins ties. A Local entry
  needs a project (it is imported with `safe_import`); with "vault only" the button is disabled.

**Install log.** `<user data dir>/epic_asset_manager/install_log.json`, `{"schema":1,"entries":[...]}`; entry =
asset id, name, source (`fab`|`local`), engine version, project path, project name, RFC 3339 timestamp. Writes are
read-modify-write under a process lock and replace the file atomically (temp file + rename). A corrupt file or a
different schema is moved to `install_log.json.bak` and a fresh log is started. Lookup (`latest_for`) matches by any
known id or by `name_key`, prefers the selected project's newest record, else the newest overall; the dialog shows
"Installed on MM/DD/YYYY" or "Installed on MM/DD/YYYY in <Project>". Epic/Fab downloads into a project are logged
as `fab` when extraction has finished and the copy into the project is queued (not after the copy completes);
local imports are logged as `local` after `safe_import` succeeds, from both the import dialog and the download
dialog. Downloads to the vault only are not logged.

**Filters.** The category dropdown became a "Filters" popover (`filter_bar.rs`) with check rows; enabled filters show
as removable pills in a row under the search bar. "Fab" and "Local" are the first two entries, on by default. An entry
passes when (Fab on and it has a Fab/Epic item) or (Local on and it has a local zip), and every selected category
matches. Categories combine with AND (each added filter narrows the list; OR would make adding a filter widen it,
which is surprising next to a search box). Categories of merged twins count; local-only entries map from their kind
(assets/plugins/projects). Local-only entries show while logged out or offline. The filter state is not persisted:
the GSettings schema in use can be stale and reading an unknown key panics. The filters apply to the Library page;
the Fab page keeps its own dropdown and the Local page its search box.

**Configuring the library directory (why "Local only" can show nothing).** The Library page lists local zips
only for directories in the GSettings key `local-library-directories` (default `[]`, so a fresh install shows no
local entries and "Fab only" is the only filter that lists anything). On Windows GSettings uses the registry
backend: values live per user in `HKCU\Software\GSettings\io\github\achetagames\epic_asset_manager` as GVariant
text, e.g. `['//scott-server/ue_assets_library/library']`. Set it in Preferences (Local Library directories) or
with `windows/install.ps1 -LibraryDir <dir>` (default `//scott-server/ue_assets_library/library`; written when the
setting is empty, or always when the parameter is passed explicitly; `-LibraryDir ''` leaves it alone). Forward
slashes avoid GVariant escaping; backslash and `//host/share` spellings both scan the same files. The directory
should be the sanitizer's `library` folder; a share root that has a `library` subfolder and no sidecars of its own
is redirected to it (`library_dir_of`). The host name `scott-server` resolves on the LAN (192.168.12.201 as a
fallback if name resolution fails). Ignored tests that need the share/display:
`cargo test --bin epic_asset_manager real_share -- --ignored --nocapture --test-threads=1` (set
`GSETTINGS_SCHEMA_DIR=<repo>/data/schemas` for the Library page one; it feeds the real zips into
`EpicLibraryBox::set_local_entries` without any Epic data: 55 zips -> 55 entries).

## 5. Verified vs not verified

Verified: crate tests (unit, integration, config; two-process claim race), sanitizer on copies of three
sample zips and on the real share (Camping, Operating Room, Advanced Crafting plugin; duplicate re-drop
handled), release build, standalone install run, Local Library scan and import programmatically,
Fab search/match/clear with a real session, download dialog opened on real Epic and Fab assets.

Also verified on the real share: .7z.001/.002 megapack, .rar plugin/content, Fuel System rar, Fabric rar, nested
Enhanced Timer Manager (5.8 chosen), PN Cleaver.

Not verified: multi-volume and encrypted RAR (unit tests only), Fab delta cache against the live API, version-label
click-through in the dialog, mouse/visual pass over the new UI, the project picker and "Create new project" flows,
`--watch` over days / task restart-on-failure, the Linux/Flatpak build after the workspace change,
offline hiding of the Match button, detail-pane legacy "Add to Project" panel (still the old one).
Section 4.6 (merge, install log, filters): pure logic is unit tested; the GTK click-through (pills, popover,
merged tiles, dialog labels, installed line) was not exercised in a display session.

## 6. Operating notes

* Dropping a zip into `import\` is enough; expect it in `library\` within ~40 s. Check `needs-review\`
  for `<name>.review.json` and `failed\` for `.error.txt`.
* App library paths: Preferences -> Local Library; add `\\scott-server\ue_assets_library\library`.
* Rebuild/install the app: `.\windows\install.ps1`.
* Do not commit `data/schemas/` (local schema cache) or `.claude/scratch`.
