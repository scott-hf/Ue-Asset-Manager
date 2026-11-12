# ue_asset_library

Library crate plus the `ue_asset_sanitizer` service for a local Unreal asset library.
No GTK; the app crate depends on the library for scanning and importing.

## What the sanitizer does

Zips dropped into a job's `<import>/unprocessed/` folder are checked (path hardening,
size caps), classified (Content / Plugin / Project), rewritten to the layout Unreal
expects (`Content/<Folder>/...` or `Plugins/<Name>/...`, build output removed) and stored
in the job's library folder as `<slug>.<sha12>.zip` plus a `<slug>.<sha12>.zip.json`
sidecar. The sidecar is written last and is the commit marker. Originals are never
deleted, only moved between state folders.

## Import folder states

| Folder | Meaning |
| --- | --- |
| `unprocessed/` | The inbox, the only folder scanned. `*.part`, `*.tmp`, `*.claim` and non-zips are ignored. A zip is picked up once its size and modified time stay unchanged for `stable_secs`. |
| `processing/` | A zip being worked on, next to a `<name>.claim` file (host, pid, start time). Exactly one process wins a claim. |
| `processed/` | Done (also duplicates of an already stored zip, with a `.duplicate.txt` note). |
| `needs-review/` | Not safe or not unambiguous (multi-root, no `/Game/` match, `..` paths, case collisions, ...). A `<name>.review.json` explains why. |
| `failed/` | Unreadable or still truncated after 3 attempts, with a `.error.txt`. |

At startup each job is reconciled after a crash: old `library/.tmp-*` files are removed, a
library zip without sidecar goes to `needs-review/`, and claims with no zip are dropped.
A zip stuck in `processing/` longer than `stale_mins` is reported; `--requeue` moves it
back to `unprocessed/`.

## Config: `sanitizer.json`

```json
{
  "jobs": [
    { "import": "D:/ue_assets_library/import", "library": "D:/ue_assets_library/library" },
    { "import": "//scott-server/ue_assets_library/import", "library": "//scott-server/ue_assets_library/library" }
  ],
  "poll_secs": 30,
  "stable_secs": 5,
  "stale_mins": 120
}
```

* `jobs`: any number; each is processed independently. An unreachable share, a missing
  import folder or an unwritable library fails only that job (it is logged once and
  retried every poll in `--watch`).
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

`cargo test -p ue_asset_library`. Tests that use the three real sample zips are skipped
when they are not present (`UE_ASSET_SAMPLES` overrides the folder).
