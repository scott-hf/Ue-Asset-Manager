//! GTK-free rules for the merged asset library: twin detection, Fab/local matching, version
//! ordering and the filter predicate. Everything here is a pure function so it can be unit tested.
//!
//! Background (diagnosed on a real library cache): Epic's vault lists every legacy marketplace
//! asset that was migrated to Fab twice. The legacy item (namespace `ue`) has the thumbnails,
//! the Fab copy (seller namespace, custom attribute `ListingIdentifier`) has none, which the
//! grid renders as the generic Unreal "U" icon. Both belong to the same asset, so the library
//! shows them as one entry.

use crate::models::project_registry::version_tuple;
use egs_api::api::types::asset_info::AssetInfo;
use egs_api::api::types::fab_library::FabAsset;
use std::cmp::Reverse;
use std::collections::BTreeSet;

/// Where the content of an entry comes from. The order is the tie-break order of the version
/// list: at the same engine version a Fab release is listed before a local library zip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    Fab,
    Local,
}

impl Source {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fab => "Fab",
            Self::Local => "Local",
        }
    }

    /// Stable text stored in the install log.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Fab => "fab",
            Self::Local => "local",
        }
    }
}

/// Comparison key of an asset name: engine version and a trailing "Plugin" removed (same rules as
/// the sanitizer's `clean_name`), then lower case letters and digits only.
pub fn name_key(name: &str) -> String {
    let (clean, _) = ue_asset_library::clean_name(name);
    clean
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The Fab listing id (a UUID) of an Epic library item, when it is a Fab copy.
pub fn epic_listing_id(asset: &AssetInfo) -> Option<String> {
    asset
        .custom_attributes
        .as_ref()
        .and_then(|a| a.get("ListingIdentifier"))
        .map(|a| a.value.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The Fab listing id of a Fab library asset (custom attribute, else the last URL segment).
pub fn fab_listing_id(asset: &FabAsset) -> Option<String> {
    asset
        .custom_attributes
        .iter()
        .find_map(|m| m.get("ListingIdentifier"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            asset
                .url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| s.len() >= 32 && s.contains('-'))
                .map(str::to_string)
        })
}

/// What is needed to decide whether two Epic library items are the same asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemIdentity {
    pub id: String,
    pub title: String,
    /// Set on the Fab copy of an asset.
    pub listing_id: Option<String>,
}

impl ItemIdentity {
    pub fn of(asset: &AssetInfo) -> Self {
        Self {
            id: asset.id.clone(),
            title: asset.title.clone().unwrap_or_default(),
            listing_id: epic_listing_id(asset),
        }
    }
}

/// Two items are twins when they have the same cleaned name and exactly one of them is the Fab
/// copy (has a listing id). Two legacy items with the same title are distinct products and stay
/// separate; so do two Fab copies.
pub fn is_twin(a: &ItemIdentity, b: &ItemIdentity) -> bool {
    if a.id == b.id || a.listing_id.is_some() == b.listing_id.is_some() {
        return false;
    }
    let key = name_key(&a.title);
    !key.is_empty() && key == name_key(&b.title)
}

/// Whether `new` may join an existing group of items. A group is at most one legacy item plus its
/// Fab copy, so only a single-item group accepts a twin. Works for any arrival order.
pub fn can_join(group: &[ItemIdentity], new: &ItemIdentity) -> bool {
    group.len() == 1 && is_twin(&group[0], new)
}

/// Identity keys used to match a local library zip against a Fab/Epic entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchKeys {
    pub listing_ids: Vec<String>,
    pub name_keys: Vec<String>,
}

impl MatchKeys {
    pub fn new(listing_ids: Vec<String>, names: &[&str]) -> Self {
        let mut name_keys: Vec<String> = names
            .iter()
            .map(|n| name_key(n))
            .filter(|k| !k.is_empty())
            .collect();
        name_keys.sort();
        name_keys.dedup();
        let mut listing_ids: Vec<String> =
            listing_ids.into_iter().filter(|l| !l.is_empty()).collect();
        listing_ids.sort();
        listing_ids.dedup();
        Self {
            listing_ids,
            name_keys,
        }
    }
}

/// Same asset? A shared Fab listing id decides. When both sides know listing ids and none is shared
/// they are different assets even if the names agree; otherwise a shared cleaned name matches.
pub fn matches_local(item: &MatchKeys, local: &MatchKeys) -> bool {
    if item
        .listing_ids
        .iter()
        .any(|l| local.listing_ids.contains(l))
    {
        return true;
    }
    if !item.listing_ids.is_empty() && !local.listing_ids.is_empty() {
        return false;
    }
    item.name_keys.iter().any(|k| local.name_keys.contains(k))
}

/// Sort the options of the version dropdown: highest engine version first (unknown engine last),
/// then Fab before Local at the same version. `key` returns the highest (major, minor) engine
/// version of an option and its source. The sort is stable, so the original order (newest
/// release first) is kept inside one source.
pub fn sort_versions<T>(options: &mut [T], key: impl Fn(&T) -> (Option<(u32, u32)>, Source)) {
    options.sort_by_key(|o| {
        let (top_engine, source) = key(o);
        (Reverse(top_engine), source)
    });
}

/// Parse engine texts such as "UE_5.8" or "5.8.1" into (major, minor), dropping GUIDs and junk.
pub fn engine_tuples<'a>(raw: impl IntoIterator<Item = &'a str>) -> Vec<(u32, u32)> {
    raw.into_iter().filter_map(version_tuple).collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        f.to_uppercase().collect::<String>() + c.as_str()
    })
}

/// Display text of a category path: "plugins/tools" becomes "Plugins / Tools".
pub fn category_label(path: &str) -> String {
    path.split('/')
        .filter(|p| !p.is_empty())
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// What a filter pill stands for: a content source or a category path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillKind<'a> {
    Source(Source),
    Category(&'a str),
}

/// Enabled filters of the asset list.
///
/// Semantics: an entry is shown when (Fab is on and it has Fab content, or Local is on and it has
/// local content) AND it matches every selected category. AND across categories narrows the list
/// with each pill; removing a pill widens it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSet {
    pub fab: bool,
    pub local: bool,
    pub categories: BTreeSet<String>,
}

impl Default for FilterSet {
    fn default() -> Self {
        Self {
            fab: true,
            local: true,
            categories: BTreeSet::new(),
        }
    }
}

impl FilterSet {
    pub fn set_source(&mut self, source: Source, on: bool) {
        match source {
            Source::Fab => self.fab = on,
            Source::Local => self.local = on,
        }
    }

    pub fn source_enabled(&self, source: Source) -> bool {
        match source {
            Source::Fab => self.fab,
            Source::Local => self.local,
        }
    }

    pub fn set_category(&mut self, key: &str, on: bool) {
        if on {
            self.categories.insert(key.to_string());
        } else {
            self.categories.remove(key);
        }
    }

    /// Nothing narrows the list (both sources on, no category).
    pub fn is_default(&self) -> bool {
        self.fab && self.local && self.categories.is_empty()
    }

    /// The pills to show, in display order: enabled sources (Fab, Local), then categories.
    pub fn pills(&self) -> Vec<PillKind<'_>> {
        let mut pills = Vec::new();
        if self.fab {
            pills.push(PillKind::Source(Source::Fab));
        }
        if self.local {
            pills.push(PillKind::Source(Source::Local));
        }
        pills.extend(self.categories.iter().map(|c| PillKind::Category(c)));
        pills
    }

    /// The filter predicate. `has_category` answers whether the entry matches one category key.
    pub fn passes(
        &self,
        has_fab: bool,
        has_local: bool,
        has_category: &impl Fn(&str) -> bool,
    ) -> bool {
        let source_ok = (self.fab && has_fab) || (self.local && has_local);
        source_ok && self.categories.iter().all(|c| has_category(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str, listing: Option<&str>) -> ItemIdentity {
        ItemIdentity {
            id: id.to_string(),
            title: title.to_string(),
            listing_id: listing.map(str::to_string),
        }
    }

    /// A version dropdown option as the dialog sees it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct VersionOption {
        source: Source,
        id: String,
        engines: Vec<(u32, u32)>,
    }

    fn sort_options(options: &mut [VersionOption]) {
        sort_versions(options, |o| (o.engines.iter().copied().max(), o.source));
    }

    fn opt(source: Source, id: &str, engines: &[(u32, u32)]) -> VersionOption {
        VersionOption {
            source,
            id: id.to_string(),
            engines: engines.to_vec(),
        }
    }

    #[test]
    fn name_key_ignores_case_punctuation_version_and_plugin_suffix() {
        assert_eq!(name_key("ProInstance Tools Plugin"), "proinstancetools");
        assert_eq!(name_key("ProInstance Tools"), "proinstancetools");
        assert_eq!(name_key("Camping Animations 5.5"), "campinganimations");
        assert_eq!(name_key("Sci-Fi / Futuristic!"), "scififuturistic");
        assert_eq!(name_key("***"), "");
    }

    #[test]
    fn legacy_item_and_fab_copy_are_twins() {
        let legacy = item("a", "Procedural Rooms", None);
        let fab = item("b", "Procedural Rooms", Some("uuid-1"));
        assert!(is_twin(&legacy, &fab));
        assert!(is_twin(&fab, &legacy));
        // the plugin suffix does not matter
        assert!(is_twin(
            &item("a", "ProInstance Tools Plugin", None),
            &item("b", "ProInstance Tools", Some("u"))
        ));
    }

    #[test]
    fn same_kind_or_other_name_is_not_a_twin() {
        let legacy = item("a", "Starter Pack", None);
        // two legacy items with one title are distinct products
        assert!(!is_twin(&legacy, &item("b", "Starter Pack", None)));
        // two Fab copies too
        assert!(!is_twin(
            &item("a", "X", Some("1")),
            &item("b", "X", Some("2"))
        ));
        // different titles
        assert!(!is_twin(&legacy, &item("b", "Other Pack", Some("u"))));
        // the same item is never its own twin
        assert!(!is_twin(&legacy, &item("a", "Starter Pack", Some("u"))));
        // unnameable titles never merge
        assert!(!is_twin(
            &item("a", "!!!", None),
            &item("b", "???", Some("u"))
        ));
    }

    #[test]
    fn a_group_takes_exactly_one_twin_in_any_arrival_order() {
        let legacy = item("a", "Rooms", None);
        let fab = item("b", "Rooms", Some("u"));
        let fab2 = item("c", "Rooms", Some("u2"));
        // legacy first, then the Fab copy
        assert!(can_join(std::slice::from_ref(&legacy), &fab));
        // Fab copy first, then the legacy item
        assert!(can_join(std::slice::from_ref(&fab), &legacy));
        // a full group takes nobody; a second Fab copy cannot join the legacy item's pair
        assert!(!can_join(&[legacy.clone(), fab.clone()], &fab2));
        assert!(!can_join(&[], &fab));
        // ... but it could pair with a lonely legacy item
        assert!(can_join(&[legacy], &fab2));
    }

    #[test]
    fn dedupe_collapses_a_listing_of_pairs_to_one_entry_each() {
        // Simulates the library list: entries are groups; every incoming item joins a matching
        // group or opens a new one (what `add_asset` does).
        let incoming = vec![
            item("1", "Procedural Rooms", None),
            item("2", "Propagation System", Some("u2")),
            item("3", "Procedural Rooms", Some("u1")),
            item("4", "Propagation System", None),
            item("5", "Solo Plugin", Some("u5")),
            item("6", "Unreal Engine", None),
            item("7", "Unreal Engine", None),
        ];
        let mut groups: Vec<Vec<ItemIdentity>> = Vec::new();
        for it in incoming {
            match groups.iter_mut().find(|g| can_join(g, &it)) {
                Some(g) => g.push(it),
                None => groups.push(vec![it]),
            }
        }
        let sizes: Vec<usize> = groups.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![2, 2, 1, 1, 1]);
        assert_eq!(groups[0][0].id, "1");
        assert_eq!(groups[0][1].id, "3");
    }

    fn keys(listing: &[&str], names: &[&str]) -> MatchKeys {
        MatchKeys::new(listing.iter().map(|s| s.to_string()).collect(), names)
    }

    #[test]
    fn local_matches_by_listing_id_even_with_other_names() {
        assert!(matches_local(
            &keys(&["L1"], &["Foo"]),
            &keys(&["L1"], &["Totally Different"])
        ));
    }

    #[test]
    fn local_matches_by_normalized_name_when_a_side_has_no_listing() {
        assert!(matches_local(
            &keys(&["L1"], &["Camping Animations"]),
            &keys(&[], &["camping animations 5.5"])
        ));
        assert!(matches_local(
            &keys(&[], &["Procedural Rooms"]),
            &keys(&["L9"], &["Procedural Rooms"])
        ));
        assert!(!matches_local(&keys(&[], &["Foo"]), &keys(&[], &["Bar"])));
        // no usable names, no listing: no match
        assert!(!matches_local(&keys(&[], &["!!!"]), &keys(&[], &["???"])));
    }

    #[test]
    fn differing_listing_ids_beat_a_name_match() {
        assert!(!matches_local(
            &keys(&["L1"], &["Same Name"]),
            &keys(&["L2"], &["Same Name"])
        ));
    }

    #[test]
    fn newest_engine_first() {
        let mut v = vec![
            opt(Source::Fab, "5.3", &[(5, 3)]),
            opt(Source::Local, "5.8", &[(5, 8)]),
            opt(Source::Fab, "5.10", &[(5, 10)]),
            opt(Source::Fab, "unknown", &[]),
            opt(Source::Fab, "5.5", &[(5, 4), (5, 5)]),
        ];
        sort_options(&mut v);
        let ids: Vec<&str> = v.iter().map(|o| o.id.as_str()).collect();
        // 5.10 is newer than 5.8 (numeric, not text, order); unknown engines go last
        assert_eq!(ids, vec!["5.10", "5.8", "5.5", "5.3", "unknown"]);
    }

    #[test]
    fn fab_is_listed_before_local_at_the_same_engine_version() {
        let mut v = vec![
            opt(Source::Local, "local-5.8", &[(5, 8)]),
            opt(Source::Fab, "fab-5.8", &[(5, 8)]),
            opt(Source::Local, "local-5.7", &[(5, 7)]),
            opt(Source::Fab, "fab-5.7", &[(5, 7)]),
        ];
        sort_options(&mut v);
        let ids: Vec<&str> = v.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["fab-5.8", "local-5.8", "fab-5.7", "local-5.7"]);
        assert_eq!(
            v[0].source,
            Source::Fab,
            "the Fab entry is the default selection"
        );
    }

    #[test]
    fn sort_keeps_the_original_order_inside_one_source() {
        let mut v = vec![
            opt(Source::Fab, "first", &[(5, 4)]),
            opt(Source::Fab, "second", &[(5, 4)]),
        ];
        sort_options(&mut v);
        assert_eq!(v[0].id, "first");
    }

    #[test]
    fn engine_rule_picks_fab_on_a_tie_and_never_goes_down_in_version() {
        use crate::models::project_registry::{choose_release, ReleaseChoice};
        let mut v = vec![
            opt(Source::Local, "local-5.8", &[(5, 8)]),
            opt(Source::Fab, "fab-5.8", &[(5, 8)]),
            opt(Source::Fab, "fab-5.6", &[(5, 6)]),
            opt(Source::Local, "local-5.9", &[(5, 9)]),
        ];
        sort_options(&mut v);
        let releases: Vec<Vec<(u32, u32)>> = v.iter().map(|o| o.engines.clone()).collect();
        // project on 5.8: exact match, and the Fab entry wins the tie
        let c = choose_release((5, 8), &releases).unwrap();
        assert!(matches!(c, ReleaseChoice::Exact(_)));
        assert_eq!(v[c.index()].id, "fab-5.8");
        // project on 5.7: the highest release not above it
        let c = choose_release((5, 7), &releases).unwrap();
        assert_eq!(v[c.index()].id, "fab-5.6");
        // project on 5.2: everything is newer, the lowest is taken
        let c = choose_release((5, 2), &releases).unwrap();
        assert!(matches!(c, ReleaseChoice::AllNewer(_)));
        assert_eq!(v[c.index()].id, "fab-5.6");
    }

    #[test]
    fn engine_tuples_drop_unparsable_texts() {
        assert_eq!(
            engine_tuples(["UE_5.8", "5.3.2", "{GUID}"]),
            vec![(5, 8), (5, 3)]
        );
    }

    #[test]
    fn default_filters_enable_both_sources_and_show_everything() {
        let f = FilterSet::default();
        assert!(f.is_default());
        let any = |_: &str| false;
        assert!(f.passes(true, false, &any));
        assert!(f.passes(false, true, &any));
        assert!(f.passes(true, true, &any));
    }

    #[test]
    fn source_filters_limit_to_fab_or_local() {
        let any = |_: &str| true;
        let mut f = FilterSet::default();
        f.set_source(Source::Local, false);
        assert!(f.passes(true, false, &any), "fab only entry");
        assert!(
            f.passes(true, true, &any),
            "merged entry still has Fab content"
        );
        assert!(!f.passes(false, true, &any), "local only entry is hidden");
        f.set_source(Source::Local, true);
        f.set_source(Source::Fab, false);
        assert!(!f.passes(true, false, &any));
        assert!(f.passes(false, true, &any));
        f.set_source(Source::Local, false);
        assert!(
            !f.passes(true, true, &any),
            "no source enabled shows nothing"
        );
    }

    #[test]
    fn categories_are_anded() {
        let mut f = FilterSet::default();
        f.set_category("plugins", true);
        f.set_category("tools", true);
        assert!(!f.is_default());
        let has = |c: &str| matches!(c, "plugins" | "tools" | "other");
        assert!(f.passes(true, false, &has));
        let only_plugins = |c: &str| c == "plugins";
        assert!(!f.passes(true, false, &only_plugins));
        f.set_category("tools", false);
        assert!(f.passes(true, false, &only_plugins));
    }

    #[test]
    fn pills_list_sources_first_then_sorted_categories() {
        let mut f = FilterSet::default();
        f.set_category("zeta", true);
        f.set_category("alpha", true);
        assert_eq!(
            f.pills(),
            vec![
                PillKind::Source(Source::Fab),
                PillKind::Source(Source::Local),
                PillKind::Category("alpha"),
                PillKind::Category("zeta"),
            ]
        );
        f.set_source(Source::Fab, false);
        assert_eq!(f.pills()[0], PillKind::Source(Source::Local));
        assert!(!f.source_enabled(Source::Fab));
    }

    #[test]
    fn category_labels_are_capitalized_per_part() {
        assert_eq!(category_label("plugins"), "Plugins");
        assert_eq!(category_label("plugins/tools"), "Plugins / Tools");
        assert_eq!(category_label(""), "");
    }

    #[test]
    fn listing_ids_are_read_from_epic_and_fab_items() {
        let mut epic = AssetInfo::default();
        assert_eq!(epic_listing_id(&epic), None);
        let mut attrs = std::collections::HashMap::new();
        attrs.insert(
            "ListingIdentifier".to_string(),
            egs_api::api::types::asset_info::CustomAttribute {
                type_field: "STRING".to_string(),
                value: "68d92f15-d5c0-4f9b-8c09-64f3065030d6".to_string(),
            },
        );
        epic.custom_attributes = Some(attrs);
        assert_eq!(
            epic_listing_id(&epic).as_deref(),
            Some("68d92f15-d5c0-4f9b-8c09-64f3065030d6")
        );

        let fab = FabAsset {
            url: "https://www.fab.com/listings/3651c66b-363c-4508-84ab-37df07e08c09".to_string(),
            ..Default::default()
        };
        assert_eq!(
            fab_listing_id(&fab).as_deref(),
            Some("3651c66b-363c-4508-84ab-37df07e08c09")
        );
        assert_eq!(fab_listing_id(&FabAsset::default()), None);
    }
}
