//! End-to-end tests: a synthetic library dir (real zips + hand-made `.assets.json` overlays),
//! the catalog loaded from it, and the MCP protocol driven through `Server::handle_line`.

use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use ue_asset_catalog::catalog::{Catalog, LoadOptions, PackStatus};
use ue_asset_catalog::tools::{App, AppConfig};
use ue_asset_catalog::Server;
use ue_asset_library::asset_index::write_index;
use ue_asset_library::{AssetIndex, Kind, LibraryEntry, PackageInfo, Sidecar, INDEX_SCHEMA};
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
            "ue_asset_catalog_it/{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
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

struct Pk {
    game_path: &'static str,
    class: &'static str,
    deps: &'static [&'static str],
    external: &'static [&'static str],
}

fn pk(game_path: &'static str, class: &'static str, deps: &'static [&'static str]) -> Pk {
    Pk {
        game_path,
        class,
        deps,
        external: &[],
    }
}

fn sha(c: char) -> String {
    format!("{}{}", c.to_string().repeat(12), "0".repeat(52))
}

fn slug(name: &str) -> String {
    name.to_lowercase().replace(' ', "-")
}

/// Write `<slug>.<sha12>.zip` + sidecar + overlay into `lib`.
fn make_pack(
    lib: &Path,
    name: &str,
    sha_char: char,
    engine: &str,
    kind: Kind,
    pkgs: &[Pk],
    with_overlay: bool,
) -> LibraryEntry {
    let sha = sha(sha_char);
    let sha12 = &sha[..12];
    let zip_path = lib.join(format!("{}.{sha12}.zip", slug(name)));
    let mut w = ZipWriter::new(fs::File::create(&zip_path).unwrap());
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut total = 0u64;
    let mut count = 0u64;
    let mut infos: Vec<PackageInfo> = Vec::new();
    let mut first_folder = String::new();
    for p in pkgs {
        let rel = p.game_path.trim_start_matches("/Game/");
        if first_folder.is_empty() {
            first_folder = rel.split('/').next().unwrap().to_string();
        }
        let main = format!("Content/{rel}.uasset");
        let exp = format!("Content/{rel}.uexp");
        let body = format!("{sha12}:{}", p.game_path).into_bytes();
        let mut size = 0u64;
        for (path, data) in [(&main, &body), (&exp, &body)] {
            w.start_file(path.clone(), opts).unwrap();
            w.write_all(data).unwrap();
            total += data.len() as u64;
            size += data.len() as u64;
            count += 1;
        }
        infos.push(PackageInfo {
            game_path: p.game_path.to_string(),
            zip_path: main,
            companions: vec![exp],
            class: Some(p.class.to_string()),
            size,
            deps: p.deps.iter().map(|s| (*s).to_string()).collect(),
            external: p.external.iter().map(|s| (*s).to_string()).collect(),
            has_thumbnail: false,
            header_unreadable: false,
            unreadable_reason: None,
        });
    }
    w.finish().unwrap();
    let sidecar = Sidecar {
        schema: 1,
        kind,
        display_name: name.to_string(),
        source_zip: format!("{name}.zip"),
        sha256: sha.clone(),
        engine_version_guess: Some(engine.to_string()),
        content_folder: Some(first_folder.clone()),
        plugin_name: None,
        zip_prefix: String::new(),
        game_prefix: String::new(),
        allowed_roots: vec![format!("Content/{first_folder}")],
        entry_count: count,
        total_uncompressed: total,
        external_refs: Vec::new(),
        sanitized_at: "2026-01-01T00:00:00Z".to_string(),
        marketplace_url: Some("https://example.invalid/pack".to_string()),
        friendly_name: None,
        extra: serde_json::Map::new(),
    };
    let sidecar_path = lib.join(format!("{}.{sha12}.zip.json", slug(name)));
    fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
    let entry = LibraryEntry {
        sidecar,
        zip_path,
        sidecar_path,
    };
    if with_overlay {
        let idx = AssetIndex {
            schema: INDEX_SCHEMA,
            sidecar_sha256: sha,
            entry_count: count,
            packages: infos,
            other_files: Vec::new(),
            engine_version_min: None,
            engine_version_max: None,
        };
        write_index(&entry, &idx).unwrap();
    }
    entry
}

/// The standard test library: two releases of "Nature Pack", a castle kit and a plugin.
fn build_library(lib: &Path) {
    let nature = |c: char, eng: &str, extra: Option<Pk>| {
        let mut v = vec![
            pk(
                "/Game/Nature/Foliage/SM_Grass_01",
                "StaticMesh",
                &["/Game/Nature/Materials/M_Grass"],
            ),
            Pk {
                external: &["/Game/Shared/T_Missing"],
                ..pk("/Game/Nature/Materials/M_Grass", "Material", &[])
            },
            pk("/Game/Nature/Rocks/SM_Rock_Big", "StaticMesh", &[]),
        ];
        if let Some(e) = extra {
            v.push(e);
        }
        make_pack(lib, "Nature Pack", c, eng, Kind::Content, &v, true);
    };
    nature('a', "5.3", None);
    nature(
        'b',
        "5.5",
        Some(pk("/Game/Nature/Foliage/SM_Bush", "StaticMesh", &[])),
    );
    make_pack(
        lib,
        "Castle Kit",
        'c',
        "5.4",
        Kind::Content,
        &[
            pk("/Game/Castle/Doors/SM_Door", "StaticMesh", &[]),
            pk("/Game/Castle/Doors/SM_Door_Frame", "StaticMesh", &[]),
            pk("/Game/Castle/Doors/SM_Gate", "StaticMesh", &[]),
        ],
        true,
    );
    // A plugin: listed, never indexed.
    make_pack(
        lib,
        "Cool Plugin",
        'd',
        "5.4",
        Kind::Plugin,
        &[pk("/Game/Cool/BP_Thing", "Blueprint", &[])],
        false,
    );
}

fn make_project(root: &Path, engine: &str) -> PathBuf {
    fs::create_dir_all(root.join("Content")).unwrap();
    let up = root.join("Test.uproject");
    fs::write(
        &up,
        format!(r#"{{"FileVersion":3,"EngineAssociation":"{engine}"}}"#),
    )
    .unwrap();
    up
}

struct Env {
    _tmp: Tmp,
    lib: PathBuf,
    proj: PathBuf,
    uproject: PathBuf,
    log: PathBuf,
    server: Server,
    next_id: u64,
}

fn env_with(engine: &str) -> Env {
    let tmp = Tmp::new();
    let lib = tmp.dir("lib");
    build_library(&lib);
    let proj = tmp.dir("proj");
    let uproject = make_project(&proj, engine);
    let log = tmp.0.join("install_log.json");
    let app = App::new(AppConfig {
        libraries: vec![lib.clone()],
        projects: vec![uproject.clone()],
        load: LoadOptions {
            build_missing: false,
            cache_path: None,
            threads: 2,
            ..LoadOptions::default()
        },
        install_log: Some(log.clone()),
        ..AppConfig::default()
    });
    Env {
        _tmp: tmp,
        lib,
        proj,
        uproject,
        log,
        server: Server::new(app),
        next_id: 1,
    }
}

impl Env {
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = self.server.handle_line(&req.to_string()).expect("reply");
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["id"], id);
        v
    }

    /// Call a tool; returns (is_error, text, parsed JSON of the text if it is JSON).
    fn tool(&mut self, name: &str, args: Value) -> (bool, String, Value) {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        assert!(r.get("error").is_none(), "protocol error: {r}");
        let is_error = r["result"]["isError"].as_bool().unwrap();
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
        (is_error, text, parsed)
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (err, text, v) = self.tool(name, args);
        assert!(!err, "{name} failed: {text}");
        v
    }
}

fn proj_file(env: &Env, rel: &str) -> PathBuf {
    env.proj.join(rel)
}

// ---------------------------------------------------------------------------------------------

#[test]
fn catalog_loads_from_a_library_dir_with_cache() {
    let tmp = Tmp::new();
    let lib = tmp.dir("lib");
    build_library(&lib);
    let cache = tmp.0.join("cache/catalog.json");
    let opts = LoadOptions {
        build_missing: false,
        cache_path: Some(cache.clone()),
        threads: 3,
        ..LoadOptions::default()
    };
    let (c, rep) = Catalog::load(std::slice::from_ref(&lib), &opts);
    assert_eq!(rep.entries, 4);
    assert_eq!(rep.from_overlay, 3);
    assert_eq!(rep.skipped_plugins, 1);
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(rep.cache_written);
    assert!(cache.exists());
    let st = c.stats();
    assert_eq!(st.packs, 4);
    assert_eq!(st.indexed_packs, 3);
    // Two Nature releases are one family: grass, material, rock, bush + 3 castle assets.
    assert_eq!(st.assets, 7);
    assert_eq!(st.families, 3);
    assert!(c
        .packs
        .iter()
        .any(|p| matches!(p.status, PackStatus::NotIndexable(_))));

    // Second load comes from the cache.
    let (c2, rep2) = Catalog::load(std::slice::from_ref(&lib), &opts);
    assert_eq!(rep2.from_cache, 3);
    assert!(!rep2.cache_written);
    assert_eq!(c2.stats().assets, 7);
    assert!(!c2.needs_reload());

    // A corrupt cache is ignored (and rewritten).
    fs::write(&cache, "garbage").unwrap();
    let (c3, rep3) = Catalog::load(std::slice::from_ref(&lib), &opts);
    assert_eq!(rep3.from_cache, 0);
    assert_eq!(c3.stats().assets, 7);
    assert!(rep3.cache_written);

    // New pack in the dir: the fingerprint changes.
    make_pack(
        &lib,
        "Extra Pack",
        'e',
        "5.1",
        Kind::Content,
        &[pk("/Game/Extra/SM_Thing", "StaticMesh", &[])],
        true,
    );
    assert!(c3.needs_reload());
    let (c4, _) = Catalog::load(std::slice::from_ref(&lib), &opts);
    assert_eq!(c4.stats().assets, 8);
}

#[test]
fn stale_overlay_is_reported_when_building_is_off() {
    let tmp = Tmp::new();
    let lib = tmp.dir("lib");
    let e = make_pack(
        &lib,
        "Stale Pack",
        'f',
        "5.2",
        Kind::Content,
        &[pk("/Game/Stale/SM_A", "StaticMesh", &[])],
        true,
    );
    // Break the overlay: wrong sidecar hash.
    let mut idx: AssetIndex =
        serde_json::from_slice(&fs::read(ue_asset_library::index_path(&e)).unwrap()).unwrap();
    idx.sidecar_sha256 = "different".into();
    write_index(&e, &idx).unwrap();
    let (c, rep) = Catalog::load(
        std::slice::from_ref(&lib),
        &LoadOptions {
            build_missing: false,
            cache_path: None,
            threads: 1,
            ..LoadOptions::default()
        },
    );
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(c.stats().failed_packs, 1);
    assert_eq!(c.stats().assets, 0);
}

#[test]
fn protocol_handshake_and_tool_list() {
    let mut env = env_with("5.4");
    let r = env.rpc(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert!(env
        .server
        .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .is_none());
    let r = env.rpc("tools/list", json!({}));
    let names: Vec<String> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for n in [
        "search_assets",
        "get_asset",
        "list_packs",
        "get_pack",
        "list_tags",
        "get_thumbnail",
        "plan_import",
        "import_assets",
        "refresh_catalog",
    ] {
        assert!(names.iter().any(|x| x == n), "missing {n}");
    }
    let r = env.rpc("no/such", json!({}));
    assert_eq!(r["error"]["code"], -32601);
}

#[test]
fn search_get_asset_packs_and_tags() {
    let mut env = env_with("5.4");
    let v = env.ok("search_assets", json!({"query": "grass"}));
    assert!(v["total"].as_u64().unwrap() >= 1);
    let first = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "SM_Grass_01")
        .expect("SM_Grass_01 is a hit");
    assert_eq!(first["kind"], "staticmesh");
    // Listed once, with both engine versions.
    assert_eq!(first["engine_versions"], json!(["5.5", "5.3"]));
    let id = first["id"].as_str().unwrap().to_string();
    assert!(
        id.starts_with("bbbbbbbbbbbb:/Game/Nature/Foliage/SM_Grass_01"),
        "{id}"
    );

    // Synonyms and filters.
    let v = env.ok("search_assets", json!({"query": "foliage", "kind": "mesh"}));
    assert!(v["total"].as_u64().unwrap() >= 2);
    let v = env.ok("search_assets", json!({"query": "", "kind": "material"}));
    assert_eq!(v["total"], 1);
    let v = env.ok("search_assets", json!({"query": "door", "pack": "castle"}));
    assert_eq!(v["hits"][0]["name"], "SM_Door");
    // The plugin never shows up.
    let v = env.ok("search_assets", json!({"query": "thing"}));
    assert_eq!(v["total"], 0);
    // Wrong argument types are tool errors, not protocol errors.
    let (err, text, _) = env.tool("search_assets", json!({"limit": "many"}));
    assert!(err, "{text}");

    // get_asset, by the representative id and by the older release's id.
    let d = env.ok("get_asset", json!({"id": id}));
    assert_eq!(d["class"], "StaticMesh");
    assert_eq!(d["releases"].as_array().unwrap().len(), 2);
    assert_eq!(d["dependencies_in_pack"][0]["name"], "M_Grass");
    assert_eq!(d["transitive_dependencies_in_pack"], 1);
    assert_eq!(d["external_dependencies_total"], 0);
    let older = "aaaaaaaaaaaa:/Game/Nature/Foliage/SM_Grass_01";
    let d2 = env.ok("get_asset", json!({"id": older}));
    assert_eq!(d2["id"], d["id"]);
    let dm = env.ok(
        "get_asset",
        json!({"id": "aaaaaaaaaaaa:/Game/Nature/Materials/M_Grass"}),
    );
    assert_eq!(dm["external_dependencies"][0], "/Game/Shared/T_Missing");
    let (err, _, _) = env.tool("get_asset", json!({"id": "bbbbbbbbbbbb:/Game/Nope"}));
    assert!(err);
    let (err, _, _) = env.tool("get_asset", json!({}));
    assert!(err);

    let p = env.ok("list_packs", json!({}));
    assert_eq!(p["total_packs"], 3);
    let plugin = p["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "plugin")
        .unwrap();
    assert_eq!(plugin["indexable"], false);
    let nature = p["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "Nature Pack")
        .unwrap();
    assert_eq!(nature["releases"].as_array().unwrap().len(), 2);
    assert_eq!(nature["asset_count"], 4);
    let gp = env.ok("get_pack", json!({"id": "nature"}));
    assert_eq!(gp["assets_by_kind"]["staticmesh"], 3);
    assert_eq!(
        gp["releases"][0]["marketplace_url"],
        "https://example.invalid/pack"
    );
    let (err, _, _) = env.tool("get_pack", json!({"id": "zzzzzz-nothing"}));
    assert!(err);

    let t = env.ok("list_tags", json!({"prefix": "class:"}));
    assert!(t["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["tag"] == "class:staticmesh"));
    let t = env.ok("list_tags", json!({"limit": 5}));
    assert_eq!(t["tags"].as_array().unwrap().len(), 5);

    // No thumbnail in the fixture: a text answer, not an error.
    let (err, text, _) = env.tool("get_thumbnail", json!({"id": id}));
    assert!(!err);
    assert!(text.contains("no stored thumbnail"));
}

#[test]
fn import_requires_a_plan_and_writes_only_the_plan() {
    let mut env = env_with("5.4");
    let grass = "bbbbbbbbbbbb:/Game/Nature/Foliage/SM_Grass_01";
    let project = env.uproject.to_string_lossy().to_string();

    // No plan id: refused, nothing written.
    let (err, text, _) = env.tool(
        "import_assets",
        json!({"project": project, "asset_ids": [grass]}),
    );
    assert!(err);
    assert!(text.contains("plan_import"), "{text}");
    let (err, _, _) = env.tool(
        "import_assets",
        json!({"project": project, "asset_ids": [grass], "confirm_plan_id": "plan-99-deadbeef"}),
    );
    assert!(err);
    assert!(!proj_file(&env, "Content/Nature").exists());

    // Plan: project is on 5.4, releases are 5.3 and 5.5 -> the older one, never the newer.
    let plan = env.ok(
        "plan_import",
        json!({"project": "Test", "asset_ids": [grass]}),
    );
    let plan_id = plan["plan_id"].as_str().unwrap().to_string();
    assert_eq!(plan["packs"][0]["pack_id"], "aaaaaaaaaaaa");
    assert_eq!(plan["packs"][0]["release_choice"], "older");
    assert_eq!(plan["packs"][0]["selected_assets"], 1);
    assert_eq!(plan["packs"][0]["added_as_dependencies"], 1);
    assert_eq!(plan["packs"][0]["files"], 4);
    assert_eq!(plan["totals"]["existing_files_in_project"], 0);
    assert!(plan["warnings"].to_string().contains("T_Missing"));
    // A plan writes nothing.
    assert!(!proj_file(&env, "Content/Nature").exists());

    // Different ids than planned: refused.
    let (err, text, _) = env.tool(
        "import_assets",
        json!({"project": project, "asset_ids": ["bbbbbbbbbbbb:/Game/Nature/Rocks/SM_Rock_Big"], "confirm_plan_id": plan_id}),
    );
    assert!(err, "{text}");
    assert!(text.contains("differ"), "{text}");
    assert!(!proj_file(&env, "Content/Nature").exists());

    // The real thing.
    let done = env.ok(
        "import_assets",
        json!({"project": project, "asset_ids": [grass], "confirm_plan_id": plan_id}),
    );
    assert_eq!(done["ok"], true);
    assert_eq!(done["packs"][0]["written"], 4);
    let wp = done["packs"][0]["written_paths"].as_array().unwrap();
    assert_eq!(wp.len(), 4);
    let root_fwd = env.proj.to_string_lossy().replace('\\', "/");
    for p in wp {
        let p = p.as_str().unwrap();
        assert!(!p.contains('\\'), "{p}");
        assert!(p.starts_with(&root_fwd), "{p} vs {root_fwd}");
        assert!(Path::new(p).is_file());
    }
    assert_eq!(
        done["packs"][0]["backup_paths"].as_array().unwrap().len(),
        0
    );
    assert_eq!(
        plan["packs"][0]["paths_to_write"].as_array().unwrap().len(),
        4
    );
    let g = proj_file(&env, "Content/Nature/Foliage/SM_Grass_01.uasset");
    assert!(g.is_file());
    assert!(proj_file(&env, "Content/Nature/Foliage/SM_Grass_01.uexp").is_file());
    assert!(proj_file(&env, "Content/Nature/Materials/M_Grass.uasset").is_file());
    // Only the plan: the rock was not touched; the content is from the 5.3 release.
    assert!(!proj_file(&env, "Content/Nature/Rocks").exists());
    assert!(fs::read_to_string(&g).unwrap().starts_with("aaaaaaaaaaaa:"));

    // Logged in the app's format.
    let log: Value = serde_json::from_str(&fs::read_to_string(&env.log).unwrap()).unwrap();
    assert_eq!(log["schema"], 1);
    assert_eq!(log["entries"][0]["source"], "local");
    assert_eq!(log["entries"][0]["asset_id"], sha('a'));
    assert_eq!(log["entries"][0]["project_name"], "Test");

    // Plan ids are single use.
    let (err, _, _) = env.tool(
        "import_assets",
        json!({"project": project, "asset_ids": [grass], "confirm_plan_id": plan_id}),
    );
    assert!(err);
}

#[test]
fn existing_files_are_skipped_unless_overwrite_and_then_backed_up() {
    let mut env = env_with("5.5");
    let grass = "bbbbbbbbbbbb:/Game/Nature/Foliage/SM_Grass_01";
    let project = env.proj.to_string_lossy().to_string(); // the root path works too
    let target = proj_file(&env, "Content/Nature/Foliage/SM_Grass_01.uasset");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "mine").unwrap();

    // Project on 5.5: exact match picks the 5.5 release.
    let plan = env.ok(
        "plan_import",
        json!({"project": project, "asset_ids": [grass], "include_deps": false}),
    );
    assert_eq!(plan["packs"][0]["pack_id"], "bbbbbbbbbbbb");
    assert_eq!(plan["packs"][0]["release_choice"], "exact");
    assert_eq!(plan["totals"]["existing_files_in_project"], 1);
    let id = plan["plan_id"].as_str().unwrap();
    let r = env.ok("import_assets", json!({"project": project, "asset_ids": [grass], "confirm_plan_id": id, "include_deps": false}));
    assert_eq!(
        r["packs"][0]["skipped_existing"].as_array().unwrap().len(),
        1
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "mine");
    // The .uexp companion did not exist and was written.
    assert!(proj_file(&env, "Content/Nature/Foliage/SM_Grass_01.uexp").is_file());

    let plan = env.ok(
        "plan_import",
        json!({"project": project, "asset_ids": [grass], "include_deps": false}),
    );
    let id = plan["plan_id"].as_str().unwrap();
    let r = env.ok(
        "import_assets",
        json!({"project": project, "asset_ids": [grass], "confirm_plan_id": id, "overwrite": true}),
    );
    assert!(r["packs"][0]["backed_up"].as_u64().unwrap() >= 1);
    let bp = r["packs"][0]["backup_paths"].as_array().unwrap();
    assert!(!bp.is_empty());
    assert!(bp[0].as_str().unwrap().contains("Saved/EAM_Backups"));
    assert!(fs::read_to_string(&target)
        .unwrap()
        .starts_with("bbbbbbbbbbbb:"));
    let backups = proj_file(&env, "Saved/EAM_Backups");
    assert!(backups.is_dir());
}

#[test]
fn paths_outside_registered_projects_are_refused() {
    let mut env = env_with("5.4");
    let grass = "aaaaaaaaaaaa:/Game/Nature/Foliage/SM_Grass_01";
    // An unregistered project that does exist and holds a .uproject.
    let other = env._tmp.dir("other");
    make_project(&other, "5.4");
    for bad in [
        other.to_string_lossy().to_string(),
        other.join("Test.uproject").to_string_lossy().to_string(),
        format!("{}/../other", env.proj.to_string_lossy()),
        format!("{}/Content/../../other", env.proj.to_string_lossy()),
        "C:/Windows".to_string(),
        "NoSuchProject".to_string(),
    ] {
        let (err, text, _) = env.tool("plan_import", json!({"project": bad, "asset_ids": [grass]}));
        assert!(err, "{bad} was accepted: {text}");
        assert!(text.contains("registered"), "{text}");
    }
    assert!(!other.join("Content/Nature").exists());
    let (err, _, _) = env.tool(
        "import_assets",
        json!({"project": other.to_string_lossy(), "asset_ids": [grass], "confirm_plan_id": "plan-1-00000000"}),
    );
    assert!(err);
    let p = env.ok("list_projects", json!({}));
    assert_eq!(p["projects"].as_array().unwrap().len(), 1);
    assert_eq!(p["projects"][0]["engine"], "5.4");
}

#[test]
fn exclude_packs_and_the_mesh_umbrella_through_the_tools() {
    let mut env = env_with("5.4");
    let all = env.ok("search_assets", json!({"query": "door"}));
    assert!(all["total"].as_u64().unwrap() >= 1);
    let none = env.ok(
        "search_assets",
        json!({"query": "door", "exclude_packs": ["castle-kit"]}),
    );
    assert_eq!(none["total"], 0);
    let some = env.ok(
        "search_assets",
        json!({"query": "", "exclude_packs": "castle-kit", "kind": "mesh"}),
    );
    assert_eq!(some["total"], 3); // grass, rock, bush: static meshes via the `mesh` umbrella
    let st = env.ok("search_assets", json!({"query": "", "kind": "staticmesh"}));
    let sk = env.ok(
        "search_assets",
        json!({"query": "", "kind": "skeletalmesh"}),
    );
    assert_eq!(st["total"], 6);
    assert_eq!(sk["total"], 0);
    let t = env.ok("list_tags", json!({"prefix": "type:"}));
    assert!(t["tags"].to_string().contains("type:staticmesh"));
    assert!(!t["tags"].to_string().contains("type:mesh\""));
}

#[test]
fn refresh_picks_up_new_packs() {
    let mut env = env_with("5.4");
    let before = env.ok("search_assets", json!({"query": "lantern"}));
    assert_eq!(before["total"], 0);
    make_pack(
        &env.lib,
        "Lantern Pack",
        'e',
        "5.4",
        Kind::Content,
        &[pk("/Game/Lantern/SM_Lantern", "StaticMesh", &[])],
        true,
    );
    let r = env.ok("refresh_catalog", json!({}));
    assert!(r["stats"]["assets"].as_u64().unwrap() >= 8);
    let after = env.ok("search_assets", json!({"query": "lantern"}));
    assert_eq!(after["total"], 1);
}

/// Needs the real share; run with `cargo test -p ue_asset_catalog real_library -- --ignored --nocapture`.
/// `UE_ASSET_LIBRARY` overrides the default share path.
#[test]
#[ignore]
fn real_library() {
    let dir = std::env::var("UE_ASSET_LIBRARY")
        .unwrap_or_else(|_| "//scott-server/ue_assets_library/library".to_string());
    let mut app = App::new(AppConfig {
        libraries: vec![PathBuf::from(dir)],
        load: LoadOptions {
            build_missing: std::env::var("UE_ASSET_BUILD").is_ok(),
            cache_path: None,
            threads: 4,
            ..LoadOptions::default()
        },
        ..AppConfig::default()
    });
    ue_asset_catalog::selftest::run(&mut app, &mut std::io::stdout()).unwrap();
}

#[test]
fn unreadable_packages_are_flagged_in_get_asset_and_plans() {
    let mut env = env_with("5.4");
    let overlay = env.lib.join("nature-pack.aaaaaaaaaaaa.zip.assets.json");
    let mut idx: AssetIndex = serde_json::from_slice(&fs::read(&overlay).unwrap()).unwrap();
    for p in &mut idx.packages {
        if p.game_path.ends_with("SM_Rock_Big") {
            p.header_unreadable = true;
            p.unreadable_reason = Some("bad_magic".into());
        }
    }
    idx.engine_version_min = Some("5.3".into());
    fs::write(&overlay, serde_json::to_vec(&idx).unwrap()).unwrap();
    env.ok("refresh_catalog", json!({}));
    let id = "aaaaaaaaaaaa:/Game/Nature/Rocks/SM_Rock_Big";
    let d = env.ok("get_asset", json!({"id": id}));
    assert_eq!(d["unreadable_reason"], "bad_magic");
    assert!(d["warnings"][0].as_str().unwrap().contains("bad_magic"));
    let s = env.ok(
        "search_assets",
        json!({"query": "rock unreadable:bad_magic"}),
    );
    assert_eq!(s["total"], 1);
    let plan = env.ok("plan_import", json!({"project": "Test", "asset_ids": [id]}));
    assert!(plan["warnings"].to_string().contains("bad_magic"), "{plan}");
    let gp = env.ok("get_pack", json!({"id": "aaaaaaaaaaaa"}));
    assert_eq!(gp["releases"][0]["engine_source"], "sidecar");
}
