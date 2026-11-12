use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use ue_asset_library::harden::{check_paths, normalize_name, EntryInfo};
use ue_asset_library::pipeline::{reconcile, run_once, try_claim, ClaimResult, Config, Dirs};
use ue_asset_library::{
    clean_name, plan_layout, preview_import, safe_import, safe_import_new_project, sanitize_zip,
    scan_library, ImportError, ImportPolicy, Kind, NeedsReview, SanitizeError, SanitizeOutcome,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

static N: AtomicU64 = AtomicU64::new(0);

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "ue_asset_library_tests/{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn opts() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
}

/// Fake `.uasset`: header junk plus NUL-terminated name-table strings.
fn fake_asset(own: &str, refs: &[&str]) -> Vec<u8> {
    let mut v = vec![0xC1, 0x83, 0x2A, 0x9E, 0, 0, 0, 0, 7, 7];
    for s in std::iter::once(&own).chain(refs.iter()) {
        v.extend_from_slice(&((s.len() + 1) as u32).to_le_bytes());
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        v.extend_from_slice(&[0x12, 0x34]);
    }
    v
}

fn make_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let mut w = ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in entries {
        if name.ends_with('/') {
            w.add_directory(name.to_string(), opts()).unwrap();
        } else {
            w.start_file(name.to_string(), opts()).unwrap();
            w.write_all(data).unwrap();
        }
    }
    w.finish().unwrap();
}

fn names_of(path: &Path) -> Vec<String> {
    let mut z = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let mut v: Vec<String> = (0..z.len())
        .map(|i| z.by_index_raw(i).unwrap().name().to_string())
        .collect();
    v.sort();
    v
}

fn sanitized(src: &Path, lib: &Path) -> (PathBuf, ue_asset_library::Sidecar) {
    match sanitize_zip(src, lib).expect("sanitize") {
        SanitizeOutcome::Sanitized {
            library_zip,
            sidecar,
            ..
        } => (library_zip, sidecar),
        other => panic!("unexpected {other:?}"),
    }
}

fn needs_review(src: &Path, lib: &Path) -> NeedsReview {
    match sanitize_zip(src, lib) {
        Err(SanitizeError::NeedsReview(n)) => n,
        other => panic!("expected needs-review, got {other:?}"),
    }
}

fn op_room_zip(path: &Path) {
    make_zip(
        path,
        &[
            ("OperatingRoomPack/", vec![]),
            (
                "OperatingRoomPack/Levels/Operating_Room.umap",
                fake_asset(
                    "/Game/OperatingRoom/Levels/Operating_Room",
                    &["/Game/Characters/Mannequins/Manny"],
                ),
            ),
            (
                "OperatingRoomPack/Materials/M_Master.uasset",
                fake_asset("/Game/OperatingRoom/Materials/M_Master", &[]),
            ),
            ("OperatingRoomPack/Materials/M_Master.uexp", vec![1, 2, 3]),
            ("OperatingRoomPack/readme.txt", b"hi".to_vec()),
            ("__MACOSX/OperatingRoomPack/._x", vec![0]),
        ],
    );
}

// ---------- clean_name ----------

#[test]
fn clean_name_cases() {
    assert_eq!(
        clean_name("Advanced Crafting System - Plugin 5.6"),
        ("Advanced Crafting System".into(), Some("5.6".into()))
    );
    assert_eq!(
        clean_name("Camping Animations 5.5"),
        ("Camping Animations".into(), Some("5.5".into()))
    );
    assert_eq!(
        clean_name("Operating Room Pack 5.0"),
        ("Operating Room Pack".into(), Some("5.0".into()))
    );
    assert_eq!(
        clean_name("Sci-Fi Props 2.0"),
        ("Sci-Fi Props 2.0".into(), None)
    );
    assert_eq!(
        clean_name("Blue Pack UE5.4"),
        ("Blue Pack".into(), Some("5.4".into()))
    );
    assert_eq!(clean_name("Blue 5.5"), ("Blue".into(), Some("5.5".into())));
    assert_eq!(
        clean_name("Old Pack (4.27)"),
        ("Old Pack".into(), Some("4.27".into()))
    );
    assert_eq!(clean_name("Pack v4.28"), ("Pack v4.28".into(), None));
    assert_eq!(clean_name("Plain"), ("Plain".into(), None));
}

// ---------- layout ----------

#[test]
fn content_rule_wrapper_renamed_to_game_folder() {
    let t = Tmp::new();
    let src = t.path().join("Operating Room Pack 5.0.zip");
    op_room_zip(&src);
    let lib = t.dir("library");
    let (zip, sc) = sanitized(&src, &lib);
    assert_eq!(sc.kind, Kind::Content);
    assert_eq!(sc.display_name, "Operating Room Pack");
    assert_eq!(sc.engine_version_guess.as_deref(), Some("5.0"));
    assert_eq!(sc.content_folder.as_deref(), Some("OperatingRoom"));
    assert_eq!(sc.zip_prefix, "OperatingRoomPack");
    assert_eq!(sc.game_prefix, "OperatingRoom");
    assert_eq!(sc.allowed_roots, vec!["Content/OperatingRoom".to_string()]);
    assert_eq!(sc.entry_count, 4);
    assert_eq!(
        sc.external_refs,
        vec!["/Game/Characters/Mannequins/Manny".to_string()]
    );
    assert_eq!(
        names_of(&zip),
        vec![
            "Content/OperatingRoom/Levels/Operating_Room.umap",
            "Content/OperatingRoom/Materials/M_Master.uasset",
            "Content/OperatingRoom/Materials/M_Master.uexp",
            "Content/OperatingRoom/readme.txt"
        ]
    );
    let name = zip.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.starts_with("operating-room-pack.") && name.ends_with(".zip"),
        "{name}"
    );
    let entries = scan_library(&lib);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].sidecar.sha256[..12],
        name[name.len() - 16..name.len() - 4]
    );
}

#[test]
fn content_wrapper_already_game_folder_and_nested_wrapper() {
    let t = Tmp::new();
    let lib = t.dir("library");
    let a = t.path().join("Camping 5.5.zip");
    make_zip(
        &a,
        &[
            (
                "CampingAnimations/Animations/AS_A.uasset",
                fake_asset("/Game/CampingAnimations/Animations/AS_A", &[]),
            ),
            (
                "CampingAnimations/Animations/AS_B.uasset",
                fake_asset("/Game/CampingAnimations/Animations/AS_B", &[]),
            ),
        ],
    );
    let (zip, sc) = sanitized(&a, &lib);
    assert_eq!(sc.zip_prefix, "");
    assert_eq!(sc.game_prefix, "");
    assert_eq!(
        names_of(&zip)[0],
        "Content/CampingAnimations/Animations/AS_A.uasset"
    );

    let b = t.path().join("Nested.zip");
    make_zip(
        &b,
        &[
            (
                "Outer/Content/Foo/Meshes/M.uasset",
                fake_asset("/Game/Foo/Meshes/M", &[]),
            ),
            ("Outer/Config/DefaultEngine.ini", b"x".to_vec()),
        ],
    );
    let (zip, sc) = sanitized(&b, &lib);
    assert_eq!(sc.zip_prefix, "Outer/Content");
    assert_eq!(names_of(&zip), vec!["Content/Foo/Meshes/M.uasset"]);
}

#[test]
fn multi_root_is_needs_review() {
    let t = Tmp::new();
    let src = t.path().join("m.zip");
    make_zip(
        &src,
        &[
            ("A/x.uasset", fake_asset("/Game/A/x", &[])),
            ("B/y.uasset", fake_asset("/Game/B/y", &[])),
        ],
    );
    match needs_review(&src, &t.dir("lib")) {
        NeedsReview::MultiRoot(r) => assert_eq!(r.len(), 2),
        o => panic!("{o:?}"),
    }
}

#[test]
fn no_game_match_is_needs_review() {
    let t = Tmp::new();
    let src = t.path().join("n.zip");
    make_zip(
        &src,
        &[
            ("Pack/x.uasset", vec![1, 2, 3, 4]),
            ("Pack/y.uasset", vec![9, 9]),
        ],
    );
    assert_eq!(needs_review(&src, &t.dir("lib")), NeedsReview::NoGameMatch);
    let src2 = t.path().join("n2.zip");
    make_zip(&src2, &[("Pack/readme.txt", vec![1])]);
    assert_eq!(needs_review(&src2, &t.dir("lib")), NeedsReview::NoGameMatch);
}

#[test]
fn rule_disagreement_is_needs_review() {
    let t = Tmp::new();
    let src = t.path().join("d.zip");
    make_zip(
        &src,
        &[
            ("P/a/x.uasset", fake_asset("/Game/Q/a/x", &[])),
            ("R/b/y.uasset", fake_asset("/Game/b/y", &[])),
        ],
    );
    assert!(matches!(
        needs_review(&src, &t.dir("lib")),
        NeedsReview::RuleDisagree(_)
    ));
}

#[test]
fn utf16_game_paths_are_found() {
    let t = Tmp::new();
    let src = t.path().join("u.zip");
    let mut data = vec![0u8; 8];
    for c in "/Game/Wide/Dir/Z".encode_utf16() {
        data.extend_from_slice(&c.to_le_bytes());
    }
    data.extend_from_slice(&[0, 0]);
    make_zip(&src, &[("W/Dir/Z.uasset", data)]);
    let (_, sc) = sanitized(&src, &t.dir("lib"));
    assert_eq!(sc.content_folder.as_deref(), Some("Wide"));
}

#[test]
fn project_zip_is_tagged_and_new_project_import_works() {
    let t = Tmp::new();
    let src = t.path().join("Cool Project 5.3.zip");
    make_zip(
        &src,
        &[
            ("Cool/Cool.uproject", b"{}".to_vec()),
            ("Cool/Content/A.uasset", vec![1, 2, 3]),
            ("Cool/Source/Cool.Target.cs", b"x".to_vec()),
        ],
    );
    let lib = t.dir("lib");
    let (_, sc) = sanitized(&src, &lib);
    assert_eq!(sc.kind, Kind::Project);
    let entry = scan_library(&lib).remove(0);
    // Not importable into an existing project.
    let proj = t.dir("proj");
    fs::write(proj.join("P.uproject"), "{}").unwrap();
    assert!(matches!(
        safe_import(&entry, &proj, ImportPolicy::SkipExisting),
        Err(ImportError::WrongKind(Kind::Project))
    ));
    assert!(matches!(
        preview_import(&entry, &proj),
        Err(ImportError::WrongKind(_))
    ));
    let parent = t.dir("parent");
    let dir = safe_import_new_project(&entry, &parent, "CoolCopy").unwrap();
    assert!(dir.join("Cool.uproject").is_file());
    assert!(dir.join("Content/A.uasset").is_file());
    assert!(dir.join("Source/Cool.Target.cs").is_file());
    // Existing target is refused; bad names are refused; no temp folders left behind.
    assert!(matches!(
        safe_import_new_project(&entry, &parent, "CoolCopy"),
        Err(ImportError::TargetExists(_))
    ));
    assert!(matches!(
        safe_import_new_project(&entry, &parent, "../evil"),
        Err(ImportError::InvalidName(_))
    ));
    assert!(matches!(
        safe_import_new_project(&entry, &parent, "NUL"),
        Err(ImportError::InvalidName(_))
    ));
    let left: Vec<_> = fs::read_dir(&parent)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(left, vec!["CoolCopy"]);
}

#[test]
fn plugin_layout_drops_build_output_and_reads_uplugin() {
    let t = Tmp::new();
    let src = t.path().join("Advanced Crafting System - Plugin 5.6.zip");
    make_zip(
        &src,
        &[
            ("Advancedd3e/SIZZ_Adv.uplugin", br#"{"FriendlyName":"Adv Craft","EngineVersion":"5.6.0","MarketplaceURL":"https://x"}"#.to_vec()),
            ("Advancedd3e/Binaries/Win64/a.dll", vec![1]),
            ("Advancedd3e/Intermediate/Build/b.obj", vec![1]),
            ("Advancedd3e/Content/C.uasset", fake_asset("/SIZZ_Adv/C", &[])),
            ("Advancedd3e/Source/SIZZ_Adv/SIZZ_Adv.Build.cs", b"x".to_vec()),
            ("Advancedd3e/Source/ThirdParty/Binaries/Win64/keep.dll", vec![2]),
        ],
    );
    let (zip, sc) = sanitized(&src, &t.dir("lib"));
    assert_eq!(sc.kind, Kind::Plugin);
    assert_eq!(sc.plugin_name.as_deref(), Some("SIZZ_Adv"));
    assert_eq!(sc.engine_version_guess.as_deref(), Some("5.6"));
    assert_eq!(sc.friendly_name.as_deref(), Some("Adv Craft"));
    assert_eq!(sc.display_name, "Advanced Crafting System");
    assert_eq!(sc.allowed_roots, vec!["Plugins/SIZZ_Adv".to_string()]);
    assert_eq!(
        names_of(&zip),
        vec![
            "Plugins/SIZZ_Adv/Content/C.uasset",
            "Plugins/SIZZ_Adv/SIZZ_Adv.uplugin",
            "Plugins/SIZZ_Adv/Source/SIZZ_Adv/SIZZ_Adv.Build.cs",
            "Plugins/SIZZ_Adv/Source/ThirdParty/Binaries/Win64/keep.dll",
        ]
    );
}

#[test]
fn two_uplugins() {
    let t = Tmp::new();
    let sib = t.path().join("sib.zip");
    make_zip(
        &sib,
        &[
            ("A/A.uplugin", b"{}".to_vec()),
            ("B/B.uplugin", b"{}".to_vec()),
        ],
    );
    assert!(matches!(
        needs_review(&sib, &t.dir("lib")),
        NeedsReview::Ambiguous(_)
    ));
    // A sub-plugin inside the main plugin's tree is fine: the outer one wins.
    let nested = t.path().join("nested.zip");
    make_zip(
        &nested,
        &[
            ("A/A.uplugin", b"{}".to_vec()),
            ("A/Plugins/Sub/Sub.uplugin", b"{}".to_vec()),
        ],
    );
    let (_, sc) = sanitized(&nested, &t.dir("lib2"));
    assert_eq!(sc.plugin_name.as_deref(), Some("A"));
}

// ---------- hardening ----------

fn single_entry_zip(t: &Tmp, name: &str) -> PathBuf {
    let p = t.path().join("h.zip");
    make_zip(&p, &[(name, fake_asset("/Game/H/x", &[]))]);
    p
}

#[test]
fn rejects_slip_absolute_and_odd_names() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    for (name, code) in [
        ("../evil.uasset", "SLIP"),
        ("ok/../../evil.uasset", "SLIP"),
        ("/abs/evil.uasset", "ABSOLUTE"),
        ("C:/evil.uasset", "DRIVE_OR_STREAM"),
        ("ok/CON.uasset", "RESERVED_NAME"),
        ("ok/trailing./x.uasset", "TRAILING_DOT_SPACE"),
        ("ok/a?b.uasset", "INVALID_CHAR"),
    ] {
        let src = single_entry_zip(&t, name);
        match needs_review(&src, &lib) {
            NeedsReview::Unsafe { code: c, .. } => assert_eq!(c, code, "{name}"),
            o => panic!("{name}: {o:?}"),
        }
    }
    assert_eq!(
        fs::read_dir(&lib).unwrap().count(),
        0,
        "nothing may reach the library"
    );
}

#[test]
fn rejects_backslash_slip_and_long_paths() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let src = single_entry_zip(&t, "ok\\..\\..\\evil.uasset");
    assert!(matches!(
        needs_review(&src, &lib),
        NeedsReview::Unsafe { .. }
    ));
    let long = format!("P/{}/x.uasset", "d".repeat(210));
    let src = single_entry_zip(&t, &long);
    match needs_review(&src, &lib) {
        NeedsReview::Unsafe { code, .. } => assert_eq!(code, "PATH_TOO_LONG"),
        o => panic!("{o:?}"),
    }
}

#[test]
fn rejects_symlink_entry() {
    let t = Tmp::new();
    let src = t.path().join("s.zip");
    let mut w = ZipWriter::new(fs::File::create(&src).unwrap());
    w.start_file("P/x.uasset", opts()).unwrap();
    w.write_all(&fake_asset("/Game/P/x", &[])).unwrap();
    w.add_symlink("P/link.uasset", "/etc/passwd", opts())
        .unwrap();
    w.finish().unwrap();
    match needs_review(&src, &t.dir("lib")) {
        NeedsReview::Unsafe { code, .. } => assert_eq!(code, "SYMLINK"),
        o => panic!("{o:?}"),
    }
}

#[test]
fn rejects_case_collision_in_zip_and_duplicates() {
    let t = Tmp::new();
    let src = t.path().join("c.zip");
    make_zip(
        &src,
        &[
            ("P/Foo/x.uasset", fake_asset("/Game/P/Foo/x", &[])),
            ("P/foo/X.uasset", fake_asset("/Game/P/foo/X", &[])),
        ],
    );
    match needs_review(&src, &t.dir("lib")) {
        NeedsReview::Unsafe { code, .. } => assert_eq!(code, "CASE_COLLISION"),
        o => panic!("{o:?}"),
    }
    let e = |p: &str, d: bool| EntryInfo {
        index: 0,
        path: p.into(),
        is_dir: d,
        size: 0,
    };
    let code = |r: Result<(), NeedsReview>| match r {
        Err(NeedsReview::Unsafe { code, .. }) => code,
        o => panic!("{o:?}"),
    };
    assert_eq!(
        code(check_paths(&[e("a/b.txt", false), e("a/b.txt", false)])),
        "DUPLICATE"
    );
    assert_eq!(
        code(check_paths(&[e("a/b.txt", false), e("A/B.txt", false)])),
        "CASE_COLLISION"
    );
    assert_eq!(
        code(check_paths(&[e("a/b", false), e("a/b/c.txt", false)])),
        "FILE_DIR_CONFLICT"
    );
    assert!(check_paths(&[e("a/", true), e("a/b.txt", false), e("a/c.txt", false)]).is_ok());
    assert!(normalize_name("a/b\\c.txt").unwrap() == "a/b/c.txt");
}

#[test]
fn truncated_zip_is_retry_not_failure() {
    let t = Tmp::new();
    let good = t.path().join("g.zip");
    op_room_zip(&good);
    let mut bytes = fs::read(&good).unwrap();
    bytes.truncate(bytes.len() - 40);
    let bad = t.path().join("bad.zip");
    fs::write(&bad, &bytes).unwrap();
    match sanitize_zip(&bad, &t.dir("lib")) {
        Err(SanitizeError::Truncated(_)) => {}
        other => panic!("{other:?}"),
    }
    let garbage = t.path().join("garbage.zip");
    fs::write(&garbage, b"not a zip at all, just text").unwrap();
    assert!(matches!(
        sanitize_zip(&garbage, &t.dir("lib")),
        Err(SanitizeError::Truncated(_)) | Err(SanitizeError::Corrupt(_))
    ));
}

// ---------- identity ----------

#[test]
fn same_hash_redrop_is_duplicate_and_same_name_different_hash_is_distinct() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let a = t.path().join("Pack 5.1.zip");
    op_room_zip(&a);
    let (zip1, _) = sanitized(&a, &lib);
    let again = t.dir("again").join("Pack 5.1.zip");
    fs::copy(&a, &again).unwrap();
    match sanitize_zip(&again, &lib).unwrap() {
        SanitizeOutcome::Duplicate { existing } => assert_eq!(existing, zip1),
        o => panic!("{o:?}"),
    }
    // Same name, different content -> a second library zip.
    let b = t.dir("b").join("Pack 5.1.zip");
    make_zip(
        &b,
        &[(
            "OperatingRoomPack/Levels/Z.umap",
            fake_asset("/Game/OperatingRoom/Levels/Z", &[]),
        )],
    );
    let (zip2, _) = sanitized(&b, &lib);
    assert_ne!(zip1, zip2);
    assert_eq!(scan_library(&lib).len(), 2);
}

#[test]
fn scan_library_drops_orphans_and_survives_bad_json() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let a = t.path().join("Pack.zip");
    op_room_zip(&a);
    sanitized(&a, &lib);
    fs::write(lib.join("orphan.abc.zip.json"), "{}").unwrap();
    fs::write(lib.join("bad.abc.zip"), "z").unwrap();
    fs::write(lib.join("bad.abc.zip.json"), "{ not json").unwrap();
    fs::write(lib.join(".tmp-x.zip.json"), "{}").unwrap();
    assert_eq!(scan_library(&lib).len(), 1);
    assert!(scan_library(&t.path().join("missing")).is_empty());
}

#[test]
fn sidecar_tolerates_unknown_fields() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let a = t.path().join("Pack.zip");
    op_room_zip(&a);
    let (zip, _) = sanitized(&a, &lib);
    let js = PathBuf::from(format!("{}.json", zip.display()));
    let mut v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&js).unwrap()).unwrap();
    v["future_field"] = serde_json::json!({"a": 1});
    fs::write(&js, serde_json::to_vec(&v).unwrap()).unwrap();
    let e = scan_library(&lib);
    assert_eq!(e.len(), 1);
    assert!(e[0].sidecar.extra.contains_key("future_field"));
}

// ---------- claim / reconcile / pipeline ----------

#[test]
fn claim_race_has_exactly_one_winner() {
    let t = Tmp::new();
    let dirs = Dirs::new(&t.dir("import"));
    dirs.create_all().unwrap();
    fs::create_dir_all(&dirs.unprocessed).unwrap();
    for round in 0..25 {
        let name = format!("r{round}.zip");
        // Alternate between the two inboxes: the import root and unprocessed/.
        let src = if round % 2 == 0 {
            dirs.root.join(&name)
        } else {
            dirs.unprocessed.join(&name)
        };
        fs::write(&src, b"x").unwrap();
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let d = dirs.clone();
                let b = barrier.clone();
                let s = src.clone();
                std::thread::spawn(move || {
                    b.wait();
                    matches!(try_claim(&d, &s), ClaimResult::Claimed(_))
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(wins, 1, "round {round}");
        assert!(dirs.processing.join(&name).is_file());
        assert!(!src.exists());
    }
}

#[test]
fn pipeline_moves_zips_to_the_right_folders() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    let dirs = Dirs::new(&import);
    dirs.create_all().unwrap();
    fs::create_dir_all(&dirs.unprocessed).unwrap();
    // Good + copy sit in the import root, Bad in the legacy unprocessed/ folder.
    op_room_zip(&import.join("Good 5.0.zip"));
    make_zip(
        &dirs.unprocessed.join("Bad.zip"),
        &[("P/readme.txt", vec![1])],
    );
    fs::write(import.join("half.zip.part"), b"x").unwrap();
    fs::write(import.join("notes.txt"), b"x").unwrap();
    fs::write(dirs.unprocessed.join("loose.uasset"), b"x").unwrap();
    // Same content dropped under another name.
    fs::copy(import.join("Good 5.0.zip"), import.join("Good copy.zip")).unwrap();

    let mut lines = Vec::new();
    let rep = run_once(&cfg, &mut |l| lines.push(l.to_string()));
    assert_eq!(rep.sanitized.len(), 1, "{lines:?}");
    assert_eq!(rep.duplicates.len(), 1, "{lines:?}");
    assert_eq!(rep.needs_review, vec!["Bad.zip".to_string()]);
    assert_eq!(rep.exit_code(), 2);
    let mut loose = rep.loose_files.clone();
    loose.sort();
    assert_eq!(
        loose,
        vec![
            "notes.txt".to_string(),
            "unprocessed/loose.uasset".to_string()
        ]
    );
    assert!(
        dirs.unprocessed.join("loose.uasset").is_file() && import.join("notes.txt").is_file(),
        "loose files stay in place"
    );
    assert!(dirs.needs_review.join("Bad.zip").is_file());
    assert!(dirs.needs_review.join("Bad.zip.review.json").is_file());
    assert!(
        import.join("half.zip.part").is_file(),
        ".part files are left alone"
    );
    assert_eq!(
        fs::read_dir(&dirs.processed)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".zip"))
            .count(),
        2
    );
    assert_eq!(
        fs::read_dir(&dirs.processing).unwrap().count(),
        0,
        "no claims left behind"
    );
    assert_eq!(scan_library(&lib).len(), 1);
}

#[test]
fn crash_leftovers_are_reconciled() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let dirs = Dirs::new(&import);
    dirs.create_all().unwrap();
    let mut cfg = Config::new(&import, &lib);
    cfg.tmp_age_secs = 0;
    cfg.claim_grace_secs = 0;
    cfg.stale_mins = 0;

    fs::write(lib.join(".tmp-deadbeef.zip"), b"partial").unwrap();
    fs::write(lib.join(".tmp-deadbeef.json"), b"partial").unwrap();
    // A fully written library zip whose sidecar never landed.
    let src = t.path().join("p.zip");
    op_room_zip(&src);
    let (zip, _) = sanitized(&src, &lib);
    fs::remove_file(format!("{}.json", zip.display())).unwrap();
    // A claim that never got its zip, and a stale claim with a zip in processing/.
    fs::write(dirs.processing.join("ghost.zip.claim"), b"{}").unwrap();
    fs::write(dirs.processing.join("stuck.zip"), b"zipdata").unwrap();
    fs::write(dirs.processing.join("stuck.zip.claim"), b"{}").unwrap();

    let actions = reconcile(&cfg);
    assert!(!lib.join(".tmp-deadbeef.zip").exists());
    assert!(!lib.join(".tmp-deadbeef.json").exists());
    assert!(!zip.exists(), "orphan library zip must leave the library");
    assert_eq!(
        fs::read_dir(&dirs.needs_review).unwrap().count(),
        2,
        "zip + review.json: {actions:?}"
    );
    assert!(!dirs.processing.join("ghost.zip.claim").exists());
    assert!(
        actions.iter().any(|a| a.contains("STALE: stuck.zip")),
        "{actions:?}"
    );
    assert!(
        dirs.processing.join("stuck.zip").exists(),
        "without --requeue it stays"
    );

    cfg.requeue = true;
    reconcile(&cfg);
    assert!(
        dirs.root.join("stuck.zip").is_file(),
        "stale claims go back to the import root"
    );
    assert!(!dirs.processing.join("stuck.zip.claim").exists());
}

fn count_zips(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".zip"))
        .count()
}

#[test]
fn zip_in_import_root_is_claimed_and_ends_in_processed() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    op_room_zip(&import.join("Dropped 5.0.zip"));
    let mut lines = Vec::new();
    let rep = run_once(&cfg, &mut |l| lines.push(l.to_string()));
    assert_eq!(
        rep.sanitized,
        vec!["Dropped 5.0.zip".to_string()],
        "{lines:?}"
    );
    let dirs = Dirs::new(&import);
    assert!(!import.join("Dropped 5.0.zip").exists());
    assert!(dirs.processed.join("Dropped 5.0.zip").is_file());
    assert_eq!(count_zips(&dirs.processing), 0);
    assert_eq!(scan_library(&lib).len(), 1);
    assert!(
        !dirs.unprocessed.exists(),
        "unprocessed/ is optional and never created"
    );
    // Second pass with nothing new: the state folders are not mistaken for inboxes.
    let rep = run_once(&cfg, &mut |_| {});
    assert!(rep.sanitized.is_empty() && rep.duplicates.is_empty());
}

#[test]
fn zip_in_unprocessed_still_works() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    let dirs = Dirs::new(&import);
    fs::create_dir_all(&dirs.unprocessed).unwrap();
    op_room_zip(&dirs.unprocessed.join("Legacy 5.0.zip"));
    let rep = run_once(&cfg, &mut |_| {});
    assert_eq!(rep.sanitized.len(), 1);
    assert!(!dirs.unprocessed.join("Legacy 5.0.zip").exists());
    assert!(dirs.processed.join("Legacy 5.0.zip").is_file());
}

#[test]
fn same_name_in_both_inboxes_does_not_clobber() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    let dirs = Dirs::new(&import);
    fs::create_dir_all(&dirs.unprocessed).unwrap();
    op_room_zip(&import.join("Pack.zip"));
    make_zip(
        &dirs.unprocessed.join("Pack.zip"),
        &[(
            "Other/Levels/Z.umap",
            fake_asset("/Game/Other/Levels/Z", &[]),
        )],
    );
    let mut lines = Vec::new();
    let rep = run_once(&cfg, &mut |l| lines.push(l.to_string()));
    // Either both are handled in one pass or the second one in the next pass.
    let mut total = rep.sanitized.len();
    if total < 2 {
        total += run_once(&cfg, &mut |_| {}).sanitized.len();
    }
    assert_eq!(total, 2, "{lines:?}");
    assert_eq!(scan_library(&lib).len(), 2);
    assert_eq!(
        count_zips(&dirs.processed),
        2,
        "both originals preserved in processed/"
    );
    assert_eq!(count_zips(&import), 0);
    assert_eq!(count_zips(&dirs.unprocessed), 0);
    assert_eq!(count_zips(&dirs.processing), 0);
}

// ---------- import ----------

fn project(t: &Tmp) -> PathBuf {
    let p = t.dir("MyProject");
    fs::write(p.join("MyProject.uproject"), "{}").unwrap();
    p
}

#[test]
fn import_skip_and_backup_policies() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let src = t.path().join("Pack.zip");
    op_room_zip(&src);
    sanitized(&src, &lib);
    let entry = scan_library(&lib).remove(0);
    let proj = project(&t);
    let existing = proj.join("Content/OperatingRoom/Levels/Operating_Room.umap");
    fs::create_dir_all(existing.parent().unwrap()).unwrap();
    fs::write(&existing, b"old").unwrap();

    let conflicts = preview_import(&entry, &proj).unwrap();
    assert_eq!(conflicts, vec![existing.clone()]);
    assert!(
        !proj.join("Content/OperatingRoom/Materials").exists(),
        "preview must not write"
    );

    let rep = safe_import(&entry, &proj, ImportPolicy::SkipExisting).unwrap();
    assert_eq!(fs::read(&existing).unwrap(), b"old");
    assert_eq!(rep.skipped, vec![existing.clone()]);
    assert_eq!(rep.written.len(), 3);
    assert!(rep.backed_up.is_empty());
    assert!(proj
        .join("Content/OperatingRoom/Materials/M_Master.uasset")
        .is_file());
    assert!(
        !proj.join("Saved/EAM_Import_Tmp").exists(),
        "temp dir is cleaned up"
    );
    assert!(
        rep.warnings.iter().any(|w| w.contains("outside this pack")),
        "{:?}",
        rep.warnings
    );

    let rep = safe_import(&entry, &proj, ImportPolicy::OverwriteWithBackup).unwrap();
    assert_ne!(fs::read(&existing).unwrap(), b"old");
    assert_eq!(rep.backed_up.len(), 4);
    assert_eq!(rep.written.len(), 4);
    assert!(rep
        .backed_up
        .iter()
        .all(|b| b.starts_with(proj.join("Saved/EAM_Backups"))));
    let old_backup = rep
        .backed_up
        .iter()
        .find(|b| b.ends_with("Operating_Room.umap"))
        .unwrap();
    assert_eq!(fs::read(old_backup).unwrap(), b"old");
    // No .bak files beside the assets.
    assert!(!proj
        .join("Content/OperatingRoom/Levels/Operating_Room.umap.bak")
        .exists());
    assert!(!proj.join("Saved/EAM_Import_Tmp").exists());
}

#[test]
fn import_rejects_entries_outside_allowed_roots_and_non_projects() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let src = t.path().join("Pack.zip");
    op_room_zip(&src);
    sanitized(&src, &lib);
    let mut entry = scan_library(&lib).remove(0);
    let proj = project(&t);

    assert!(matches!(
        safe_import(&entry, &t.dir("not_a_project"), ImportPolicy::SkipExisting),
        Err(ImportError::NotAProject(_))
    ));

    let good_roots = entry.sidecar.allowed_roots.clone();
    entry.sidecar.allowed_roots = vec!["Content/Other".to_string()];
    assert!(matches!(
        safe_import(&entry, &proj, ImportPolicy::SkipExisting),
        Err(ImportError::OutsideAllowedRoots(_))
    ));
    entry.sidecar.allowed_roots = good_roots;
    entry.sidecar.entry_count += 1;
    assert!(matches!(
        safe_import(&entry, &proj, ImportPolicy::SkipExisting),
        Err(ImportError::SidecarMismatch(_))
    ));
    assert!(
        !proj.join("Content").exists(),
        "nothing may be written on rejection"
    );

    // A tampered library zip that smuggles an entry outside the roots.
    let evil = t.path().join("evil.zip");
    make_zip(
        &evil,
        &[
            ("Content/OperatingRoom/ok.uasset", vec![1]),
            ("Config/DefaultEngine.ini", vec![2]),
        ],
    );
    let mut entry2 = scan_library(&lib).remove(0);
    entry2.zip_path = evil;
    entry2.sidecar.entry_count = 2;
    entry2.sidecar.total_uncompressed = 2;
    assert!(matches!(
        safe_import(&entry2, &proj, ImportPolicy::SkipExisting),
        Err(ImportError::OutsideAllowedRoots(_))
    ));
    let slip = t.path().join("slip.zip");
    make_zip(&slip, &[("Content/OperatingRoom/../../x.uasset", vec![1])]);
    entry2.zip_path = slip;
    entry2.sidecar.entry_count = 1;
    entry2.sidecar.total_uncompressed = 1;
    assert!(matches!(
        safe_import(&entry2, &proj, ImportPolicy::SkipExisting),
        Err(ImportError::Unsafe(_))
    ));
}

#[test]
fn plugin_import_warns_without_source_folder() {
    let t = Tmp::new();
    let lib = t.dir("lib");
    let src = t.path().join("Plug.zip");
    make_zip(
        &src,
        &[
            ("P/P.uplugin", b"{}".to_vec()),
            ("P/Source/P/P.Build.cs", b"x".to_vec()),
            ("P/Content/C.uasset", fake_asset("/P/C", &[])),
        ],
    );
    sanitized(&src, &lib);
    let entry = scan_library(&lib).remove(0);
    let proj = project(&t);
    let rep = safe_import(&entry, &proj, ImportPolicy::SkipExisting).unwrap();
    assert!(
        rep.warnings.iter().any(|w| w.contains("Source")),
        "{:?}",
        rep.warnings
    );
    assert!(proj.join("Plugins/P/P.uplugin").is_file());
    fs::create_dir_all(proj.join("Source")).unwrap();
    let rep = safe_import(&entry, &proj, ImportPolicy::SkipExisting).unwrap();
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
}

// ---------- real samples (skipped when absent) ----------

fn sample(name: &str) -> Option<PathBuf> {
    let base = std::env::var("UE_ASSET_SAMPLES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../.claude/scratch/samples").to_string()
    });
    let p = Path::new(&base).join(name);
    p.is_file().then_some(p)
}

#[test]
fn real_samples_plan_as_expected() {
    let cases = [
        (
            "Camping Animations 5.5.zip",
            Kind::Content,
            "CampingAnimations",
        ),
        (
            "Operating Room Pack 5.0.zip",
            Kind::Content,
            "OperatingRoom",
        ),
        (
            "Advanced Crafting System - Plugin 5.6.zip",
            Kind::Plugin,
            "SIZZ_Adv_Crafting_System",
        ),
    ];
    for (file, kind, key) in cases {
        let Some(p) = sample(file) else {
            eprintln!("skipping {file}: not present");
            continue;
        };
        let mut z = ZipArchive::new(fs::File::open(p).unwrap()).unwrap();
        let l = plan_layout(&mut z).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(l.kind, kind, "{file}");
        let want = if kind == Kind::Plugin {
            format!("Plugins/{key}")
        } else {
            format!("Content/{key}")
        };
        assert_eq!(l.allowed_roots, vec![want.clone()], "{file}");
        assert!(
            l.renames
                .iter()
                .all(|r| r.to.starts_with(&format!("{want}/"))),
            "{file}"
        );
        if kind == Kind::Plugin {
            assert!(
                l.renames
                    .iter()
                    .all(|r| !r.to.contains("/Binaries/") && !r.to.contains("/Intermediate/")),
                "{file}"
            );
        }
    }
}

#[test]
fn read_helper_compiles() {
    // Keeps `Read` import used on platforms where other tests are cfg'd out.
    let mut s = String::new();
    std::io::Cursor::new(b"ok".to_vec())
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "ok");
}
