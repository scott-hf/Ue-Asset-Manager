//! Display-name cleanup for dropped archive file names.

use regex::Regex;
use std::sync::OnceLock;

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.*?)[\s_(\[-]*(\d+)\.(\d+)(?:\.\d+)?[\s)\]]*$").unwrap())
}

/// `Name (5.0 – 5.8)`, `Name[5.0-5.3]`, `Name 5.0-5.3`: a range of engine versions.
fn range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(.*?)[\s_(\[-]*(\d+)\.(\d+)(?:\.\d+)?\s*[-\u{2010}-\u{2015}~]\s*(?:ue\s*)?(\d+)\.(\d+)(?:\.\d+)?[\s)\]]*$",
        )
        .unwrap()
    })
}

/// Trailing "UE" / "v" token left in front of a version, e.g. "Pack UE" from "Pack UE5.5".
fn ue_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|[\s_(\[-]+)(?:ue|v)$").unwrap())
}

fn plugin_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)[\s_-]*\bplugin\s*$").unwrap())
}

fn archive_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:\.part\d+)?\.(?:7z|rar|zip)$|\.\d{3}$|\.r\d{2}$").unwrap()
    })
}

/// True for real UE versions: 4.0-4.27 and 5.x.
pub fn is_real_ue_version(major: u32, minor: u32) -> bool {
    (major == 4 && minor <= 27) || (major == 5 && minor <= 9)
}

/// Parse the major.minor of an engine version string such as "5.6.0".
pub fn parse_engine_version(s: &str) -> Option<String> {
    let mut it = s.trim().split('.');
    let major: u32 = it.next()?.trim().parse().ok()?;
    let minor: u32 = it.next()?.trim().parse().ok()?;
    if is_real_ue_version(major, minor) {
        Some(format!("{major}.{minor}"))
    } else {
        None
    }
}

/// Result of [`clean_name_ranged`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanName {
    pub display: String,
    /// The single version, or the LOWEST endpoint of a range.
    pub version: Option<String>,
    /// `(min, max)` when the name carries a version range.
    pub range: Option<(String, String)>,
}

/// Remove trailing archive suffixes (`.7z`, `.rar`, `.zip`, `.001`, `.part1.rar`, `.r00`),
/// repeatedly, so `Pack[5.0-5.3].7z.001` becomes `Pack[5.0-5.3]`.
pub fn strip_archive_suffixes(stem: &str) -> String {
    let mut s = stem.trim().to_string();
    while let Some(m) = archive_suffix_re().find(&s) {
        if m.start() == 0 {
            break;
        }
        s.truncate(m.start());
    }
    s
}

fn clean_head(head: &str, stem: &str) -> String {
    let head = ue_prefix_re().replace(head, "");
    let name = head
        .trim_matches(|ch: char| {
            ch.is_whitespace() || ch == '_' || ch == '-' || ch == '(' || ch == '['
        })
        .to_string();
    if name.is_empty() {
        stem.to_string()
    } else {
        name
    }
}

/// Clean an archive file stem into a display name, an optional engine version and an
/// optional version range.
pub fn clean_name_ranged(zip_stem: &str) -> CleanName {
    let stem = strip_archive_suffixes(zip_stem);
    let stem = stem.as_str();
    let mut name = stem.to_string();
    let mut version = None;
    let mut range = None;
    let mut matched = false;
    if let Some(c) = range_re().captures(stem) {
        let p = |i: usize| c[i].parse::<u32>().unwrap_or(u32::MAX);
        let (a, b) = ((p(2), p(3)), (p(4), p(5)));
        if is_real_ue_version(a.0, a.1) && is_real_ue_version(b.0, b.1) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            version = Some(format!("{}.{}", lo.0, lo.1));
            range = Some((format!("{}.{}", lo.0, lo.1), format!("{}.{}", hi.0, hi.1)));
            name = clean_head(&c[1], stem);
            matched = true;
        }
    }
    if !matched {
        if let Some(c) = version_re().captures(stem) {
            let major: u32 = c[2].parse().unwrap_or(u32::MAX);
            let minor: u32 = c[3].parse().unwrap_or(u32::MAX);
            if is_real_ue_version(major, minor) {
                version = Some(format!("{major}.{minor}"));
                name = clean_head(&c[1], stem);
            }
        }
    }
    let stripped = plugin_re().replace(&name, "").trim().to_string();
    if !stripped.is_empty() {
        name = stripped;
    }
    if name.is_empty() {
        name = stem.to_string();
    }
    CleanName {
        display: name,
        version,
        range,
    }
}

/// Clean a zip file stem into a display name and an optional engine version.
///
/// "Advanced Crafting System - Plugin 5.6" -> ("Advanced Crafting System", Some("5.6"));
/// "Sci-Fi Props 2.0" keeps its 2.0 (not a real UE version);
/// "Pack (5.0 – 5.8)" -> ("Pack", Some("5.0")) (lowest endpoint of the range).
pub fn clean_name(zip_stem: &str) -> (String, Option<String>) {
    let c = clean_name_ranged(zip_stem);
    (c.display, c.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_and_archive_names() {
        let r =
            clean_name_ranged("Fabric Material Pack V4 - 20 Pcs 4k Material (5.0 \u{2013} 5.8)");
        assert_eq!(r.display, "Fabric Material Pack V4 - 20 Pcs 4k Material");
        assert_eq!(r.version.as_deref(), Some("5.0"));
        assert_eq!(r.range, Some(("5.0".into(), "5.8".into())));
        let r = clean_name_ranged("Objective Path Guide (5.3 \u{2013} 5.8)");
        assert_eq!(r.display, "Objective Path Guide");
        assert_eq!(r.version.as_deref(), Some("5.3"));
        assert_eq!(r.range, Some(("5.3".into(), "5.8".into())));
        for s in [
            "Post_Apocalyptic_NYC_Environment_Megapack[5.0-5.3]",
            "Post_Apocalyptic_NYC_Environment_Megapack[5.0-5.3].7z",
            "Post_Apocalyptic_NYC_Environment_Megapack[5.0-5.3].7z.001",
        ] {
            let r = clean_name_ranged(s);
            assert_eq!(
                r.display, "Post_Apocalyptic_NYC_Environment_Megapack",
                "{s}"
            );
            assert_eq!(r.version.as_deref(), Some("5.0"));
            assert_eq!(r.range, Some(("5.0".into(), "5.3".into())));
        }
        // Not engine versions: no version, name untouched.
        let r = clean_name_ranged("Fabric Material Pack V4 - 20 Pcs 4k Material");
        assert_eq!(r.display, "Fabric Material Pack V4 - 20 Pcs 4k Material");
        assert_eq!(r.version, None);
        assert_eq!(clean_name("Pack v4.28"), ("Pack v4.28".into(), None));
        assert_eq!(
            clean_name("Camping Animations 5.5.zip"),
            ("Camping Animations".into(), Some("5.5".into()))
        );
        assert_eq!(clean_name("Big Pack.part1.rar"), ("Big Pack".into(), None));
    }

    #[test]
    fn suffix_stripping() {
        assert_eq!(strip_archive_suffixes("a.7z.001"), "a");
        assert_eq!(strip_archive_suffixes("a.zip.002"), "a");
        assert_eq!(strip_archive_suffixes("a.part01.rar"), "a");
        assert_eq!(
            strip_archive_suffixes("Sci-Fi Props 2.0"),
            "Sci-Fi Props 2.0"
        );
    }
}
