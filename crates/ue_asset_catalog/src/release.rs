//! Release choice for a project: assets can go up in engine version, never down.
//!
//! Pure port of `choose_release` from the GTK app (`src/models/project_registry.rs`), kept here so
//! the catalog needs no GTK. Only (major, minor) counts.

/// (major, minor) engine version.
pub type Version = (u32, u32);

/// Parse "5.3", "5.3.2", "UE5.3" or "UE_5.3" into (major, minor).
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let s = s
        .trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '-')
        .trim();
    let mut it = s.split('.');
    let major: u32 = it.next()?.trim().parse().ok()?;
    let minor: u32 = it
        .next()?
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor))
}

pub fn format_version(v: Version) -> String {
    format!("{}.{}", v.0, v.1)
}

/// Which release to pick for a project on engine `project`.
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

    /// Short machine-readable verdict.
    pub const fn verdict(self) -> &'static str {
        match self {
            Self::Exact(_) => "exact",
            Self::Older(_) => "older",
            Self::AllNewer(_) => "newer",
        }
    }
}

/// Pick the release for a project on engine `project`.
///
/// `releases[i]` holds the (major, minor) engine versions of release `i`. The result is the release
/// with the highest version that is not above `project`; when every release is newer, the lowest
/// one. Ties go to the earlier release. `None` when no release has a usable version.
pub fn choose_release(project: Version, releases: &[Vec<Version>]) -> Option<ReleaseChoice> {
    let mut best: Option<(Version, usize)> = None;
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
    let mut lowest: Option<(Version, usize)> = None;
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
pub fn release_is_newer(project: Version, versions: &[Version]) -> bool {
    !versions.is_empty() && versions.iter().all(|v| *v > project)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &[&[Version]]) -> Vec<Vec<Version>> {
        v.iter().map(|x| x.to_vec()).collect()
    }

    #[test]
    fn exact_wins() {
        let rel = r(&[&[(5, 3)], &[(5, 5)], &[(5, 7)]]);
        assert_eq!(choose_release((5, 5), &rel), Some(ReleaseChoice::Exact(1)));
    }

    #[test]
    fn highest_not_above_project() {
        let rel = r(&[&[(5, 1)], &[(5, 3)], &[(5, 8)]]);
        assert_eq!(choose_release((5, 6), &rel), Some(ReleaseChoice::Older(1)));
    }

    #[test]
    fn never_downgrade_unless_all_newer() {
        let rel = r(&[&[(5, 8)], &[(5, 6)]]);
        assert_eq!(
            choose_release((5, 4), &rel),
            Some(ReleaseChoice::AllNewer(1))
        );
        assert!(release_is_newer((5, 4), &[(5, 6)]));
        assert!(!release_is_newer((5, 6), &[(5, 6)]));
    }

    #[test]
    fn ties_go_to_earlier_and_empty_is_none() {
        let rel = r(&[&[(5, 3)], &[(5, 3)]]);
        assert_eq!(choose_release((5, 3), &rel), Some(ReleaseChoice::Exact(0)));
        assert_eq!(choose_release((5, 3), &[]), None);
        assert_eq!(choose_release((5, 3), &r(&[&[], &[]])), None);
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("5.3"), Some((5, 3)));
        assert_eq!(parse_version("5.3.2"), Some((5, 3)));
        assert_eq!(parse_version("UE_5.4"), Some((5, 4)));
        assert_eq!(parse_version("{B9B25D9F-0000}"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(format_version((5, 3)), "5.3");
    }
}
