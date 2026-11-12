//! Display-name cleanup for dropped zip file names.

use regex::Regex;
use std::sync::OnceLock;

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.*?)[\s_(\[-]*(\d+)\.(\d+)(?:\.\d+)?[\s)\]]*$").unwrap())
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

/// Clean a zip file stem into a display name and an optional engine version.
///
/// "Advanced Crafting System - Plugin 5.6" -> ("Advanced Crafting System", Some("5.6"));
/// "Sci-Fi Props 2.0" keeps its 2.0 (not a real UE version).
pub fn clean_name(zip_stem: &str) -> (String, Option<String>) {
    let stem = zip_stem.trim();
    let mut name = stem.to_string();
    let mut version = None;
    if let Some(c) = version_re().captures(stem) {
        let major: u32 = c[2].parse().unwrap_or(u32::MAX);
        let minor: u32 = c[3].parse().unwrap_or(u32::MAX);
        if is_real_ue_version(major, minor) {
            version = Some(format!("{major}.{minor}"));
            let head = c[1].to_string();
            let head = ue_prefix_re().replace(&head, "");
            name = head
                .trim_matches(|ch: char| {
                    ch.is_whitespace() || ch == '_' || ch == '-' || ch == '(' || ch == '['
                })
                .to_string();
            if name.is_empty() {
                name = stem.to_string();
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
    (name, version)
}
