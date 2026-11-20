//! Plain-text persistence of the Epic login tokens, independent of GSettings
//! (the Windows registry backend does not reliably keep them) and of the
//! Secret Service. Stored as JSON in `<user data dir>/epic_asset_manager/epic_auth.json`.
//!
//! Token values are never logged.

use chrono::{DateTime, Utc};
use egs_api::api::types::account::UserData;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

const SCHEMA: u32 = 1;
const FILE_NAME: &str = "epic_auth.json";
/// Same margin the rest of the login code uses (see `tools::auth`).
const MIN_TOKEN_VALIDITY_SECS: i64 = 600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAuth {
    pub schema: u32,
    pub access_token: Option<String>,
    pub token_type: Option<String>,
    /// RFC 3339
    pub expires_at: Option<String>,
    pub refresh_token: Option<String>,
    /// RFC 3339
    pub refresh_expires_at: Option<String>,
}

fn fmt_time(t: Option<DateTime<Utc>>) -> Option<String> {
    t.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn parse_time(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(crate::tools::auth::parse_token_time)
}

fn usable(now: DateTime<Utc>, exp: Option<DateTime<Utc>>) -> bool {
    exp.is_some_and(|e| (e - now).num_seconds() > MIN_TOKEN_VALIDITY_SECS)
}

impl StoredAuth {
    pub fn from_user_data(ud: &UserData) -> Self {
        Self {
            schema: SCHEMA,
            access_token: ud.access_token().map(String::from),
            token_type: ud.token_type.clone(),
            expires_at: fmt_time(ud.expires_at),
            refresh_token: ud.refresh_token().map(String::from),
            refresh_expires_at: fmt_time(ud.refresh_expires_at),
        }
    }

    pub fn access_expiry(&self) -> Option<DateTime<Utc>> {
        parse_time(&self.expires_at)
    }

    pub fn refresh_expiry(&self) -> Option<DateTime<Utc>> {
        parse_time(&self.refresh_expires_at)
    }

    /// True when there is a non-empty access token that is still good for longer than the margin.
    pub fn access_usable(&self, now: DateTime<Utc>) -> bool {
        self.access_token.as_deref().is_some_and(|t| !t.is_empty())
            && usable(now, self.access_expiry())
    }

    /// True when there is a non-empty refresh token that is still good for longer than the margin.
    pub fn refresh_usable(&self, now: DateTime<Utc>) -> bool {
        self.refresh_token.as_deref().is_some_and(|t| !t.is_empty())
            && usable(now, self.refresh_expiry())
    }

    /// Build the `UserData` to restore at startup: tokens that are expired (by the
    /// usual margin) are left out, so the caller falls back to the refresh grant.
    pub fn to_user_data(&self, now: DateTime<Utc>) -> UserData {
        let mut ud = UserData::new();
        if self.access_usable(now) {
            ud.set_access_token(self.access_token.clone());
            ud.expires_at = self.access_expiry();
            ud.token_type = Some(
                self.token_type
                    .clone()
                    .unwrap_or_else(|| "bearer".to_string()),
            );
        }
        if self.refresh_usable(now) {
            ud.set_refresh_token(self.refresh_token.clone());
            ud.refresh_expires_at = self.refresh_expiry();
        }
        ud
    }
}

pub fn default_path() -> PathBuf {
    gtk4::glib::user_data_dir()
        .join("epic_asset_manager")
        .join(FILE_NAME)
}

/// Atomic write: temp file in the same directory, then rename over the target.
pub fn save_to(path: &Path, auth: &StoredAuth) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(auth).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Missing, unreadable or corrupt files yield `None`.
pub fn load_from(path: &Path) -> Option<StoredAuth> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Unable to read stored login {}: {}", path.display(), e);
            }
            return None;
        }
    };
    match serde_json::from_slice::<StoredAuth>(&data) {
        Ok(a) if a.schema == SCHEMA => Some(a),
        Ok(a) => {
            warn!("Ignoring stored login with unknown schema {}", a.schema);
            None
        }
        Err(e) => {
            warn!("Ignoring corrupt stored login file: {}", e);
            None
        }
    }
}

pub fn clear_at(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => info!("Stored login removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("Unable to remove stored login {}: {}", path.display(), e),
    }
}

/// Persist the current session; logs the outcome (no token values).
pub fn save(ud: &UserData) {
    if ud.access_token().is_none() && ud.refresh_token().is_none() {
        return;
    }
    let path = default_path();
    let auth = StoredAuth::from_user_data(ud);
    match save_to(&path, &auth) {
        Ok(()) => info!(
            "Stored login saved to {} (access until {}, refresh until {})",
            path.display(),
            auth.expires_at.as_deref().unwrap_or("-"),
            auth.refresh_expires_at.as_deref().unwrap_or("-")
        ),
        Err(e) => warn!("Failed to store login to {}: {}", path.display(), e),
    }
}

pub fn load() -> Option<StoredAuth> {
    let path = default_path();
    let auth = load_from(&path);
    match &auth {
        Some(a) => {
            let now = Utc::now();
            info!(
                "Stored login loaded from {} (access usable: {}, refresh usable: {})",
                path.display(),
                a.access_usable(now),
                a.refresh_usable(now)
            );
        }
        None => info!("No stored login at {}", path.display()),
    }
    auth
}

pub fn clear() {
    clear_at(&default_path());
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn temp_file() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eam_auth_store_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE_NAME)
    }

    fn sample(now: DateTime<Utc>, access_h: i64, refresh_h: i64) -> StoredAuth {
        let mut ud = UserData::new();
        ud.set_access_token(Some("a-token".to_string()));
        ud.token_type = Some("bearer".to_string());
        ud.expires_at = Some(now + Duration::hours(access_h));
        ud.set_refresh_token(Some("r-token".to_string()));
        ud.refresh_expires_at = Some(now + Duration::hours(refresh_h));
        StoredAuth::from_user_data(&ud)
    }

    #[test]
    fn round_trip() {
        let path = temp_file();
        let now = Utc::now();
        let a = sample(now, 8, 24);
        save_to(&path, &a).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, a);
        let ud = loaded.to_user_data(now);
        assert_eq!(ud.access_token(), Some("a-token"));
        assert_eq!(ud.refresh_token(), Some("r-token"));
        assert_eq!(ud.token_type.as_deref(), Some("bearer"));
        assert!(ud.refresh_expires_at.is_some());
        // overwrite works (rename over existing file)
        let b = sample(now, 1, 2);
        save_to(&path, &b).unwrap();
        assert_eq!(load_from(&path).unwrap(), b);
        clear_at(&path);
        assert!(load_from(&path).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_is_none() {
        assert!(load_from(&temp_file()).is_none());
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let path = temp_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_from(&path).is_none());
        std::fs::write(&path, br#"{"schema":99}"#).unwrap();
        assert!(load_from(&path).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn expired_access_keeps_valid_refresh() {
        let now = Utc::now();
        let a = sample(now, -1, 24);
        assert!(!a.access_usable(now));
        assert!(a.refresh_usable(now));
        let ud = a.to_user_data(now);
        assert!(ud.access_token().is_none());
        assert_eq!(ud.refresh_token(), Some("r-token"));
    }

    #[test]
    fn nearly_expired_tokens_are_dropped() {
        let now = Utc::now();
        let mut a = sample(now, 8, 24);
        a.expires_at = fmt_time(Some(now + Duration::seconds(300)));
        a.refresh_expires_at = fmt_time(Some(now + Duration::seconds(600)));
        assert!(!a.access_usable(now));
        assert!(!a.refresh_usable(now));
        let ud = a.to_user_data(now);
        assert!(ud.access_token().is_none() && ud.refresh_token().is_none());
    }

    #[test]
    fn missing_expiry_is_unusable() {
        let now = Utc::now();
        let mut a = sample(now, 8, 24);
        a.expires_at = None;
        a.refresh_expires_at = Some("garbage".to_string());
        assert!(!a.access_usable(now));
        assert!(!a.refresh_usable(now));
    }
}
