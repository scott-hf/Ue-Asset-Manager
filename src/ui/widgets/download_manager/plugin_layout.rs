//! Mapping of Epic plugin manifests onto a project's `Plugins/` folder.
//!
//! Marketplace plugins are usually laid out for an engine install
//! (`Engine/Plugins/Marketplace/<ReleaseId>/...`).  When such an asset is copied into a project the
//! files have to end up in `<project>/Plugins/<PluginName>/...` and nowhere else.

/// A plugin folder found in a manifest: the folder prefix (with a trailing `/`) and the plugin name.
pub type PluginRoot = (String, String);

const LAYOUT_PREFIXES: [&str; 3] = ["Engine/Plugins/Marketplace/", "Engine/Plugins/", "Plugins/"];

/// Split a manifest path into the plugin folder prefix and the path inside that folder.
fn split_root(file: &str) -> Option<(String, String, String)> {
    let normalized = file.replace('\\', "/");
    let normalized = normalized.trim_start_matches('/');
    for prefix in LAYOUT_PREFIXES {
        if let Some(rest) = normalized.strip_prefix(prefix) {
            let (folder, inner) = rest.split_once('/')?;
            if folder.is_empty() || inner.is_empty() {
                return None;
            }
            return Some((
                format!("{prefix}{folder}/"),
                folder.to_string(),
                inner.to_string(),
            ));
        }
    }
    None
}

/// Strip an obvious trailing release suffix (`<12 hex digits>V<number>`) from a folder name.
fn strip_release_suffix(folder: &str) -> Option<String> {
    let v = folder.rfind('V')?;
    let (head, digits) = (&folder[..v], &folder[v + 1..]);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) || head.len() <= 12 {
        return None;
    }
    let (name, hash) = head.split_at(head.len() - 12);
    let is_hash = hash
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    (is_hash && !name.is_empty()).then(|| name.to_string())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', ':']) && name != "." && name != ".."
}

/// Find the plugin folders in a manifest and decide the project folder name of each.
///
/// The name is the stem of the `.uplugin` at the folder's root.  Without one, a folder that is not
/// below `Marketplace` keeps its own name and a Marketplace folder loses an obvious release suffix.
/// Anything else (or several `.uplugin` in one folder) is an error rather than a guess.
pub fn plugin_roots(files: &[String]) -> Result<Vec<PluginRoot>, String> {
    // prefix -> (folder name, uplugin stems at the root)
    let mut roots: Vec<(String, String, Vec<String>)> = Vec::new();
    for file in files {
        let Some((prefix, folder, inner)) = split_root(file) else {
            continue;
        };
        let idx = match roots.iter().position(|r| r.0 == prefix) {
            Some(i) => i,
            None => {
                roots.push((prefix, folder, Vec::new()));
                roots.len() - 1
            }
        };
        if !inner.contains('/') {
            if let Some(stem) = inner.strip_suffix(".uplugin") {
                roots[idx].2.push(stem.to_string());
            }
        }
    }
    let mut result = Vec::new();
    for (prefix, folder, mut stems) in roots {
        stems.sort();
        stems.dedup();
        let name = match stems.len() {
            1 => Some(stems.remove(0)),
            0 if prefix.starts_with("Engine/Plugins/Marketplace/") => strip_release_suffix(&folder),
            0 => Some(folder.clone()),
            _ => None,
        };
        match name.filter(|n| valid_name(n)) {
            Some(name) => result.push((prefix, name)),
            None => {
                return Err(format!(
                    "Cannot determine the plugin name for '{}' ({} .uplugin files)",
                    prefix.trim_end_matches('/'),
                    stems.len()
                ));
            }
        }
    }
    Ok(result)
}

/// Map a manifest file to its path below the project directory: `Plugins/<PluginName>/<path>`.
/// Files outside every plugin folder (or that escape it) map to `None` and are not copied.
pub fn map_plugin_path(roots: &[PluginRoot], file: &str) -> Option<String> {
    let (prefix, _, inner) = split_root(file)?;
    if inner.split('/').any(|c| c == ".." || c.is_empty()) {
        return None;
    }
    let (_, name) = roots.iter().find(|(p, _)| *p == prefix)?;
    Some(format!("Plugins/{name}/{inner}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn marketplace_uses_uplugin_stem() {
        let f = files(&[
            "Engine/Plugins/Marketplace/UIEffectaaaff14a0cffV9/ExtendBlueprintNode.uplugin",
            "Engine/Plugins/Marketplace/UIEffectaaaff14a0cffV9/Source/ExtendBlueprintNode/A.cpp",
            "Engine/Plugins/Marketplace/UIEffectaaaff14a0cffV9/Resources/Icon128.png",
        ]);
        let roots = plugin_roots(&f).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].1, "ExtendBlueprintNode");
        assert_eq!(
            map_plugin_path(&roots, &f[1]).as_deref(),
            Some("Plugins/ExtendBlueprintNode/Source/ExtendBlueprintNode/A.cpp")
        );
        assert_eq!(
            map_plugin_path(&roots, &f[0]).as_deref(),
            Some("Plugins/ExtendBlueprintNode/ExtendBlueprintNode.uplugin")
        );
    }

    #[test]
    fn plain_layouts_map_the_same_way() {
        let f = files(&[
            "Plugins/Foo/Foo.uplugin",
            "Engine/Plugins/Bar/Content/x.uasset",
        ]);
        let roots = plugin_roots(&f).unwrap();
        assert_eq!(
            map_plugin_path(&roots, &f[0]).as_deref(),
            Some("Plugins/Foo/Foo.uplugin")
        );
        // no .uplugin: a non-Marketplace folder keeps its name
        assert_eq!(
            map_plugin_path(&roots, &f[1]).as_deref(),
            Some("Plugins/Bar/Content/x.uasset")
        );
    }

    #[test]
    fn several_plugins_are_mapped_per_folder() {
        let f = files(&[
            "Engine/Plugins/Marketplace/AAA/A.uplugin",
            "Engine/Plugins/Marketplace/AAA/Content/a.uasset",
            "Engine/Plugins/Marketplace/BBB/Source/B.uplugin",
            "Engine/Plugins/Marketplace/BBB/B2.uplugin",
        ]);
        let roots = plugin_roots(&f).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(
            map_plugin_path(&roots, &f[1]).as_deref(),
            Some("Plugins/A/Content/a.uasset")
        );
        assert_eq!(
            map_plugin_path(&roots, &f[3]).as_deref(),
            Some("Plugins/B2/B2.uplugin")
        );
    }

    #[test]
    fn release_suffix_is_stripped_only_when_obvious() {
        let f = files(&["Engine/Plugins/Marketplace/UIEffectaaaff14a0cffV9/Content/x.uasset"]);
        let roots = plugin_roots(&f).unwrap();
        assert_eq!(roots[0].1, "UIEffect");
        let f = files(&["Engine/Plugins/Marketplace/SomethingOdd/Content/x.uasset"]);
        assert!(plugin_roots(&f).is_err());
    }

    #[test]
    fn ambiguous_uplugins_abort() {
        let f = files(&[
            "Engine/Plugins/Marketplace/AAA/A.uplugin",
            "Engine/Plugins/Marketplace/AAA/B.uplugin",
        ]);
        assert!(plugin_roots(&f).is_err());
    }

    #[test]
    fn never_leaves_the_plugin_folder() {
        let f = files(&["Plugins/Foo/Foo.uplugin"]);
        let roots = plugin_roots(&f).unwrap();
        assert_eq!(map_plugin_path(&roots, "Config/DefaultEngine.ini"), None);
        assert_eq!(map_plugin_path(&roots, "Content/Foo.uasset"), None);
        assert_eq!(map_plugin_path(&roots, "Plugins/Foo/../../x.txt"), None);
        assert_eq!(map_plugin_path(&roots, "Plugins/Foo/a//b"), None);
        assert_eq!(map_plugin_path(&roots, "Plugins/Other/a.txt"), None);
        assert_eq!(
            map_plugin_path(&roots, "Plugins\\Foo\\Source\\a.cpp").as_deref(),
            Some("Plugins/Foo/Source/a.cpp")
        );
    }
}
