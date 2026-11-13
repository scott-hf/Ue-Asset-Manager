//! App-owned log of installed assets: which asset (Fab release or local library zip) was put into
//! which project, with which engine version, and when.
//!
//! The log is one small JSON file in the app data dir (`install_log.json`, next to `eam.db`):
//! `{"schema":1,"entries":[...]}`. It is written atomically (temp file + rename, like the Fab
//! cache), so a crash never leaves half a file. A file that cannot be read or has another schema
//! version is moved aside to `install_log.json.bak` and a fresh log is started.

use crate::models::fab_cache::write_atomic;
use crate::models::library_merge::{name_key, Source};
use crate::models::project_registry::path_key;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use log::{error, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const SCHEMA_VERSION: u32 = 1;
pub const FILE_NAME: &str = "install_log.json";

/// Serialises the read-modify-write cycle of `append` within the process.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// Epic catalog id / Fab asset id for marketplace installs, the sha256 for local zips.
    pub asset_id: String,
    pub name: String,
    /// `"fab"` or `"local"` (see `library_merge::Source::key`). Installs of Epic vault items are
    /// logged as `"fab"`: the Epic marketplace content is Fab content.
    pub source: String,
    pub engine_version: String,
    /// The `.uproject` file (or the project root) the asset was installed into.
    pub project_path: String,
    pub project_name: String,
    /// RFC 3339, UTC.
    pub installed_at: String,
}

impl InstallRecord {
    /// A record for an install that is about to happen; `installed_at` is filled in when it is
    /// written to the log.
    pub fn new(
        asset_id: &str,
        name: &str,
        source: Source,
        engine_version: &str,
        project_path: &str,
        project_name: &str,
    ) -> Self {
        Self {
            asset_id: asset_id.to_string(),
            name: name.to_string(),
            source: source.key().to_string(),
            engine_version: engine_version.to_string(),
            project_path: project_path.to_string(),
            project_name: project_name.to_string(),
            installed_at: String::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LogFile {
    schema: u32,
    entries: Vec<InstallRecord>,
}

pub fn default_path() -> PathBuf {
    gtk4::glib::user_data_dir()
        .join("epic_asset_manager")
        .join(FILE_NAME)
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Read all records. A missing file is an empty log; an unreadable one is moved aside.
pub fn load(path: &Path) -> Vec<InstallRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<LogFile>(&text) {
        Ok(file) if file.schema == SCHEMA_VERSION => file.entries,
        Ok(file) => {
            warn!(
                "Install log {} has schema {} (expected {SCHEMA_VERSION}); starting a new log",
                path.display(),
                file.schema
            );
            move_aside(path);
            Vec::new()
        }
        Err(e) => {
            warn!(
                "Install log {} is unreadable ({e}); starting a new log",
                path.display()
            );
            move_aside(path);
            Vec::new()
        }
    }
}

fn move_aside(path: &Path) {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    if let Err(e) = std::fs::rename(path, PathBuf::from(backup)) {
        warn!("Could not move the install log aside: {e}");
    }
}

fn save(path: &Path, entries: Vec<InstallRecord>) -> std::io::Result<()> {
    let file = LogFile {
        schema: SCHEMA_VERSION,
        entries,
    };
    let bytes = serde_json::to_vec_pretty(&file)?;
    write_atomic(path, &bytes)
}

/// Add one record to the log file at `path`.
pub fn append(path: &Path, record: InstallRecord) -> std::io::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut entries = load(path);
    entries.push(record);
    save(path, entries)
}

/// Add one record to the app's install log. Failures are logged, never fatal: the install itself
/// already succeeded.
pub fn record_install(mut record: InstallRecord) {
    if record.installed_at.is_empty() {
        record.installed_at = now_rfc3339();
    }
    let path = default_path();
    if let Err(e) = append(&path, record) {
        error!("Could not write the install log {}: {e}", path.display());
    }
}

/// Records that are installs of the asset described by `ids` (asset ids) or `name_keys`
/// (`library_merge::name_key` of the asset's names).
fn matching<'a>(
    records: &'a [InstallRecord],
    ids: &'a [String],
    name_keys: &'a [String],
) -> impl Iterator<Item = &'a InstallRecord> {
    records.iter().filter(move |r| {
        ids.iter().any(|i| i == &r.asset_id) || {
            let key = name_key(&r.name);
            !key.is_empty() && name_keys.contains(&key)
        }
    })
}

fn time_of(record: &InstallRecord) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&record.installed_at)
        .map_or(DateTime::<Utc>::MIN_UTC, |t| t.with_timezone(&Utc))
}

/// The record to show for an asset: when a project is selected, the most recent install into that
/// project; otherwise (or when it was never installed there) the most recent install anywhere.
pub fn latest_for<'a>(
    records: &'a [InstallRecord],
    ids: &'a [String],
    name_keys: &'a [String],
    project: Option<&str>,
) -> Option<&'a InstallRecord> {
    let newest =
        |it: Box<dyn Iterator<Item = &'a InstallRecord> + 'a>| it.max_by_key(|r| time_of(r));
    if let Some(project) = project.filter(|p| !p.is_empty()) {
        let wanted = path_key(project);
        let in_project =
            matching(records, ids, name_keys).filter(move |r| path_key(&r.project_path) == wanted);
        if let Some(found) = newest(Box::new(in_project)) {
            return Some(found);
        }
    }
    newest(Box::new(matching(records, ids, name_keys)))
}

/// "Installed on MM/DD/YYYY", plus " in <project>" when the project is known. The date is shown in
/// the given time zone (the user's local zone in the app).
pub fn installed_line<Tz: TimeZone>(record: &InstallRecord, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let date = DateTime::parse_from_rfc3339(&record.installed_at).map_or_else(
        |_| record.installed_at.clone(),
        |t| t.with_timezone(tz).format("%m/%d/%Y").to_string(),
    );
    let project = if record.project_name.is_empty() {
        Path::new(&record.project_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        record.project_name.clone()
    };
    if project.is_empty() {
        format!("Installed on {date}")
    } else {
        format!("Installed on {date} in {project}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eam-install-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rec(id: &str, name: &str, project: &str, at: &str) -> InstallRecord {
        InstallRecord {
            asset_id: id.to_string(),
            name: name.to_string(),
            source: "fab".to_string(),
            engine_version: "5.4".to_string(),
            project_path: project.to_string(),
            project_name: Path::new(project)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            installed_at: at.to_string(),
        }
    }

    #[test]
    fn missing_file_is_an_empty_log() {
        let dir = temp_dir("missing");
        assert!(load(&dir.join(FILE_NAME)).is_empty());
    }

    #[test]
    fn append_then_load_round_trips_and_keeps_order() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(FILE_NAME);
        let a = rec(
            "a",
            "Alpha",
            "C:/P/One/One.uproject",
            "2026-11-10T08:00:00Z",
        );
        let b = InstallRecord {
            source: "local".to_string(),
            ..rec(
                "sha",
                "Beta",
                "C:/P/Two/Two.uproject",
                "2026-11-11T08:00:00Z",
            )
        };
        append(&path, a.clone()).unwrap();
        append(&path, b.clone()).unwrap();
        assert_eq!(load(&path), vec![a, b]);
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["entries"][1]["source"], "local");
        // no temp file is left behind
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn corrupt_log_is_moved_aside_and_restarted() {
        let dir = temp_dir("corrupt");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_empty());
        assert!(dir.join("install_log.json.bak").exists());
        append(
            &path,
            rec("a", "Alpha", "C:/P/One.uproject", "2026-11-10T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(load(&path).len(), 1);
    }

    #[test]
    fn foreign_schema_is_moved_aside() {
        let dir = temp_dir("schema");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, r#"{"schema":99,"entries":[]}"#).unwrap();
        assert!(load(&path).is_empty());
        assert!(dir.join("install_log.json.bak").exists());
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn lookup_finds_the_latest_install_by_id_or_by_name() {
        let records = vec![
            rec(
                "a",
                "Alpha Plugin",
                "C:/P/One/One.uproject",
                "2026-01-01T00:00:00Z",
            ),
            rec(
                "other-id",
                "Alpha",
                "C:/P/Two/Two.uproject",
                "2026-03-01T00:00:00Z",
            ),
            rec("b", "Beta", "C:/P/One/One.uproject", "2026-04-01T00:00:00Z"),
        ];
        // by id only
        let ids = strings(&["a"]);
        let names = strings(&["alpha"]);
        let found = latest_for(&records, &ids, &[], None).unwrap();
        assert_eq!(found.asset_id, "a");
        // by cleaned name: "Alpha Plugin" and "Alpha" are the same asset, the newer one wins
        let found = latest_for(&records, &ids, &names, None).unwrap();
        assert_eq!(found.asset_id, "other-id");
        // never installed
        assert!(latest_for(&records, &strings(&["zzz"]), &strings(&["zzz"]), None).is_none());
        assert!(latest_for(&[], &strings(&["a"]), &[], None).is_none());
    }

    #[test]
    fn lookup_prefers_the_selected_project() {
        let records = vec![
            rec(
                "a",
                "Alpha",
                "C:/P/One/One.uproject",
                "2026-01-01T00:00:00Z",
            ),
            rec(
                "a",
                "Alpha",
                "C:/P/Two/Two.uproject",
                "2026-05-01T00:00:00Z",
            ),
            rec(
                "a",
                "Alpha",
                "C:/P/One/One.uproject",
                "2026-02-01T00:00:00Z",
            ),
        ];
        let ids = strings(&["a"]);
        // project One: its own latest install, not the newer one in Two
        let found = latest_for(&records, &ids, &[], Some("C:/P/One/One.uproject")).unwrap();
        assert_eq!(found.installed_at, "2026-02-01T00:00:00Z");
        // path comparison ignores separator style
        let found = latest_for(&records, &ids, &[], Some("C:\\P\\One\\One.uproject")).unwrap();
        assert_eq!(found.installed_at, "2026-02-01T00:00:00Z");
        // project without an install of this asset: falls back to the latest overall
        let found = latest_for(&records, &ids, &[], Some("C:/P/Three/Three.uproject")).unwrap();
        assert_eq!(found.installed_at, "2026-05-01T00:00:00Z");
        // an empty selection counts as no project
        let found = latest_for(&records, &ids, &[], Some("")).unwrap();
        assert_eq!(found.installed_at, "2026-05-01T00:00:00Z");
    }

    #[test]
    fn installed_line_formats_the_date_and_project() {
        let r = rec(
            "a",
            "Alpha",
            "C:/P/Shooter/Shooter.uproject",
            "2026-11-12T10:30:00Z",
        );
        assert_eq!(
            installed_line(&r, &Utc),
            "Installed on 11/12/2026 in Shooter"
        );
        let no_project = InstallRecord {
            project_name: String::new(),
            project_path: String::new(),
            ..r.clone()
        };
        assert_eq!(installed_line(&no_project, &Utc), "Installed on 11/12/2026");
        let bad_date = InstallRecord {
            installed_at: "yesterday".to_string(),
            ..r
        };
        assert_eq!(
            installed_line(&bad_date, &Utc),
            "Installed on yesterday in Shooter"
        );
    }

    #[test]
    fn now_is_a_parsable_utc_timestamp() {
        let now = now_rfc3339();
        assert!(now.ends_with('Z'));
        assert!(DateTime::parse_from_rfc3339(&now).is_ok());
    }
}
