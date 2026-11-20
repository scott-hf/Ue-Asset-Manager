//! Selective import: asset index over a synthetic pack, closure, thumbnails and subset import.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use ue_asset_library::pipeline::{run_once, Config, Dirs};
use ue_asset_library::uasset::synth::Synth;
use ue_asset_library::{
    build_index, closure, index_path, load_or_build_index, preview_import_subset, read_thumbnail,
    safe_import, safe_import_subset, sanitize_zip, scan_library, AssetIndex, ImportError,
    ImportPolicy, Kind, LibraryEntry, SanitizeOutcome, Selection, INDEX_SCHEMA,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

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
            "ue_asset_library_tests/sel-{}-{}-{}",
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

fn make_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut w = ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in entries {
        w.start_file(name.to_string(), opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

fn sanitized(src: &Path, lib: &Path) -> LibraryEntry {
    match sanitize_zip(src, lib).expect("sanitize") {
        SanitizeOutcome::Sanitized { .. } => {}
        other => panic!("unexpected {other:?}"),
    }
    scan_library(lib).remove(0)
}

fn project(t: &Tmp) -> PathBuf {
    let p = t.dir("MyProject");
    fs::write(p.join("MyProject.uproject"), "{}").unwrap();
    p
}

// ---- the synthetic "Grass" pack ----

struct GrassBytes {
    sm_a: Vec<u8>,
    sm_b: Vec<u8>,
    m_a: Vec<u8>,
    m_a_uexp: Vec<u8>,
    t_a: Vec<u8>,
}

fn grass_bytes() -> GrassBytes {
    let sm_a = Synth::new("StaticMesh", "SM_A")
        .dep("/Game/Grass/Materials/M_A")
        .dep("/Game/Other/Thing")
        .dep("/Engine/BasicShapes/Cube")
        .name("/Game/Grass/Meshes/SM_A")
        .build();
    let mut b = Synth::new("StaticMesh", "SM_B").name("/Game/Grass/Meshes/SM_B");
    b.thumbnail = None;
    let sm_b = b.build();
    let m_a = Synth::new("Material", "M_A")
        .soft("/Game/Grass/Textures/T_A")
        .name("/Game/Grass/Materials/M_A")
        .name("/Foo/Bar")
        .build();
    let t_a = Synth::new("Texture2D", "T_A")
        .name("/Game/Grass/Textures/T_A")
        .name("/Game/Grass/Meshes/SM_A.SM_A")
        .name("/Game/Elsewhere/Z")
        .build();
    GrassBytes {
        sm_a,
        sm_b,
        m_a,
        m_a_uexp: vec![7; 50],
        t_a,
    }
}

fn grass_zip(path: &Path, with_bad: bool) -> GrassBytes {
    let g = grass_bytes();
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("GrassPack/Meshes/SM_A.uasset", g.sm_a.clone()),
        ("GrassPack/Meshes/SM_B.uasset", g.sm_b.clone()),
        ("GrassPack/Materials/M_A.uasset", g.m_a.clone()),
        ("GrassPack/Materials/M_A.uexp", g.m_a_uexp.clone()),
        ("GrassPack/Textures/T_A.uasset", g.t_a.clone()),
        ("GrassPack/Textures/T_A.ubulk", vec![1; 10]),
        ("GrassPack/Textures/T_A.m.ubulk", vec![2; 20]),
        ("GrassPack/readme.txt", b"hello".to_vec()),
    ];
    if with_bad {
        entries.push(("GrassPack/Meshes/SM_Bad.uasset", b"garbage".to_vec()));
    }
    make_zip(path, &entries);
    g
}

fn grass_entry(t: &Tmp, with_bad: bool) -> (LibraryEntry, GrassBytes) {
    let src = t.path().join("Grass Pack 5.3.zip");
    let g = grass_zip(&src, with_bad);
    let lib = t.dir("lib");
    (sanitized(&src, &lib), g)
}

fn pkg<'a>(idx: &'a AssetIndex, gp: &str) -> &'a ue_asset_library::PackageInfo {
    idx.packages
        .iter()
        .find(|p| p.game_path == gp)
        .unwrap_or_else(|| panic!("{gp} not in {:?}", idx.packages))
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn index_over_synthetic_pack() {
    let t = Tmp::new();
    let (entry, g) = grass_entry(&t, true);
    assert_eq!(entry.sidecar.kind, Kind::Content);
    let idx = build_index(&entry).unwrap();
    assert_eq!(idx.schema, INDEX_SCHEMA);
    assert_eq!(idx.sidecar_sha256, entry.sidecar.sha256);
    assert_eq!(idx.entry_count, entry.sidecar.entry_count);
    let names: Vec<&str> = idx.packages.iter().map(|p| p.game_path.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "/Game/Grass/Materials/M_A",
            "/Game/Grass/Meshes/SM_A",
            "/Game/Grass/Meshes/SM_B",
            "/Game/Grass/Meshes/SM_Bad",
            "/Game/Grass/Textures/T_A"
        ]
    );
    let a = pkg(&idx, "/Game/Grass/Meshes/SM_A");
    assert_eq!(a.zip_path, "Content/Grass/Meshes/SM_A.uasset");
    assert_eq!(a.class.as_deref(), Some("StaticMesh"));
    assert_eq!(a.deps, s(&["/Game/Grass/Materials/M_A"]));
    assert_eq!(a.external, s(&["/Game/Other/Thing"]));
    assert!(a.has_thumbnail && !a.header_unreadable);
    assert_eq!(a.size, g.sm_a.len() as u64);

    let m = pkg(&idx, "/Game/Grass/Materials/M_A");
    assert_eq!(m.class.as_deref(), Some("Material"));
    assert_eq!(m.deps, s(&["/Game/Grass/Textures/T_A"]));
    assert!(
        m.external.is_empty(),
        "/Foo/Bar must not count: {:?}",
        m.external
    );
    assert_eq!(m.companions, s(&["Content/Grass/Materials/M_A.uexp"]));
    assert_eq!(m.size, (g.m_a.len() + g.m_a_uexp.len()) as u64);

    let tex = pkg(&idx, "/Game/Grass/Textures/T_A");
    assert_eq!(tex.class.as_deref(), Some("Texture2D"));
    assert_eq!(tex.deps, s(&["/Game/Grass/Meshes/SM_A"]));
    assert_eq!(tex.external, s(&["/Game/Elsewhere/Z"]));
    assert_eq!(
        tex.companions,
        s(&[
            "Content/Grass/Textures/T_A.m.ubulk",
            "Content/Grass/Textures/T_A.ubulk"
        ])
    );
    assert_eq!(tex.size, (g.t_a.len() + 30) as u64);

    let b = pkg(&idx, "/Game/Grass/Meshes/SM_B");
    assert!(b.deps.is_empty() && b.external.is_empty() && !b.has_thumbnail);

    let bad = pkg(&idx, "/Game/Grass/Meshes/SM_Bad");
    assert!(bad.header_unreadable && bad.class.is_none() && !bad.has_thumbnail);
    assert_eq!(bad.unreadable_reason.as_deref(), Some("bad_magic"));
    assert!(a.unreadable_reason.is_none());
    assert_eq!(idx.engine_version_min.as_deref(), Some("5.3"));
    assert_eq!(idx.engine_version_max.as_deref(), Some("5.3"));

    assert_eq!(idx.other_files.len(), 1);
    assert_eq!(idx.other_files[0].path, "Content/Grass/readme.txt");
    assert_eq!(idx.other_files[0].size, 5);
}

#[test]
fn closure_handles_cycles_and_unknown() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let idx = build_index(&entry).unwrap();
    // SM_A -> M_A -> T_A -> SM_A (cycle).
    let all = s(&[
        "/Game/Grass/Materials/M_A",
        "/Game/Grass/Meshes/SM_A",
        "/Game/Grass/Textures/T_A",
    ]);
    assert_eq!(closure(&idx, &s(&["/Game/Grass/Meshes/SM_A"])), all);
    assert_eq!(closure(&idx, &s(&["/Game/Grass/Textures/T_A"])), all);
    assert_eq!(
        closure(&idx, &s(&["/Game/Grass/Meshes/SM_B"])),
        s(&["/Game/Grass/Meshes/SM_B"])
    );
    // Case-insensitive, duplicates collapse, unknown dropped.
    assert_eq!(
        closure(
            &idx,
            &s(&[
                "/game/grass/meshes/sm_b",
                "/Game/Grass/Meshes/SM_B",
                "/Game/Nope"
            ])
        ),
        s(&["/Game/Grass/Meshes/SM_B"])
    );
    assert!(closure(&idx, &[]).is_empty());
}

#[test]
fn overlay_is_loaded_validated_and_rebuilt() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let path = index_path(&entry);
    assert!(path.to_string_lossy().ends_with(".zip.assets.json"));
    assert!(!path.exists());
    let built = load_or_build_index(&entry).unwrap();
    assert!(path.is_file(), "overlay written");
    // No temp files left in the library folder.
    let tmp_left = fs::read_dir(entry.zip_path.parent().unwrap())
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(".tmp-"));
    assert!(!tmp_left);
    assert_eq!(load_or_build_index(&entry).unwrap(), built);
    // The scan ignores the overlay.
    assert_eq!(scan_library(entry.zip_path.parent().unwrap()).len(), 1);

    // A fresh overlay is trusted as it is.
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    v["packages"] = serde_json::json!([]);
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(load_or_build_index(&entry).unwrap().packages.is_empty());

    // Stale: entry count or sidecar hash or schema mismatch -> rebuilt and rewritten.
    for (k, val) in [
        ("entry_count", serde_json::json!(999)),
        ("sidecar_sha256", serde_json::json!("deadbeef")),
        ("schema", serde_json::json!(99)),
    ] {
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        v["packages"] = serde_json::json!([]);
        v[k] = val;
        fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let idx = load_or_build_index(&entry).unwrap();
        assert_eq!(idx, built, "{k}");
        let again: AssetIndex = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(again, built, "{k} rewritten");
    }
    // Corrupt json -> rebuilt.
    fs::write(&path, b"{not json").unwrap();
    assert_eq!(load_or_build_index(&entry).unwrap(), built);
    // Write failure is ignored (a directory sits where the overlay goes).
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(load_or_build_index(&entry).unwrap(), built);
}

#[test]
fn thumbnails_are_read_lazily() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, true);
    let png = read_thumbnail(&entry, "/Game/Grass/Meshes/SM_A")
        .unwrap()
        .unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    assert!(read_thumbnail(&entry, "/game/grass/meshes/sm_a")
        .unwrap()
        .is_some());
    assert!(read_thumbnail(&entry, "/Game/Grass/Meshes/SM_B")
        .unwrap()
        .is_none());
    assert!(read_thumbnail(&entry, "/Game/Grass/Meshes/SM_Bad")
        .unwrap()
        .is_none());
    assert!(matches!(
        read_thumbnail(&entry, "/Game/Grass/Nope"),
        Err(ImportError::UnknownPackage(_))
    ));
}

fn sel(paths: &[&str], deps: bool) -> Selection {
    Selection {
        game_paths: s(paths),
        include_deps: deps,
    }
}

fn rel_written(proj: &Path, rep: &ue_asset_library::ImportReport) -> Vec<String> {
    let mut v: Vec<String> = rep
        .written
        .iter()
        .map(|p| {
            p.strip_prefix(proj)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    v.sort();
    v
}

#[test]
fn subset_import_writes_only_selection_and_closure() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let idx = build_index(&entry).unwrap();
    let proj = project(&t);

    // Only SM_B: one file.
    let rep = safe_import_subset(
        &entry,
        &proj,
        ImportPolicy::SkipExisting,
        &idx,
        &sel(&["/Game/Grass/Meshes/SM_B"], true),
    )
    .unwrap();
    assert_eq!(
        rel_written(&proj, &rep),
        vec!["Content/Grass/Meshes/SM_B.uasset"]
    );
    assert!(rep.external_missing.is_empty());
    assert!(!proj.join("Content/Grass/readme.txt").exists());
    assert!(!proj.join("Saved/EAM_Import_Tmp").exists());

    // SM_A with deps: SM_A, M_A (+uexp), T_A (+2 bulk); external reported.
    let rep = safe_import_subset(
        &entry,
        &proj,
        ImportPolicy::SkipExisting,
        &idx,
        &sel(&["/Game/Grass/Meshes/SM_A"], true),
    )
    .unwrap();
    assert_eq!(
        rel_written(&proj, &rep),
        vec![
            "Content/Grass/Materials/M_A.uasset",
            "Content/Grass/Materials/M_A.uexp",
            "Content/Grass/Meshes/SM_A.uasset",
            "Content/Grass/Textures/T_A.m.ubulk",
            "Content/Grass/Textures/T_A.uasset",
            "Content/Grass/Textures/T_A.ubulk",
        ]
    );
    assert_eq!(
        rep.external_missing,
        s(&["/Game/Elsewhere/Z", "/Game/Other/Thing"])
    );
    assert!(rep.warnings.iter().any(|w| w.contains("not in this pack")));
    assert!(!proj.join("Content/Grass/readme.txt").exists());
    // Bytes are exact.
    let on_disk = fs::read(proj.join("Content/Grass/Meshes/SM_A.uasset")).unwrap();
    assert_eq!(on_disk, grass_bytes().sm_a);
}

#[test]
fn subset_without_deps_imports_just_the_selection() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let idx = build_index(&entry).unwrap();
    let proj = project(&t);
    let rep = safe_import_subset(
        &entry,
        &proj,
        ImportPolicy::SkipExisting,
        &idx,
        &sel(&["/Game/Grass/Meshes/SM_A"], false),
    )
    .unwrap();
    assert_eq!(
        rel_written(&proj, &rep),
        vec!["Content/Grass/Meshes/SM_A.uasset"]
    );
    assert!(rep.warnings.iter().any(|w| w.contains("not imported")));
    assert_eq!(rep.external_missing, s(&["/Game/Other/Thing"]));
}

#[test]
fn subset_conflict_policies() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let idx = build_index(&entry).unwrap();
    let proj = project(&t);
    let existing = proj.join("Content/Grass/Materials/M_A.uasset");
    fs::create_dir_all(existing.parent().unwrap()).unwrap();
    fs::write(&existing, b"old").unwrap();
    let pick = sel(&["/Game/Grass/Meshes/SM_A"], true);

    let conflicts = preview_import_subset(&entry, &proj, &idx, &pick).unwrap();
    assert_eq!(conflicts, vec![existing.clone()]);
    assert!(
        !proj.join("Content/Grass/Meshes").exists(),
        "preview must not write"
    );

    let rep = safe_import_subset(&entry, &proj, ImportPolicy::SkipExisting, &idx, &pick).unwrap();
    assert_eq!(fs::read(&existing).unwrap(), b"old");
    assert_eq!(rep.skipped, vec![existing.clone()]);
    assert_eq!(rep.conflicts, vec![existing.clone()]);
    assert_eq!(rep.written.len(), 5);

    let rep = safe_import_subset(
        &entry,
        &proj,
        ImportPolicy::OverwriteWithBackup,
        &idx,
        &pick,
    )
    .unwrap();
    assert_ne!(fs::read(&existing).unwrap(), b"old");
    assert_eq!(rep.backed_up.len(), 6);
    assert_eq!(rep.written.len(), 6);
    assert!(rep
        .backed_up
        .iter()
        .all(|b| b.starts_with(proj.join("Saved/EAM_Backups"))));
    let old = rep
        .backed_up
        .iter()
        .find(|b| b.ends_with("M_A.uasset"))
        .unwrap();
    assert_eq!(fs::read(old).unwrap(), b"old");
    assert!(!proj.join("Saved/EAM_Import_Tmp").exists());
}

#[test]
fn subset_errors() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let idx = build_index(&entry).unwrap();
    let proj = project(&t);
    assert!(matches!(
        safe_import_subset(
            &entry,
            &proj,
            ImportPolicy::SkipExisting,
            &idx,
            &sel(&[], true)
        ),
        Err(ImportError::EmptySelection)
    ));
    assert!(matches!(
        preview_import_subset(&entry, &proj, &idx, &sel(&[], false)),
        Err(ImportError::EmptySelection)
    ));
    match safe_import_subset(
        &entry,
        &proj,
        ImportPolicy::SkipExisting,
        &idx,
        &sel(&["/Game/Grass/Meshes/SM_B", "/Game/Nope/X"], true),
    ) {
        Err(ImportError::UnknownPackage(p)) => assert_eq!(p, "/Game/Nope/X"),
        other => panic!("{other:?}"),
    }
    assert!(
        !proj.join("Content").exists(),
        "a rejected selection writes nothing"
    );
    // Not a project.
    let plain = t.dir("plain");
    assert!(matches!(
        safe_import_subset(
            &entry,
            &plain,
            ImportPolicy::SkipExisting,
            &idx,
            &sel(&["/Game/Grass/Meshes/SM_B"], true)
        ),
        Err(ImportError::NotAProject(_))
    ));
    // Whole-zip validation still runs: a sidecar that disagrees with the zip is refused.
    let mut bad = entry.clone();
    bad.sidecar.entry_count += 1;
    assert!(matches!(
        safe_import_subset(
            &bad,
            &proj,
            ImportPolicy::SkipExisting,
            &idx,
            &sel(&["/Game/Grass/Meshes/SM_B"], true)
        ),
        Err(ImportError::SidecarMismatch(_))
    ));
    // A stale index naming an entry that is not in the zip is refused.
    let mut stale = idx.clone();
    stale.packages[0].zip_path = "Content/Grass/Gone.uasset".into();
    let gp = stale.packages[0].game_path.clone();
    assert!(matches!(
        safe_import_subset(
            &entry,
            &proj,
            ImportPolicy::SkipExisting,
            &stale,
            &Selection {
                game_paths: vec![gp],
                include_deps: false
            }
        ),
        Err(ImportError::SidecarMismatch(_))
    ));
    assert!(!proj.join("Content").exists());
}

#[test]
fn project_pack_indexes_content_only_and_imports_into_content() {
    let t = Tmp::new();
    let sm_a = Synth::new("StaticMesh", "SM_A")
        .dep("/Game/Pack/Materials/M_A")
        .dep("/PluginX/Thing")
        .build();
    let m_a = Synth::new("Material", "M_A").build();
    let plug = Synth::new("Texture2D", "Thing").build();
    let src = t.path().join("Cool Project 5.3.zip");
    make_zip(
        &src,
        &[
            ("Cool/Cool.uproject", b"{\"x\":1}".to_vec()),
            ("Cool/Config/DefaultEngine.ini", b"[a]".to_vec()),
            ("Cool/Content/Pack/Meshes/SM_A.uasset", sm_a),
            ("Cool/Content/Pack/Materials/M_A.uasset", m_a),
            ("Cool/Content/Pack/notes.md", b"n".to_vec()),
            ("Cool/Plugins/PluginX/Content/Thing.uasset", plug),
            ("Cool/Source/Cool.Target.cs", b"x".to_vec()),
        ],
    );
    let lib = t.dir("lib");
    let entry = sanitized(&src, &lib);
    assert_eq!(entry.sidecar.kind, Kind::Project);
    let idx = load_or_build_index(&entry).unwrap();
    let names: Vec<&str> = idx.packages.iter().map(|p| p.game_path.as_str()).collect();
    assert_eq!(
        names,
        vec!["/Game/Pack/Materials/M_A", "/Game/Pack/Meshes/SM_A"]
    );
    let a = pkg(&idx, "/Game/Pack/Meshes/SM_A");
    assert_eq!(a.zip_path, "Content/Pack/Meshes/SM_A.uasset");
    assert_eq!(a.deps, s(&["/Game/Pack/Materials/M_A"]));
    assert_eq!(a.external, s(&["/PluginX/Thing"]));
    assert_eq!(
        idx.other_files
            .iter()
            .map(|o| o.path.as_str())
            .collect::<Vec<_>>(),
        vec!["Content/Pack/notes.md"]
    );

    let proj = project(&t);
    let rep = safe_import_subset(
        &entry,
        &proj,
        ImportPolicy::SkipExisting,
        &idx,
        &sel(&["/Game/Pack/Meshes/SM_A"], true),
    )
    .unwrap();
    assert_eq!(
        rel_written(&proj, &rep),
        vec![
            "Content/Pack/Materials/M_A.uasset",
            "Content/Pack/Meshes/SM_A.uasset"
        ]
    );
    assert_eq!(rep.external_missing, s(&["/PluginX/Thing"]));
    // The project's own files are untouched and nothing outside Content/ arrived.
    assert_eq!(fs::read(proj.join("MyProject.uproject")).unwrap(), b"{}");
    assert!(!proj.join("Config").exists());
    assert!(!proj.join("Source").exists());
    assert!(!proj.join("Plugins").exists());
    assert!(!proj.join("Cool.uproject").exists());
    // Full-pack import of a project stays refused.
    assert!(matches!(
        safe_import(&entry, &proj, ImportPolicy::SkipExisting),
        Err(ImportError::WrongKind(Kind::Project))
    ));
}

#[test]
fn plugin_packs_are_not_indexable() {
    let t = Tmp::new();
    let src = t.path().join("MyPlug 5.3.zip");
    make_zip(
        &src,
        &[
            (
                "MyPlug/MyPlug.uplugin",
                b"{\"FriendlyName\":\"x\",\"EngineVersion\":\"5.3.0\"}".to_vec(),
            ),
            (
                "MyPlug/Content/A.uasset",
                Synth::new("Material", "A").build(),
            ),
        ],
    );
    let lib = t.dir("lib");
    let entry = sanitized(&src, &lib);
    assert_eq!(entry.sidecar.kind, Kind::Plugin);
    assert!(matches!(
        build_index(&entry),
        Err(ImportError::WrongKind(Kind::Plugin))
    ));
    assert!(matches!(
        load_or_build_index(&entry),
        Err(ImportError::WrongKind(Kind::Plugin))
    ));
    let idx = AssetIndex {
        schema: INDEX_SCHEMA,
        sidecar_sha256: String::new(),
        entry_count: 0,
        packages: vec![],
        other_files: vec![],
        engine_version_min: None,
        engine_version_max: None,
    };
    let proj = project(&t);
    assert!(matches!(
        safe_import_subset(
            &entry,
            &proj,
            ImportPolicy::SkipExisting,
            &idx,
            &sel(&["/MyPlug/A"], true)
        ),
        Err(ImportError::WrongKind(Kind::Plugin))
    ));
}

#[test]
fn sanitizer_pipeline_writes_the_overlay_and_plugins_get_none() {
    let t = Tmp::new();
    let import = t.dir("import");
    let lib = t.dir("library");
    let mut cfg = Config::new(&import, &lib);
    cfg.stable_secs = 0;
    Dirs::new(&import).create_all().unwrap();
    grass_zip(&import.join("Grass Pack 5.3.zip"), false);
    make_zip(
        &import.join("Plug 5.3.zip"),
        &[
            ("Plug/Plug.uplugin", b"{}".to_vec()),
            ("Plug/Content/A.uasset", Synth::new("Material", "A").build()),
        ],
    );
    let mut lines = Vec::new();
    let rep = run_once(&cfg, &mut |l| lines.push(l.to_string()));
    assert_eq!(rep.sanitized.len(), 2, "{lines:?}");
    let entries = scan_library(&lib);
    assert_eq!(entries.len(), 2);
    for e in &entries {
        match e.sidecar.kind {
            Kind::Content => {
                assert!(index_path(e).is_file(), "{lines:?}");
                let idx: AssetIndex =
                    serde_json::from_slice(&fs::read(index_path(e)).unwrap()).unwrap();
                assert_eq!(idx.packages.len(), 4);
                assert_eq!(idx, build_index(e).unwrap());
            }
            _ => assert!(!index_path(e).exists()),
        }
    }
    assert!(
        lines.iter().any(|l| l.contains("asset index written")),
        "{lines:?}"
    );
    // The overlay does not disturb the library scan or reconcile.
    let rep2 = run_once(&cfg, &mut |_| {});
    assert!(rep2.sanitized.is_empty());
    assert_eq!(scan_library(&lib).len(), 2);
}

/// Real library: `UE_ASSET_REAL_LIBRARY=<dir>` (e.g. `//scott-server/ue_assets_library/library`).
/// Builds an in-memory index for every content/project zip (nothing is written) and prints stats.
#[test]
#[ignore]
fn real_library_index_stats() {
    let Some(dir) = std::env::var_os("UE_ASSET_REAL_LIBRARY") else {
        eprintln!("UE_ASSET_REAL_LIBRARY not set; skipped");
        return;
    };
    let mut dir = PathBuf::from(dir);
    if scan_library(&dir).is_empty() && dir.join("library").is_dir() {
        dir = dir.join("library");
    }
    let entries = scan_library(&dir);
    eprintln!("{} entries in {}", entries.len(), dir.display());
    let (mut zips, mut pkgs, mut deps, mut ext, mut thumbs, mut bad, mut other) =
        (0, 0, 0, 0, 0, 0, 0);
    let mut classes: std::collections::BTreeMap<String, usize> = Default::default();
    let started = std::time::Instant::now();
    for e in &entries {
        if !matches!(e.sidecar.kind, Kind::Content | Kind::Project) {
            continue;
        }
        let t0 = std::time::Instant::now();
        match build_index(e) {
            Ok(idx) => {
                zips += 1;
                let (mut p_t, mut p_b) = (0, 0);
                for p in &idx.packages {
                    pkgs += 1;
                    deps += p.deps.len();
                    ext += p.external.len();
                    if p.has_thumbnail {
                        thumbs += 1;
                        p_t += 1;
                    }
                    if p.header_unreadable {
                        bad += 1;
                        p_b += 1;
                    }
                    *classes
                        .entry(p.class.clone().unwrap_or_else(|| "?".into()))
                        .or_default() += 1;
                }
                other += idx.other_files.len();
                eprintln!(
                    "{:<50} {:?} pkgs {:>5} thumbs {:>5} unreadable {:>4} ({:.1}s)",
                    e.sidecar.display_name,
                    e.sidecar.kind,
                    idx.packages.len(),
                    p_t,
                    p_b,
                    t0.elapsed().as_secs_f32()
                );
            }
            Err(err) => eprintln!("{}: ERROR {err}", e.sidecar.display_name),
        }
    }
    let mut top: Vec<_> = classes.into_iter().collect();
    top.sort_by_key(|a| std::cmp::Reverse(a.1));
    eprintln!(
        "STATS zips {zips} packages {pkgs} deps {deps} external {ext} thumbnails {thumbs} \
         header_unreadable {bad} other_files {other} in {:.1}s",
        started.elapsed().as_secs_f32()
    );
    eprintln!("top classes: {:?}", &top[..top.len().min(12)]);
}

/// Diagnose unreadable packages of one real pack (read-only). `UE_ASSET_REAL_LIBRARY=<dir>`,
/// `UE_ASSET_REAL_PACK=<display name substring>`.
#[test]
#[ignore]
fn real_pack_unreadable_diagnosis() {
    let (Some(dir), Some(pack)) = (
        std::env::var_os("UE_ASSET_REAL_LIBRARY"),
        std::env::var("UE_ASSET_REAL_PACK").ok(),
    ) else {
        eprintln!("UE_ASSET_REAL_LIBRARY / UE_ASSET_REAL_PACK not set; skipped");
        return;
    };
    let entries = scan_library(Path::new(&dir));
    let e = entries
        .iter()
        .find(|e| e.sidecar.display_name.contains(&pack))
        .expect("pack not found");
    let idx = build_index(e).unwrap();
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut engines: std::collections::BTreeMap<String, usize> = Default::default();
    for p in &idx.packages {
        if p.header_unreadable {
            let why = ue_asset_library::asset_index::probe_package(e, &p.zip_path)
                .map(|ok| format!("PARSES NOW: {ok}"))
                .unwrap_or_else(|e| e);
            // Group by message without the head bytes.
            let key = why.split(" [head").next().unwrap_or(&why).to_string();
            let sample = format!("{} {}", p.zip_path, why.split("[head").nth(1).unwrap_or(""));
            groups.entry(key).or_default().push(sample);
        } else if let Ok(s) = ue_asset_library::asset_index::probe_package(e, &p.zip_path) {
            let k = s.split("engine ").nth(1).unwrap_or("").to_string();
            *engines.entry(k).or_default() += 1;
        }
    }
    eprintln!("sidecar engine: {:?}", e.sidecar.engine_version_guess);
    eprintln!("readable engines: {engines:?}");
    for (k, v) in &groups {
        eprintln!("{} x {k}", v.len());
        for s in v.iter().take(3) {
            eprintln!("    {s}");
        }
    }
}

#[test]
fn old_overlays_without_new_fields_still_load() {
    let t = Tmp::new();
    let (entry, _) = grass_entry(&t, false);
    let built = load_or_build_index(&entry).unwrap();
    let path = index_path(&entry);
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("engine_version_min");
    v.as_object_mut().unwrap().remove("engine_version_max");
    for p in v["packages"].as_array_mut().unwrap() {
        p.as_object_mut().unwrap().remove("unreadable_reason");
    }
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let old = load_or_build_index(&entry).unwrap();
    assert_eq!(old.engine_version_min, None);
    assert_eq!(old.packages, built.packages);
}

/// Reproduce the zip crate's malformed record for raw copies past 4 GiB: the offset field is
/// 0xFFFFFFFF and the extra carries the zip64 block twice, `[usize, csize, offset]` then `[0, 0, offset]`.
fn damage_like_zip_crate(path: &Path, names: &[&str]) {
    let mut data = fs::read(path).unwrap();
    let n = data.len();
    assert_eq!(&data[n - 22..n - 18], b"PK\x05\x06");
    let cd_size = u32::from_le_bytes(data[n - 10..n - 6].try_into().unwrap()) as usize;
    let cd_off = u32::from_le_bytes(data[n - 6..n - 2].try_into().unwrap()) as usize;
    let u32_at = |d: &[u8], i: usize| u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
    let u16_at = |d: &[u8], i: usize| u16::from_le_bytes(d[i..i + 2].try_into().unwrap()) as usize;
    let cd = data[cd_off..cd_off + cd_size].to_vec();
    let mut new_cd = Vec::new();
    let mut p = 0;
    let mut hit = 0;
    while p < cd.len() {
        let (nl, el, cl) = (
            u16_at(&cd, p + 28),
            u16_at(&cd, p + 30),
            u16_at(&cd, p + 32),
        );
        let end = p + 46 + nl + el + cl;
        let name = String::from_utf8_lossy(&cd[p + 46..p + 46 + nl]).to_string();
        if names.contains(&name.as_str()) {
            hit += 1;
            let mut rec = cd[p..end].to_vec();
            let (csize, usize_, off) = (
                u32_at(&cd, p + 20) as u64,
                u32_at(&cd, p + 24) as u64,
                u32_at(&cd, p + 42) as u64,
            );
            rec[42..46].copy_from_slice(&u32::MAX.to_le_bytes());
            rec[30..32].copy_from_slice(&((el + 56) as u16).to_le_bytes());
            let mut extra = Vec::new();
            for (a, b) in [(usize_, csize), (0, 0)] {
                extra.extend_from_slice(&1u16.to_le_bytes());
                extra.extend_from_slice(&24u16.to_le_bytes());
                extra.extend_from_slice(&a.to_le_bytes());
                extra.extend_from_slice(&b.to_le_bytes());
                extra.extend_from_slice(&off.to_le_bytes());
            }
            let at = 46 + nl + el;
            rec.splice(at..at, extra);
            new_cd.extend(rec);
        } else {
            new_cd.extend_from_slice(&cd[p..end]);
        }
        p = end;
    }
    assert_eq!(hit, names.len());
    data.truncate(cd_off);
    data.extend_from_slice(&new_cd);
    let mut eocd = vec![0u8; 22];
    eocd.copy_from_slice(&fs::read(path).unwrap()[n - 22..]);
    eocd[12..16].copy_from_slice(&(new_cd.len() as u32).to_le_bytes());
    data.extend_from_slice(&eocd);
    fs::write(path, data).unwrap();
}

#[test]
fn zips_with_duplicated_zip64_blocks_are_read_through_the_repair_view() {
    use std::io::Read;
    let t = Tmp::new();
    let (entry, g) = grass_entry(&t, false);
    let clean = build_index(&entry).unwrap();
    let damaged = [
        "Content/Grass/Meshes/SM_A.uasset",
        "Content/Grass/Materials/M_A.uasset",
        "Content/Grass/Materials/M_A.uexp",
        "Content/Grass/Textures/T_A.uasset",
    ];
    damage_like_zip_crate(&entry.zip_path, &damaged);
    // The premise: the zip crate now reports size 0 and fails the CRC for those entries.
    {
        let mut z = zip::ZipArchive::new(fs::File::open(&entry.zip_path).unwrap()).unwrap();
        let mut f = z.by_name(damaged[0]).unwrap();
        assert_eq!(f.size(), 0);
        let mut v = Vec::new();
        assert!(f.read_to_end(&mut v).is_err());
    }
    // Index: nothing unreadable, identical to the index of the healthy zip.
    let idx = build_index(&entry).unwrap();
    assert!(
        idx.packages.iter().all(|p| !p.header_unreadable),
        "{:?}",
        idx.packages
    );
    assert_eq!(idx, clean);
    assert!(read_thumbnail(&entry, "/Game/Grass/Meshes/SM_A")
        .unwrap()
        .is_some());
    // Subset import and full import write the right bytes (validate sees the real sizes).
    let proj = project(&t);
    let pick = sel(&["/Game/Grass/Meshes/SM_A"], true);
    assert!(preview_import_subset(&entry, &proj, &idx, &pick)
        .unwrap()
        .is_empty());
    safe_import_subset(&entry, &proj, ImportPolicy::SkipExisting, &idx, &pick).unwrap();
    assert_eq!(
        fs::read(proj.join("Content/Grass/Meshes/SM_A.uasset")).unwrap(),
        g.sm_a
    );
    assert_eq!(
        fs::read(proj.join("Content/Grass/Materials/M_A.uasset")).unwrap(),
        g.m_a
    );
    assert_eq!(
        fs::read(proj.join("Content/Grass/Materials/M_A.uexp")).unwrap(),
        g.m_a_uexp
    );
    assert_eq!(
        fs::read(proj.join("Content/Grass/Textures/T_A.uasset")).unwrap(),
        g.t_a
    );
    let proj2 = t.dir("Second");
    fs::write(proj2.join("Second.uproject"), "{}").unwrap();
    let rep = safe_import(&entry, &proj2, ImportPolicy::SkipExisting).unwrap();
    assert_eq!(rep.written.len(), 8);
    assert_eq!(
        fs::read(proj2.join("Content/Grass/Meshes/SM_A.uasset")).unwrap(),
        g.sm_a
    );
}

/// Read-only check against the real megapack: `UE_ASSET_REAL_LIBRARY`, `UE_ASSET_REAL_PACK`.
#[test]
#[ignore]
fn real_pack_repaired_index_and_preview() {
    let (Some(dir), Some(pack)) = (
        std::env::var_os("UE_ASSET_REAL_LIBRARY"),
        std::env::var("UE_ASSET_REAL_PACK").ok(),
    ) else {
        eprintln!("UE_ASSET_REAL_LIBRARY / UE_ASSET_REAL_PACK not set; skipped");
        return;
    };
    let entries = scan_library(Path::new(&dir));
    let e = entries
        .iter()
        .find(|e| e.sidecar.display_name.contains(&pack))
        .expect("pack not found");
    let t0 = std::time::Instant::now();
    let idx = build_index(e).unwrap();
    let bad = idx.packages.iter().filter(|p| p.header_unreadable).count();
    eprintln!(
        "packages {} unreadable {} engine {:?}..{:?} in {:.1}s",
        idx.packages.len(),
        bad,
        idx.engine_version_min,
        idx.engine_version_max,
        t0.elapsed().as_secs_f32()
    );
    let tmp = Tmp::new();
    let proj = project(&tmp);
    // Three assets from the previously broken tail of the archive (late zip entries).
    let mut late: Vec<&ue_asset_library::PackageInfo> = idx.packages.iter().collect();
    late.sort_by(|a, b| a.zip_path.cmp(&b.zip_path));
    let picks: Vec<String> = idx
        .packages
        .iter()
        .filter(|p| {
            p.zip_path.contains("/Textures/T_Hat") || p.zip_path.contains("/Textures/T_Hammer")
        })
        .take(3)
        .map(|p| p.game_path.clone())
        .collect();
    eprintln!("picks {picks:?}");
    let s = Selection {
        game_paths: picks,
        include_deps: true,
    };
    let t1 = std::time::Instant::now();
    let conflicts = preview_import_subset(e, &proj, &idx, &s).unwrap();
    eprintln!(
        "preview ok, {} conflicts in {:.1}s",
        conflicts.len(),
        t1.elapsed().as_secs_f32()
    );
    // Extract (reads from the share, writes only into the local temp project).
    let one = Selection {
        game_paths: s.game_paths[2..3].to_vec(),
        include_deps: false,
    };
    let rep = safe_import_subset(e, &proj, ImportPolicy::SkipExisting, &idx, &one).unwrap();
    let want: u64 = idx
        .packages
        .iter()
        .find(|p| p.game_path == one.game_paths[0])
        .unwrap()
        .size;
    let got: u64 = rep
        .written
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    eprintln!(
        "imported {} files, {got} bytes (index says {want})",
        rep.written.len()
    );
    assert_eq!(got, want);
}
