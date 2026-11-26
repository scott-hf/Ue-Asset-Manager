//! Hosted bundles over real TCP: `prepare_import` builds a verified zip, `GET /bundles/<id>`
//! serves it (repeatably until its TTL), and neither blocks a search.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use ue_asset_catalog::bundles::BundleConfig;
use ue_asset_catalog::catalog::{Catalog, LoadOptions};
use ue_asset_catalog::http_server::{self, Limits, Running, StartOptions};
use ue_asset_catalog::tools::{load_catalog, App, AppConfig};
use ue_asset_library::asset_index::write_index;
use ue_asset_library::{AssetIndex, Kind, LibraryEntry, PackageInfo, Sidecar, INDEX_SCHEMA};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const TOKEN: &str = "bundle-token-0123456789";

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
            "ue_asset_catalog_bundles/{}-{}-{}",
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

/// Deterministic, hard to compress bytes.
fn body(seed: u8, n: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9u32 ^ u32::from(seed).wrapping_mul(0x0101_0101);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

struct Pkg {
    game_path: &'static str,
    main: Vec<u8>,
    /// (extension, bytes) of companion files.
    companions: Vec<(&'static str, Vec<u8>)>,
    deps: Vec<&'static str>,
    external: Vec<&'static str>,
}

fn pkg(game_path: &'static str, seed: u8) -> Pkg {
    Pkg {
        game_path,
        main: body(seed, 3000),
        companions: Vec::new(),
        deps: Vec::new(),
        external: Vec::new(),
    }
}

/// A library pack (zip + sidecar + index overlay); returns the zip path.
fn make_pack(lib: &Path, name: &str, sha_char: char, pkgs: &[Pkg]) -> PathBuf {
    let sha = format!("{}{}", sha_char.to_string().repeat(12), "0".repeat(52));
    let sha12 = &sha[..12];
    let slug = name.to_lowercase().replace(' ', "-");
    let zip_path = lib.join(format!("{slug}.{sha12}.zip"));
    let mut w = ZipWriter::new(fs::File::create(&zip_path).unwrap());
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut infos = Vec::new();
    let mut count = 0u64;
    let mut total = 0u64;
    let mut first_folder = String::new();
    for p in pkgs {
        let rel = p.game_path.trim_start_matches("/Game/");
        if first_folder.is_empty() {
            first_folder = rel.split('/').next().unwrap().to_string();
        }
        let main = format!("Content/{rel}.uasset");
        w.start_file(main.clone(), opts).unwrap();
        w.write_all(&p.main).unwrap();
        count += 1;
        total += p.main.len() as u64;
        let mut size = p.main.len() as u64;
        let mut companions = Vec::new();
        for (ext, bytes) in &p.companions {
            let c = format!("Content/{rel}.{ext}");
            w.start_file(c.clone(), opts).unwrap();
            w.write_all(bytes).unwrap();
            count += 1;
            total += bytes.len() as u64;
            size += bytes.len() as u64;
            companions.push(c);
        }
        infos.push(PackageInfo {
            game_path: p.game_path.to_string(),
            zip_path: main,
            companions,
            class: Some("StaticMesh".to_string()),
            size,
            deps: p.deps.iter().map(|d| (*d).to_string()).collect(),
            external: p.external.iter().map(|d| (*d).to_string()).collect(),
            has_thumbnail: false,
            header_unreadable: false,
            unreadable_reason: None,
        });
    }
    w.finish().unwrap();
    let sidecar = Sidecar {
        schema: 1,
        kind: Kind::Content,
        display_name: name.to_string(),
        source_zip: format!("{name}.zip"),
        sha256: sha.clone(),
        engine_version_guess: Some("5.4".to_string()),
        content_folder: Some(first_folder.clone()),
        plugin_name: None,
        zip_prefix: String::new(),
        game_prefix: String::new(),
        allowed_roots: vec![format!("Content/{first_folder}")],
        entry_count: count,
        total_uncompressed: total,
        external_refs: Vec::new(),
        sanitized_at: "2026-01-01T00:00:00Z".to_string(),
        marketplace_url: None,
        friendly_name: None,
        extra: serde_json::Map::new(),
    };
    let sidecar_path = lib.join(format!("{slug}.{sha12}.zip.json"));
    fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
    let entry = LibraryEntry {
        sidecar,
        zip_path: zip_path.clone(),
        sidecar_path,
    };
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
    zip_path
}

const GRASS_01: &str = "aaaaaaaaaaaa:/Game/Grass/Meshes/SM_Grass_01";
const GRASS_02: &str = "aaaaaaaaaaaa:/Game/Grass/Meshes/SM_Grass_02";
const HUGE: &str = "eeeeeeeeeeee:/Game/Big/SM_Huge";
const CLASH_A: &str = "cccccccccccc:/Game/Shared/SM_X";
const CLASH_B: &str = "dddddddddddd:/Game/Shared/SM_X";
const CLASH_C: &str = "ffffffffffff:/Game/Shared/SM_X";

fn build_library(lib: &Path) -> PathBuf {
    let mut sm1 = pkg("/Game/Grass/Meshes/SM_Grass_01", 1);
    sm1.companions = vec![("uexp", body(11, 100))];
    sm1.deps = vec!["/Game/Grass/Materials/M_Grass"];
    sm1.external = vec!["/Game/Engine2/Thing"];
    let mut mat = pkg("/Game/Grass/Materials/M_Grass", 2);
    mat.companions = vec![("uexp", body(12, 100))];
    mat.deps = vec!["/Game/Grass/Textures/T_Grass"];
    let mut tex = pkg("/Game/Grass/Textures/T_Grass", 3);
    tex.companions = vec![("ubulk", body(13, 100))];
    let sm2 = pkg("/Game/Grass/Meshes/SM_Grass_02", 4);
    let grass = make_pack(lib, "Grass Pack", 'a', &[sm1, mat, tex, sm2]);

    let mut huge = pkg("/Game/Big/SM_Huge", 5);
    huge.companions = vec![("ubulk", body(14, 6 * 1024 * 1024))];
    make_pack(lib, "Big Pack", 'e', &[huge]);

    let mut a = pkg("/Game/Shared/SM_X", 6);
    a.main = b"AAAA".to_vec();
    make_pack(lib, "Clash A", 'c', &[a]);
    let mut b = pkg("/Game/Shared/SM_X", 7);
    b.main = b"BBBB".to_vec();
    make_pack(lib, "Clash B", 'd', &[b]);
    let mut c = pkg("/Game/Shared/SM_X", 8);
    c.main = b"AAAA".to_vec();
    make_pack(lib, "Clash C", 'f', &[c]);
    grass
}

struct Hosted {
    tmp: Tmp,
    lib: PathBuf,
    bundle_dir: PathBuf,
    grass_zip: PathBuf,
    addr: SocketAddr,
    _running: Option<Running>,
}

impl Hosted {
    fn start(tune: impl FnOnce(&mut BundleConfig)) -> Hosted {
        Hosted::start_with(|_| {}, tune)
    }

    /// `pre` runs on the bundle folder before the server opens it.
    fn start_with(pre: impl FnOnce(&Path), tune: impl FnOnce(&mut BundleConfig)) -> Hosted {
        let tmp = Tmp::new();
        let lib = tmp.dir("lib");
        let grass_zip = build_library(&lib);
        let bundle_dir = tmp.dir("bundles");
        pre(&bundle_dir);
        let mut bc = BundleConfig::new(bundle_dir.clone());
        bc.sweep_interval = Duration::from_millis(200);
        tune(&mut bc);
        let cfg = AppConfig {
            libraries: vec![lib.clone()],
            load: LoadOptions {
                build_missing: false,
                cache_path: None,
                threads: 2,
                ..LoadOptions::default()
            },
            read_only: true,
            no_auto_refresh: true,
            bundle: Some(bc),
            ..AppConfig::default()
        };
        let loader_cfg = cfg.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let app = App::with_catalog(Catalog::empty(), cfg);
        let running = http_server::start(
            listener,
            app,
            Box::new(move || load_catalog(&loader_cfg)),
            StartOptions {
                token: Some(TOKEN.as_bytes().to_vec()),
                allowed_hosts: Vec::new(),
                limits: Limits::default(),
            },
        )
        .unwrap();
        let h = Hosted {
            tmp,
            lib,
            bundle_dir,
            grass_zip,
            addr: running.local_addr,
            _running: Some(running),
        };
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if h.get("/health", &[]).json()["status"] == "ok" {
                break;
            }
            assert!(Instant::now() < end, "catalog never became ready");
            std::thread::sleep(Duration::from_millis(25));
        }
        h
    }

    fn host(&self) -> String {
        self.addr.to_string()
    }

    fn get(&self, path: &str, extra: &[(&str, &str)]) -> Reply {
        let mut s = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n", self.host());
        for (k, v) in extra {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str("\r\n");
        raw_request(self.addr, s.as_bytes())
    }

    fn get_bundle(&self, id: &str) -> Reply {
        let auth = format!("Bearer {TOKEN}");
        self.get(&format!("/bundles/{id}"), &[("Authorization", &auth)])
    }

    fn post(&self, body: &str, auth: bool) -> Reply {
        let mut s = format!("POST /mcp HTTP/1.1\r\nHost: {}\r\n", self.host());
        if auth {
            s.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
        }
        s.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        raw_request(self.addr, s.as_bytes())
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let r = self.post(&body.to_string(), true);
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    fn call(&self, name: &str, args: Value) -> Value {
        self.rpc("tools/call", json!({"name": name, "arguments": args}))
    }

    fn prepare(&self, ids: &[&str], deps: Option<bool>) -> Value {
        let mut a = json!({"asset_ids": ids, "project_engine_version": "5.4"});
        if let Some(d) = deps {
            a["include_deps"] = json!(d);
        }
        self.call("prepare_import", a)
    }

    fn bundle_files(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.bundle_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }
}

struct Reply {
    status: u16,
    head: String,
    bytes: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes)
            .unwrap_or_else(|e| panic!("not json ({e}): {}", self.text()))
    }
    fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.head.lines().find_map(|l| {
            l.to_ascii_lowercase()
                .starts_with(&want)
                .then(|| l[want.len()..].trim().to_string())
        })
    }
}

fn parse_reply(buf: &[u8]) -> Reply {
    let at = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..at]).to_string();
    let bytes = if at + 4 <= buf.len() {
        buf[at + 4..].to_vec()
    } else {
        Vec::new()
    };
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        head,
        bytes,
    }
}

fn raw_request(addr: SocketAddr, req: &[u8]) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(req).unwrap();
    let _ = s.shutdown(Shutdown::Write);
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    parse_reply(&buf)
}

fn tool_text(r: &Value) -> String {
    r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// The JSON result of a tool call that must have succeeded.
fn ok(r: &Value) -> Value {
    assert_eq!(r["result"]["isError"], false, "{}", tool_text(r));
    serde_json::from_str(&tool_text(r)).unwrap()
}

/// The error text of a tool call that must have failed.
fn err(r: &Value) -> String {
    assert_eq!(r["result"]["isError"], true, "{}", tool_text(r));
    tool_text(r)
}

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn zip_entry(z: &mut ZipArchive<impl Read + std::io::Seek>, name: &str) -> Vec<u8> {
    let mut v = Vec::new();
    z.by_name(name).unwrap().read_to_end(&mut v).unwrap();
    v
}

#[test]
fn bundle_is_built_served_repeatably_and_verifies_against_the_source() {
    let h = Hosted::start(|_| {});
    let r = ok(&h.prepare(&[GRASS_01], None));
    let id = r["bundle_id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(r["download"], format!("/bundles/{id}"));
    assert_eq!(r["files_total"], 6);
    assert_eq!(r["verdict"], "exact");
    assert_eq!(r["packs"][0]["pack_id"], "aaaaaaaaaaaa");
    assert_eq!(r["omitted_deps_total"], 0);
    assert_eq!(r["external_missing"], json!(["/Game/Engine2/Thing"]));
    assert!(r["expires_at"].as_str().unwrap().ends_with('Z'));
    assert!(r.get("plan_id").is_none());
    assert_eq!(h.get("/health", &[]).json()["bundles_pending"], 1);

    for round in 0..2 {
        let g = h.get_bundle(&id);
        assert_eq!(g.status, 200, "round {round}: {}", g.head);
        assert_eq!(g.header("content-type").as_deref(), Some("application/zip"));
        assert_eq!(g.header("x-bundle-id").as_deref(), Some(id.as_str()));
        assert_eq!(
            g.header("content-length").unwrap(),
            r["size_bytes"].as_u64().unwrap().to_string()
        );
        let sha = g.header("x-bundle-sha256").unwrap();
        assert_eq!(sha, r["bundle_sha256"].as_str().unwrap());
        assert_eq!(sha, sha_hex(&g.bytes));

        let mut bundle = ZipArchive::new(Cursor::new(g.bytes.clone())).unwrap();
        let manifest: Value =
            serde_json::from_slice(&zip_entry(&mut bundle, "manifest.json")).unwrap();
        assert_eq!(manifest["format"], 1);
        assert_eq!(manifest["bundle_id"], id.as_str());
        assert_eq!(manifest["project_engine_version"], "5.4");
        assert_eq!(manifest["engine_verdict"]["verdict"], "exact");
        assert_eq!(
            manifest["total_uncompressed_bytes"],
            r["uncompressed_bytes"]
        );
        let files = manifest["files"].as_array().unwrap();
        let mut names: Vec<String> = bundle.file_names().map(str::to_string).collect();
        names.sort();
        let mut want: Vec<String> = files
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        want.push("manifest.json".to_string());
        want.sort();
        assert_eq!(
            names, want,
            "zip entries are the manifest files plus manifest.json"
        );
        let mut source = ZipArchive::new(fs::File::open(&h.grass_zip).unwrap()).unwrap();
        for f in files {
            let path = f["path"].as_str().unwrap();
            assert!(path.starts_with("Content/"), "{path}");
            let got = zip_entry(&mut bundle, path);
            assert_eq!(got, zip_entry(&mut source, path), "{path}");
            assert_eq!(f["sha256"], sha_hex(&got).as_str(), "{path}");
            assert_eq!(f["size"], got.len());
        }
        assert_eq!(files.len(), 6);
    }
}

#[test]
fn without_deps_the_omitted_ones_are_reported() {
    let h = Hosted::start(|_| {});
    let r = ok(&h.prepare(&[GRASS_01], Some(false)));
    assert_eq!(r["files_total"], 2);
    assert_eq!(
        r["omitted_deps"],
        json!(["aaaaaaaaaaaa:/Game/Grass/Materials/M_Grass"])
    );
    assert_eq!(r["omitted_deps_total"], 1);
    assert!(r["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("dependenc")));
    let g = h.get_bundle(r["bundle_id"].as_str().unwrap());
    let mut z = ZipArchive::new(Cursor::new(g.bytes)).unwrap();
    let manifest: Value = serde_json::from_slice(&zip_entry(&mut z, "manifest.json")).unwrap();
    assert_eq!(manifest["omitted_deps"], r["omitted_deps"]);
    assert_eq!(manifest["external_missing"], json!(["/Game/Engine2/Thing"]));
}

#[test]
fn a_bundle_expires_after_its_ttl_and_is_swept() {
    let h = Hosted::start(|c| c.ttl = Duration::from_secs(2));
    let r = ok(&h.prepare(&[GRASS_02], None));
    let id = r["bundle_id"].as_str().unwrap().to_string();
    assert_eq!(h.get_bundle(&id).status, 200);
    assert_eq!(h.get_bundle(&id).status, 200, "repeat GET before the TTL");
    assert!(h.bundle_files().contains(&format!("{id}.zip")));
    std::thread::sleep(Duration::from_millis(2600));
    let g = h.get_bundle(&id);
    assert_eq!(g.status, 410, "{}", g.text());
    assert_eq!(g.json()["error"]["code"], "BUNDLE_EXPIRED");
    assert_eq!(h.get("/health", &[]).json()["bundles_pending"], 0);
    assert!(
        !h.bundle_files().contains(&format!("{id}.zip")),
        "the sweep removed the file: {:?}",
        h.bundle_files()
    );
    // Unknown ids, valid or not: 404.
    assert_eq!(h.get_bundle(&"0".repeat(32)).status, 404);
    assert_eq!(h.get_bundle("abc").status, 404);
    assert_eq!(h.get_bundle("..%2F..%2Fsecret").status, 404);
}

#[test]
fn caps_name_the_size_and_the_cap() {
    // Per bundle.
    let h = Hosted::start(|c| c.max_bundle_bytes = 1000);
    let e = err(&h.prepare(&[GRASS_01], None));
    assert!(e.starts_with("error: CAP_EXCEEDED:"), "{e}");
    assert!(e.contains("cap is 1000"), "{e}");
    assert!(h.bundle_files().is_empty(), "{:?}", h.bundle_files());

    // Cache budget (reserved before a build).
    let h = Hosted::start(|c| c.max_cache_bytes = 1024 * 1024);
    let e = err(&h.prepare(&[GRASS_01], None));
    assert!(e.starts_with("error: BUSY:"), "{e}");
    assert!(e.contains("cache"), "{e}");

    // Rolling hour: the third bundle of ~9.4 KB does not fit into 20 KB.
    let h = Hosted::start(|c| c.max_hour_bytes = 20_000);
    ok(&h.prepare(&[GRASS_01], None));
    ok(&h.prepare(&[GRASS_01], None));
    let e = err(&h.prepare(&[GRASS_01], None));
    assert!(e.starts_with("error: CAP_EXCEEDED:"), "{e}");
    assert!(e.contains("hourly") && e.contains("20000"), "{e}");
}

#[test]
fn a_second_prepare_is_busy_and_search_answers_while_a_bundle_builds() {
    let h = Hosted::start(|c| c.build_delay = Duration::from_millis(1500));
    std::thread::scope(|s| {
        let first = s.spawn(|| h.prepare(&[GRASS_01], None));
        std::thread::sleep(Duration::from_millis(400));
        // The one build slot is taken.
        let e = err(&h.prepare(&[GRASS_02], None));
        assert!(e.starts_with("error: BUSY:"), "{e}");
        // The catalog lock is free: a search answers long before the build ends.
        let t = Instant::now();
        let r = ok(&h.call("search_assets", json!({"query": "grass"})));
        assert!(r["total"].as_u64().unwrap() >= 1);
        assert!(
            t.elapsed() < Duration::from_millis(700),
            "search waited for the build: {:?}",
            t.elapsed()
        );
        let done = ok(&first.join().unwrap());
        assert_eq!(done["files_total"], 6);
    });
    // The slot is free again.
    ok(&h.prepare(&[GRASS_02], None));
}

#[test]
fn a_slow_download_does_not_block_search_and_completes_intact() {
    let h = Hosted::start(|_| {});
    let r = ok(&h.prepare(&[HUGE], None));
    let id = r["bundle_id"].as_str().unwrap();
    let size = r["size_bytes"].as_u64().unwrap() as usize;
    assert!(size > 6 * 1024 * 1024);

    let auth = format!("Bearer {TOKEN}");
    let mut s = TcpStream::connect(h.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(
        format!(
            "GET /bundles/{id} HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\n\r\n",
            h.host()
        )
        .as_bytes(),
    )
    .unwrap();
    // Read a little, then stop reading: the server is now blocked inside the stream.
    let mut first = vec![0u8; 1024];
    let n = s.read(&mut first).unwrap();
    first.truncate(n);
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    let found = ok(&h.call("search_assets", json!({"query": "huge"})));
    assert_eq!(found["total"], 1);
    assert!(
        t.elapsed() < Duration::from_millis(700),
        "search waited for the download: {:?}",
        t.elapsed()
    );
    // Resume: the download completes and is byte-exact.
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).unwrap();
    first.extend_from_slice(&rest);
    let reply = parse_reply(&first);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.bytes.len(), size);
    assert_eq!(sha_hex(&reply.bytes), r["bundle_sha256"].as_str().unwrap());
}

#[test]
fn auth_is_required_and_the_server_stays_read_only() {
    let h = Hosted::start(|_| {});
    let r = ok(&h.prepare(&[GRASS_02], None));
    let id = r["bundle_id"].as_str().unwrap();
    // No bearer / wrong bearer on a real bundle: 401, nothing leaks.
    let none = h.get(&format!("/bundles/{id}"), &[]);
    assert_eq!(none.status, 401);
    assert!(none.header("www-authenticate").is_some());
    let wrong = h.get(
        &format!("/bundles/{id}"),
        &[("Authorization", "Bearer nope")],
    );
    assert_eq!(wrong.status, 401);
    assert!(!wrong.bytes.starts_with(b"PK"));
    // prepare_import over /mcp needs the bearer too.
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"prepare_import","arguments":{"asset_ids":[GRASS_02],"project_engine_version":"5.4"}}});
    assert_eq!(h.post(&body.to_string(), false).status, 401);

    // The tool list has prepare_import but no write tool; write tools are refused.
    let list = h.rpc("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"prepare_import"), "{names:?}");
    for gone in [
        "import_assets",
        "plan_import",
        "list_projects",
        "refresh_catalog",
    ] {
        assert!(!names.contains(&gone), "{gone}");
        let e = err(&h.call(gone, json!({})));
        assert!(e.contains("READ_ONLY"), "{gone}: {e}");
    }
    let init = h.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    let text = init["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("prepare_import") && !text.contains("plan_import"));

    // Bad arguments are plain tool errors.
    assert!(err(&h.call("prepare_import", json!({}))).contains("asset_ids"));
    assert!(err(&h.call(
        "prepare_import",
        json!({"asset_ids": [GRASS_01], "project_engine_version": "x"})
    ))
    .contains("not a version"));
    assert!(err(&h.call(
        "prepare_import",
        json!({"asset_ids": ["aaaaaaaaaaaa:/Game/Nope"], "project_engine_version": "5.4"})
    ))
    .starts_with("error: NOT_FOUND:"));
    // Only the bundle folder was ever written (and only this bundle is in it).
    assert_eq!(h.bundle_files(), vec![format!("{id}.zip")]);
    let _ = (&h.tmp, &h.lib);
}

#[test]
fn packs_that_disagree_are_a_conflict_and_identical_paths_are_deduplicated() {
    let h = Hosted::start(|_| {});
    let e = err(&h.prepare(&[CLASH_A, CLASH_B], None));
    assert!(e.starts_with("error: CONFLICT:"), "{e}");
    assert!(e.contains("Content/Shared/SM_X.uasset"), "{e}");
    // Nothing is left behind and the slot is free.
    assert!(h.bundle_files().is_empty(), "{:?}", h.bundle_files());
    let r = ok(&h.prepare(&[CLASH_A, CLASH_C], None));
    assert_eq!(r["files_total"], 1);
    assert_eq!(r["packs"].as_array().unwrap().len(), 2);
    let g = h.get_bundle(r["bundle_id"].as_str().unwrap());
    let mut z = ZipArchive::new(Cursor::new(g.bytes)).unwrap();
    assert_eq!(zip_entry(&mut z, "Content/Shared/SM_X.uasset"), b"AAAA");
}

#[test]
fn startup_removes_only_stale_bundle_files() {
    let id = "0123456789abcdef0123456789abcdef";
    let h = Hosted::start_with(
        |dir| {
            fs::write(dir.join(format!(".tmp-{id}")), b"half").unwrap();
            fs::write(dir.join(format!("{id}.zip")), b"old").unwrap();
            fs::write(dir.join("notes.txt"), b"keep").unwrap();
            fs::write(dir.join("other.zip"), b"keep").unwrap();
        },
        |_| {},
    );
    assert_eq!(h.bundle_files(), vec!["notes.txt", "other.zip"]);
    // The stale id is not served.
    assert_eq!(h.get_bundle(id).status, 404);
}

fn run_bin(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ue_asset_mcp"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn the_bundle_folder_must_be_apart_from_the_library() {
    let tmp = Tmp::new();
    let lib = tmp.dir("lib");
    let token = tmp.0.join("t.token");
    fs::write(&token, TOKEN).unwrap();
    let lib_s = lib.to_string_lossy().to_string();
    let tok_s = token.to_string_lossy().to_string();
    let inside = lib.join("bundles").to_string_lossy().to_string();
    let (code, err) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "127.0.0.1:0",
        "--token-file",
        &tok_s,
        "--bundle-dir",
        &inside,
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("apart from the library"), "{err}");
    // The library itself, and a parent of it, are refused too.
    let (code, _) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "127.0.0.1:0",
        "--token-file",
        &tok_s,
        "--bundle-dir",
        &lib_s,
    ]);
    assert_eq!(code, Some(1));
    let parent = tmp.0.to_string_lossy().to_string();
    let (code, _) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "127.0.0.1:0",
        "--token-file",
        &tok_s,
        "--bundle-dir",
        &parent,
    ]);
    assert_eq!(code, Some(1));
    // Bundle flags without --http are a usage error.
    let (code, err) = run_bin(&["--library", &lib_s, "--bundle-dir", &parent]);
    assert_eq!(code, Some(1));
    assert!(err.contains("need --http"), "{err}");
}
