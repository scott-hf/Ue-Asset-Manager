//! Append to the app's install log (`<user data dir>/epic_asset_manager/install_log.json`), same
//! format as the app's `src/models/install_log.rs`: `{"schema":1,"entries":[...]}`, written
//! atomically (temp file + rename). A corrupt file or another schema is moved aside to
//! `install_log.json.bak` and a fresh log is started.
//!
//! The app's writer lives in the GTK crate and cannot be linked from here, so this is a small
//! compatible copy. The read-modify-write is not locked against the app (a rare lost update is
//! possible when both write in the same instant; the install itself is never affected).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const FILE_NAME: &str = "install_log.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// The sha256 of the library zip for local installs.
    pub asset_id: String,
    pub name: String,
    /// `local` for library zips.
    pub source: String,
    pub engine_version: String,
    /// The `.uproject` file the asset was installed into.
    pub project_path: String,
    pub project_name: String,
    /// RFC 3339, UTC.
    pub installed_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LogFile {
    schema: u32,
    entries: Vec<serde_json::Value>,
}

/// `<user data dir>/epic_asset_manager/install_log.json` without GLib: `%LOCALAPPDATA%` on Windows
/// (GLib's user data dir there), `$XDG_DATA_HOME` or `~/.local/share` elsewhere.
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }?;
    Some(base.join("epic_asset_manager").join(FILE_NAME))
}

/// `<user cache dir>/ue_asset_catalog/catalog.json` (`%LOCALAPPDATA%` on Windows,
/// `$XDG_CACHE_HOME` or `~/.cache` elsewhere).
pub fn default_cache_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("ue_asset_catalog").join("catalog.json"))
}

fn move_aside(path: &Path) {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    let _ = fs::rename(path, PathBuf::from(backup));
}

fn load_entries(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<LogFile>(&text) {
        Ok(f) if f.schema == SCHEMA_VERSION => f.entries,
        _ => {
            move_aside(path);
            Vec::new()
        }
    }
}

/// Add one record. Entries written by the app are kept verbatim.
pub fn append(path: &Path, record: &InstallRecord) -> std::io::Result<()> {
    let mut entries = load_entries(path);
    entries.push(serde_json::to_value(record)?);
    let bytes = serde_json::to_vec_pretty(&LogFile {
        schema: SCHEMA_VERSION,
        entries,
    })?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// RFC 3339 UTC timestamp for "now" (no chrono; Howard Hinnant's civil-from-days).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339_from_secs(secs)
}

pub fn rfc3339_from_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Tmp;

    fn rec(n: &str) -> InstallRecord {
        InstallRecord {
            asset_id: "abc".into(),
            name: n.into(),
            source: "local".into(),
            engine_version: "5.3".into(),
            project_path: "E:/P/P.uproject".into(),
            project_name: "P".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn appends_and_keeps_app_entries() {
        let t = Tmp::new();
        let p = t.path().join("sub/install_log.json");
        append(&p, &rec("one")).unwrap();
        append(&p, &rec("two")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["entries"].as_array().unwrap().len(), 2);
        assert_eq!(v["entries"][1]["name"], "two");
    }

    #[test]
    fn corrupt_log_is_moved_aside() {
        let t = Tmp::new();
        let p = t.path().join("install_log.json");
        fs::write(&p, "not json").unwrap();
        append(&p, &rec("one")).unwrap();
        assert!(t.path().join("install_log.json.bak").exists());
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339_from_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_secs(1_700_000_000), "2023-11-14T22:13:20Z");
        assert!(now_rfc3339().ends_with('Z'));
    }
}
