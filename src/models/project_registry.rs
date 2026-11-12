//! Individually registered Unreal projects and their engine version overrides.
//!
//! Projects can be registered by `.uproject` file (in addition to the scanned
//! `unreal-projects-directories`), and a project whose `EngineAssociation` cannot be
//! turned into a version (custom engine builds use a GUID) can carry an override.
//! Version resolution order: override > numeric `EngineAssociation` > installed engine.

use gtk4::{gio, prelude::*};
use std::collections::HashMap;

const REGISTERED_KEY: &str = "registered-projects";
const VERSION_KEY: &str = "project-engine-versions";

/// Where a resolved engine version came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionSource {
    Override,
    Association,
    Installed,
}

impl VersionSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Association => "from engine association",
            Self::Installed => "installed engine",
        }
    }
}

/// Stable key for a path: forward slashes, lower case on Windows.
pub fn path_key(path: &str) -> String {
    let p = path.replace('\\', "/");
    if cfg!(windows) {
        p.to_lowercase()
    } else {
        p
    }
}

/// Reduce "UE_5.8", "5.8.1" or "5" to "5.8" / "5.0"; `None` for GUIDs and other text.
pub fn normalize_version(raw: &str) -> Option<String> {
    let s = raw
        .trim()
        .trim_start_matches("UE_")
        .trim_start_matches("UE ")
        .trim();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let mut parts = s.split('.').filter(|p| !p.is_empty());
    let major = parts.next()?;
    let minor = parts.next().unwrap_or("0");
    Some(format!("{major}.{minor}"))
}

/// (major, minor) of a version text such as "UE_5.8" or "5.8.1"; the patch is ignored.
pub fn version_tuple(raw: &str) -> Option<(u32, u32)> {
    let normalized = normalize_version(raw)?;
    let (major, minor) = normalized.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Which release to pick for a project: assets can go up in versions, never down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseChoice {
    /// A release for exactly the project's version.
    Exact(usize),
    /// The highest release older than the project's version.
    Older(usize),
    /// Every release is newer than the project; this is the lowest of them.
    AllNewer(usize),
}

impl ReleaseChoice {
    pub const fn index(self) -> usize {
        match self {
            Self::Exact(i) | Self::Older(i) | Self::AllNewer(i) => i,
        }
    }
}

/// Pick the release to download for a project on engine `project`.
///
/// `releases[i]` holds the (major, minor) engine versions of release `i`. The result is the release
/// with the highest version that is not above `project`; when every release is newer, the lowest
/// one. Ties go to the earlier release. `None` when no release has a usable version.
pub fn choose_release(project: (u32, u32), releases: &[Vec<(u32, u32)>]) -> Option<ReleaseChoice> {
    let mut best: Option<((u32, u32), usize)> = None;
    for (i, versions) in releases.iter().enumerate() {
        let Some(top) = versions.iter().copied().filter(|v| *v <= project).max() else {
            continue;
        };
        if best.is_none_or(|(b, _)| top > b) {
            best = Some((top, i));
        }
    }
    if let Some((version, i)) = best {
        return Some(if version == project {
            ReleaseChoice::Exact(i)
        } else {
            ReleaseChoice::Older(i)
        });
    }
    let mut lowest: Option<((u32, u32), usize)> = None;
    for (i, versions) in releases.iter().enumerate() {
        let Some(low) = versions.iter().copied().min() else {
            continue;
        };
        if lowest.is_none_or(|(l, _)| low < l) {
            lowest = Some((low, i));
        }
    }
    lowest.map(|(_, i)| ReleaseChoice::AllNewer(i))
}

/// `true` when every version of the release is newer than the project's.
pub fn release_is_newer(project: (u32, u32), versions: &[(u32, u32)]) -> bool {
    !versions.is_empty() && versions.iter().all(|v| *v > project)
}

pub fn registered_projects(settings: &gio::Settings) -> Vec<String> {
    settings
        .strv(REGISTERED_KEY)
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

/// Register a `.uproject` file. Returns `true` when it was not registered before.
pub fn register_project(settings: &gio::Settings, uproject: &str) -> bool {
    let mut current = registered_projects(settings);
    let key = path_key(uproject);
    if current.iter().any(|p| path_key(p) == key) {
        return false;
    }
    current.push(uproject.to_string());
    settings.set_strv(REGISTERED_KEY, current).is_ok()
}

fn overrides(settings: &gio::Settings) -> HashMap<String, String> {
    settings
        .value(VERSION_KEY)
        .get::<HashMap<String, String>>()
        .unwrap_or_default()
}

pub fn engine_override(settings: &gio::Settings, uproject: &str) -> Option<String> {
    overrides(settings)
        .get(&path_key(uproject))
        .cloned()
        .filter(|v| !v.is_empty())
}

/// Set (or clear with `None` / empty text) the engine version override of a project.
pub fn set_engine_override(
    settings: &gio::Settings,
    uproject: &str,
    version: Option<&str>,
) -> bool {
    let mut map = overrides(settings);
    let key = path_key(uproject);
    match version.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => {
            map.insert(key, v.to_string());
        }
        None => {
            map.remove(&key);
        }
    }
    settings.set_value(VERSION_KEY, &map.to_variant()).is_ok()
}

/// Resolve the engine version of a project: override > numeric association > installed engine.
/// `installed` looks up the version of an installed engine matching the association (by GUID).
pub fn resolve_engine_version(
    settings: &gio::Settings,
    uproject: &str,
    association: &str,
    installed: impl FnOnce() -> Option<String>,
) -> Option<(String, VersionSource)> {
    if let Some(v) = engine_override(settings, uproject).and_then(|v| normalize_version(&v)) {
        return Some((v, VersionSource::Override));
    }
    if let Some(v) = normalize_version(association) {
        return Some((v, VersionSource::Association));
    }
    installed()
        .and_then(|v| normalize_version(&v))
        .map(|v| (v, VersionSource::Installed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(list: &[&[(u32, u32)]]) -> Vec<Vec<(u32, u32)>> {
        list.iter().map(|v| v.to_vec()).collect()
    }

    #[test]
    fn versions_compare_as_numbers_and_ignore_patch() {
        assert_eq!(version_tuple("UE_5.8"), Some((5, 8)));
        assert_eq!(version_tuple("5.8.1"), Some((5, 8)));
        assert_eq!(version_tuple("4.27"), Some((4, 27)));
        assert_eq!(version_tuple("5"), Some((5, 0)));
        assert_eq!(version_tuple("{B9B25D9F-4B37}"), None);
        // 5.10 is newer than 5.9 (a string compare would say the opposite)
        assert!((5, 10) > (5, 9));
        assert_eq!(
            choose_release((5, 10), &r(&[&[(5, 9)], &[(5, 10)]])),
            Some(ReleaseChoice::Exact(1))
        );
    }

    #[test]
    fn exact_match_wins() {
        let rel = r(&[&[(5, 8)], &[(5, 7)], &[(5, 6)]]);
        assert_eq!(choose_release((5, 7), &rel), Some(ReleaseChoice::Exact(1)));
    }

    #[test]
    fn highest_release_not_above_the_project() {
        // asset only has 5.6 and 5.9, project is 5.8: 5.6, never 5.9
        let rel = r(&[&[(5, 9)], &[(5, 6)], &[(5, 0)], &[(4, 27)]]);
        assert_eq!(choose_release((5, 8), &rel), Some(ReleaseChoice::Older(1)));
    }

    #[test]
    fn multi_version_release_uses_its_best_fit() {
        let rel = r(&[&[(5, 3), (5, 4)], &[(5, 5), (5, 9)]]);
        assert_eq!(choose_release((5, 6), &rel), Some(ReleaseChoice::Older(1)));
        assert_eq!(choose_release((5, 4), &rel), Some(ReleaseChoice::Exact(0)));
    }

    #[test]
    fn all_newer_falls_back_to_the_lowest() {
        let rel = r(&[&[(5, 8)], &[(5, 6)], &[(5, 7)]]);
        assert_eq!(
            choose_release((5, 4), &rel),
            Some(ReleaseChoice::AllNewer(1))
        );
        assert_eq!(
            choose_release((5, 4), &rel).map(ReleaseChoice::index),
            Some(1)
        );
    }

    #[test]
    fn nothing_usable_gives_none() {
        assert_eq!(choose_release((5, 8), &[]), None);
        assert_eq!(choose_release((5, 8), &r(&[&[], &[]])), None);
    }

    #[test]
    fn newer_marker() {
        assert!(release_is_newer((5, 6), &[(5, 7)]));
        assert!(!release_is_newer((5, 6), &[(5, 6)]));
        assert!(!release_is_newer((5, 6), &[(5, 7), (5, 5)]));
        assert!(!release_is_newer((5, 6), &[]));
    }
}
