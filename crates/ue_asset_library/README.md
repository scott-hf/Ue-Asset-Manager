# ue_asset_library

Library crate plus the `ue_asset_sanitizer` service for a local Unreal asset library.
No GTK; the app crate depends on the library for scanning and importing.

## What the sanitizer does

Zips, `.7z`, `.rar` and multi-volume archives dropped directly into a job's `<import>/`
folder (or into an optional `<import>/unprocessed/` subfolder, kept for compatibility) are checked (path hardening,
size caps), classified (Content / Plugin / Project), rewritten to the layout Unreal
expects (`Content/<Folder>/...` or `Plugins/<Name>/...`, build output removed) and stored
in the job's library folder as `<slug>.<sha12>.zip` plus a `<slug>.<sha12>.zip.json`
sidecar. The sidecar is written last and is the commit marker. Originals are never
deleted, only moved between state folders.

### Archive inputs (`.7z`, `.rar`, multi-volume)

Supported names: `x.zip`, `x.7z`, `x.rar`, `x.7z.001/.002/...`, `x.zip.001/...`,
`x.part1.rar/x.part2.rar/...` and `x.rar` + `x.r00/x.r01/...`. Files are grouped per inbox
folder into volume sets. A set is processed only when every volume (1..N, no gaps) is present
and all volumes are stable; a set with a missing volume simply waits (reported as `waiting`,
nothing is moved). For `.7z` the expected total size is read from the 7z start header, so a
missing last volume is detected too; for RAR a missing trailing volume surfaces as a
`MISSING_VOLUME` result that puts the set back and retries without counting an attempt.
Only the first volume is the entry point: the claim file is `<first volume>.claim`, and all
volumes move together into `processing/`, then `processed/` (or `needs-review/` / `failed/`).

The archive is converted to a temp zip inside the library folder (same filesystem), then
the normal sanitizer runs on it, so the output is always `<slug>.<sha12>.zip` plus sidecar.
`sha256` (and the `sha12` in the file name) is the hash of the original archive: all volumes
in order, so duplicate detection works. The sidecar `source_zip` is the first volume's name;
`extra.source_format` (`7z`/`rar`/`split-zip`) and, for sets, `extra.source_volumes` are added.
Hardening is the same as for zips (`..`, absolute paths, drive/stream names, reserved names,
symlinks, case collisions, entry/size caps). Password-protected archives go to
`needs-review/` with code `ENCRYPTED`; damaged or unreadable ones with `ARCHIVE_CORRUPT`.

Libraries: `sevenz-rust2` (pure Rust, streams 7z straight into the temp zip; split 7z is read
through a chained reader) and `unrar` (bundled UnRAR C++ source, so no external binary).
RAR content is extracted to `library/.tmp-<id>/` under names the sanitizer chose, then zipped.
On windows-gnu the crate links `advapi32` itself (raw-dylib), which `unrar_sys` forgets.

Own-path matching is tolerant of files renamed after cooking: an asset whose package name
equals its file stem minus a trailing `_<digits>` (e.g. `Anim_X_10.uasset` holding
`.../Anim_X`) is counted as resolved, but only when it maps to a rule the exact matches
already voted for (never a new root).

Names: version ranges such as `Pack (5.0 – 5.8)`, `Pack[5.0-5.3]` are stripped from the display
name; `engine_version_guess` is the lowest endpoint and `extra.engine_version_min` /
`engine_version_max` hold the range. Archive suffixes (`.7z`, `.rar`, `.001`, `.part1.rar`) are
removed before cleaning.

## Import folder states

| Folder | Meaning |
| --- | --- |
| `<import>/` itself | The inbox. Zips lying directly here are picked up once their size and modified time stay unchanged for `stable_secs`. Subfolders are never treated as inboxes. `*.part`, `*.tmp`, `*.claim` are ignored; other non-archive files (e.g. a loose `.uasset`) are reported and left in place. |
| `unprocessed/` | Optional second inbox (compatibility). Missing or empty is fine; it is never created. |
| `processing/` | A zip being worked on, next to a `<name>.claim` file (host, pid, start time). Exactly one process wins a claim. Two zips with the same name in both inboxes are handled one after the other; `processed/` keeps both (a unique suffix is added). |
| `processed/` | Done (also duplicates of an already stored zip, with a `.duplicate.txt` note). |
| `needs-review/` | Not safe or not unambiguous (multi-root, no `/Game/` match, `..` paths, case collisions, ...). A `<name>.review.json` explains why. |
| `failed/` | Unreadable or still truncated after 3 attempts, with a `.error.txt`. |

At startup each job is reconciled after a crash: old `library/.tmp-*` files are removed, a
library zip without sidecar goes to `needs-review/`, and claims with no zip are dropped.
A zip stuck in `processing/` longer than `stale_mins` is reported; `--requeue` moves it
back to the import root.

## Config: `sanitizer.json`

```json
{
  "jobs": [
    { "import": "D:/ue_assets_library/import", "library": "D:/ue_assets_library/library" },
    { "import": "//scott-server/ue_assets_library/import", "library": "//scott-server/ue_assets_library/library" }
  ],
  "poll_secs": 30,
  "stable_secs": 5,
  "stale_mins": 120,
  "prefer_engine_versions": ["5.8"]
}
```

* `jobs`: any number; each is processed independently. An unreachable share, a missing
  import folder or an unwritable library fails only that job (it is logged once and
  retried every poll in `--watch`).
* `prefer_engine_versions` (default `["5.8"]`, order = priority): when an archive holds no
  assets of its own but one inner archive per engine version (e.g. `Pack 5.7.zip` +
  `Pack 5.8.zip`), the first listed version found is used; if none matches, the highest version
  present. The sidecar records `extra.nested_chosen` / `extra.nested_skipped`;
  `engine_version_guess` is the chosen inner version and the hash/`source_zip` stay the outer
  archive's. Inner archives with different base names -> `needs-review` code `NESTED_AMBIGUOUS`;
  more than 2 nesting levels -> `NESTED_TOO_DEEP`. Entries must be single-file zip/7z/rar.
* `poll_secs`, `stable_secs`, `stale_mins` are optional (defaults shown). Unknown keys are
  rejected so typos do not pass silently.
* Relative paths are resolved against the config file's folder. Use forward slashes or
  doubled backslashes in JSON. The import folder must already exist (it is never created
  for you); the library folder is created when missing.

An example lives in `sanitizer.example.json`.

## Running

```
ue_asset_sanitizer --config sanitizer.json --once     # one pass over every job, then exit
ue_asset_sanitizer --config sanitizer.json --watch    # poll forever (no inotify on SMB)
ue_asset_sanitizer --import <dir> --library <dir> --once   # single ad-hoc job
```

* Without `--config` and without `--import`/`--library`, `sanitizer.json` next to the
  executable is used.
* `--import/--library` together with `--config` is an error. The ad-hoc form never loads a
  default config.
* `--poll-secs`, `--stable-secs`, `--stale-mins` override the config values; the effective
  values are printed at startup. `--requeue` applies to every job.
* Exit codes: `0` ok, `1` usage or config error, `2` some zip went to `needs-review/` or
  `failed/`, `3` some job was unavailable (`--once` only).

## Windows scheduled task

Run it at logon (or at startup under a service account that can reach the shares) and
restart it if it stops:

```
schtasks /Create /TN "UE Asset Sanitizer" /SC ONLOGON /RL LIMITED /F ^
  /TR "\"C:\Tools\ue_asset_sanitizer.exe\" --watch --config \"C:\Tools\sanitizer.json\""
schtasks /Run /TN "UE Asset Sanitizer"
schtasks /Query /TN "UE Asset Sanitizer" /V /FO LIST
schtasks /End /TN "UE Asset Sanitizer"
```

Use `/SC ONSTART /RU <account> /RP *` for a task that runs without a logged-in user.
For a periodic one-shot instead of a resident process use
`/SC MINUTE /MO 5 ... --once --config ...`. Use absolute paths, or keep the paths in the
config relative to the config file. `ue_asset_sanitizer` prints to stdout; redirect it in
the task command (`cmd /c "... >> C:\Tools\sanitizer.log 2>&1"`) when you want a log.

## Tests

`cargo test -p ue_asset_library`. `tests/archives.rs` builds its 7z / split fixtures in
process (no RAR writer exists, so RAR is covered by garbage-input tests and manual runs against
real archives). Tests that use the three real sample zips are skipped
when they are not present (`UE_ASSET_SAMPLES` overrides the folder).
