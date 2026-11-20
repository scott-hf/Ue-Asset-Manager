//! The read-only HTTP transport over real TCP on 127.0.0.1 with a synthetic library.

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};
use ue_asset_catalog::catalog::{Catalog, LoadOptions};
use ue_asset_catalog::http_server::{self, Limits, Running, StartOptions};
use ue_asset_catalog::tools::{load_catalog, App, AppConfig};
use ue_asset_library::asset_index::write_index;
use ue_asset_library::{AssetIndex, Kind, LibraryEntry, PackageInfo, Sidecar, INDEX_SCHEMA};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const TOKEN: &str = "test-token-0123456789";

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
            "ue_asset_catalog_http/{}-{}-{}",
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

fn make_pack(lib: &Path, name: &str, sha_char: char, games: &[(&str, &str)]) {
    let sha = format!("{}{}", sha_char.to_string().repeat(12), "0".repeat(52));
    let sha12 = &sha[..12];
    let slug = name.to_lowercase().replace(' ', "-");
    let zip_path = lib.join(format!("{slug}.{sha12}.zip"));
    let mut w = ZipWriter::new(fs::File::create(&zip_path).unwrap());
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut infos = Vec::new();
    let mut count = 0u64;
    let mut first_folder = String::new();
    for (game_path, class) in games {
        let rel = game_path.trim_start_matches("/Game/");
        if first_folder.is_empty() {
            first_folder = rel.split('/').next().unwrap().to_string();
        }
        let main = format!("Content/{rel}.uasset");
        w.start_file(main.clone(), opts).unwrap();
        w.write_all(game_path.as_bytes()).unwrap();
        count += 1;
        infos.push(PackageInfo {
            game_path: (*game_path).to_string(),
            zip_path: main,
            companions: Vec::new(),
            class: Some((*class).to_string()),
            size: game_path.len() as u64,
            deps: Vec::new(),
            external: Vec::new(),
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
        total_uncompressed: 10,
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
        zip_path,
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
}

fn build_library(lib: &Path) {
    make_pack(
        lib,
        "Nature Pack",
        'a',
        &[
            ("/Game/Nature/Foliage/SM_Grass_01", "StaticMesh"),
            ("/Game/Nature/Rocks/SM_Rock_Big", "StaticMesh"),
        ],
    );
    make_pack(
        lib,
        "Castle Kit",
        'c',
        &[("/Game/Castle/Doors/SM_Door", "StaticMesh")],
    );
}

fn app_config(lib: &Path) -> AppConfig {
    AppConfig {
        libraries: vec![lib.to_path_buf()],
        load: LoadOptions {
            build_missing: false,
            cache_path: None,
            threads: 2,
            ..LoadOptions::default()
        },
        read_only: true,
        no_auto_refresh: true,
        ..AppConfig::default()
    }
}

struct Hosted {
    tmp: Tmp,
    lib: PathBuf,
    _running: Option<Running>,
    addr: SocketAddr,
    gate: Option<Sender<()>>,
}

impl Hosted {
    fn start() -> Hosted {
        Hosted::start_with(false, Limits::default())
    }

    /// `gated`: the first catalog load waits until `release()`.
    fn start_with(gated: bool, limits: Limits) -> Hosted {
        let tmp = Tmp::new();
        let lib = tmp.dir("lib");
        build_library(&lib);
        let cfg = app_config(&lib);
        let (tx, rx) = channel::<()>();
        let loader_cfg = cfg.clone();
        let loader = Box::new(move || {
            if gated {
                let _ = rx.recv();
            }
            load_catalog(&loader_cfg)
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let app = App::with_catalog(Catalog::empty(), cfg);
        let running = http_server::start(
            listener,
            app,
            loader,
            StartOptions {
                token: TOKEN.as_bytes().to_vec(),
                allowed_hosts: Vec::new(),
                limits,
            },
        )
        .unwrap();
        let addr = running.local_addr;
        Hosted {
            tmp,
            lib,
            _running: Some(running),
            addr,
            gate: if gated { Some(tx) } else { None },
        }
        .ready_unless_gated(gated)
    }

    fn ready_unless_gated(self, gated: bool) -> Hosted {
        if !gated {
            self.wait_ready();
        }
        self
    }

    fn release(&mut self) {
        if let Some(g) = self.gate.take() {
            let _ = g.send(());
        }
    }

    fn wait_ready(&self) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            let r = self.get("/health", &[]);
            if r.json()["status"] == "ok" {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("catalog never became ready");
    }

    fn host(&self) -> String {
        self.addr.to_string()
    }

    fn raw(&self, req: &str) -> Reply {
        raw_request(self.addr, req.as_bytes())
    }

    fn get(&self, path: &str, extra: &[(&str, &str)]) -> Reply {
        let mut s = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n", self.host());
        for (k, v) in extra {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str("\r\n");
        self.raw(&s)
    }

    /// POST /mcp with the bearer token and `Content-Length`.
    fn post(&self, body: &str, extra: &[(&str, &str)]) -> Reply {
        let mut s = format!(
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            self.host(),
            body.len()
        );
        for (k, v) in extra {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str("\r\n");
        s.push_str(body);
        self.raw(&s)
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let r = self.post(&body.to_string(), &[]);
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()
    }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not json ({e}): {}", self.body))
    }
    fn has_header(&self, name: &str) -> bool {
        self.head.lines().any(|l| {
            l.to_ascii_lowercase()
                .starts_with(&name.to_ascii_lowercase())
        })
    }
}

fn parse_reply(buf: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

fn raw_request(addr: SocketAddr, req: &[u8]) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
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

#[test]
fn bearer_is_required_and_checked() {
    let h = Hosted::start();
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let none = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n{list}",
        h.host(),
        list.len()
    );
    let r = h.raw(&none);
    assert_eq!(r.status, 401);
    assert!(r.has_header("www-authenticate: bearer"));
    assert_eq!(r.json()["error"]["code"], "UNAUTHORIZED");
    assert!(!r.body.contains(TOKEN));
    let wrong = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer nope\r\nContent-Length: {}\r\n\r\n{list}",
        h.host(),
        list.len()
    );
    assert_eq!(h.raw(&wrong).status, 401);
    let basic = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {TOKEN}\r\nContent-Length: {}\r\n\r\n{list}",
        h.host(),
        list.len()
    );
    assert_eq!(h.raw(&basic).status, 401);
    let ok = h.post(list, &[]);
    assert_eq!(ok.status, 200);
    assert!(ok.has_header("connection: close"));
}

#[test]
fn tools_list_has_no_write_tools_and_calls_are_refused() {
    let h = Hosted::start();
    let r = h.rpc("tools/list", json!({}));
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "search_assets",
        "get_asset",
        "list_packs",
        "get_pack",
        "list_tags",
        "get_thumbnail",
    ] {
        assert!(names.contains(&want), "{want} missing in {names:?}");
    }
    for gone in [
        "import_assets",
        "plan_import",
        "list_projects",
        "refresh_catalog",
        "request_generation",
        "generation_status",
    ] {
        assert!(!names.contains(&gone), "{gone} must be hidden");
        let r = h.rpc("tools/call", json!({"name": gone, "arguments": {}}));
        assert_eq!(r["result"]["isError"], true, "{gone}");
        assert!(
            tool_text(&r).contains("READ_ONLY"),
            "{gone}: {}",
            tool_text(&r)
        );
    }
    let init = h.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    assert!(!init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("plan_import"));
}

#[test]
fn search_works_over_http() {
    let h = Hosted::start();
    let r = h.rpc(
        "tools/call",
        json!({"name": "search_assets", "arguments": {"query": "grass"}}),
    );
    assert_eq!(r["result"]["isError"], false);
    let v: Value = serde_json::from_str(&tool_text(&r)).unwrap();
    assert!(v["total"].as_u64().unwrap() >= 1, "{v}");
    let r = h.rpc(
        "tools/call",
        json!({"name": "search_assets", "arguments": {"query": "door"}}),
    );
    let v: Value = serde_json::from_str(&tool_text(&r)).unwrap();
    assert!(v["total"].as_u64().unwrap() >= 1, "{v}");
}

#[test]
fn routes_methods_and_health() {
    let h = Hosted::start();
    // GET /mcp: 405 + Allow.
    let r = h.get("/mcp", &[("Authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(r.status, 405);
    assert!(r.has_header("allow: post"));
    // /health is open and carries no secrets or paths.
    let r = h.get("/health", &[]);
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["server"], "ue_asset_mcp");
    assert_eq!(v["read_only"], true);
    assert_eq!(v["packs"], 2);
    assert_eq!(v["assets"], 3);
    assert_eq!(v["catalog_generation"], 1);
    assert!(v["catalog_loaded_at"].as_str().unwrap().ends_with('Z'));
    assert!(!r.body.contains(TOKEN));
    assert!(!r.body.contains("ue_asset_catalog_http"));
    // Unknown path: 404; wrong method on /health: 405.
    assert_eq!(h.get("/nope", &[]).status, 404);
    let r = h.raw(&format!(
        "POST /health HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\n\r\n",
        h.host()
    ));
    assert_eq!(r.status, 405);
    // Bundles: auth first, then method, then (no store yet) not found.
    assert_eq!(h.get("/bundles/abc", &[]).status, 401);
    let auth = format!("Bearer {TOKEN}");
    assert_eq!(
        h.get("/bundles/abc", &[("Authorization", &auth)]).status,
        404
    );
    let r = h.raw(&format!(
        "POST /bundles/abc HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\nContent-Length: 0\r\n\r\n",
        h.host()
    ));
    assert_eq!(r.status, 405);
    assert!(r.has_header("allow: get"));
}

#[test]
fn host_and_origin_are_checked_first() {
    let h = Hosted::start();
    let auth = format!("Bearer {TOKEN}");
    let r = h.raw("GET /health HTTP/1.1\r\nHost: evil.example:80\r\n\r\n");
    assert_eq!(r.status, 403);
    let r = h.raw("GET /health HTTP/1.1\r\n\r\n");
    assert_eq!(r.status, 403);
    let r = h.get("/health", &[("Origin", "http://evil.example")]);
    assert_eq!(r.status, 403);
    let r = h.get(
        "/mcp",
        &[("Origin", "http://evil.example"), ("Authorization", &auth)],
    );
    assert_eq!(r.status, 403);
}

#[test]
fn framing_and_message_rules() {
    let h = Hosted::start();
    let auth = format!("Bearer {TOKEN}");
    // 411: no Content-Length.
    let r = h.raw(&format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\n\r\n",
        h.host()
    ));
    assert_eq!(r.status, 411);
    // 400: any Transfer-Encoding.
    let r = h.raw(&format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        h.host()
    ));
    assert_eq!(r.status, 400);
    // 413: body over 1 MiB (rejected before reading it).
    let r = h.raw(&format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\nContent-Length: 2000000\r\n\r\n",
        h.host()
    ));
    assert_eq!(r.status, 413);
    // 202: a notification has no reply.
    let r = h.post(
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        &[],
    );
    assert_eq!(r.status, 202);
    assert!(r.body.is_empty());
    // 400: a batch, and broken JSON.
    let r = h.post(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, &[]);
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], -32600);
    let r = h.post("{nope", &[]);
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], -32700);
    // 400: unknown MCP-Protocol-Version; a known one and none are accepted.
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    assert_eq!(
        h.post(ping, &[("MCP-Protocol-Version", "1999-01-01")])
            .status,
        400
    );
    assert_eq!(
        h.post(ping, &[("MCP-Protocol-Version", "2025-06-18")])
            .status,
        200
    );
    assert_eq!(h.post(ping, &[]).status, 200);
    // Header block too large.
    let big = "a".repeat(20 * 1024);
    let r = h.raw(&format!(
        "GET /health HTTP/1.1\r\nHost: {}\r\nX-Big: {big}\r\n\r\n",
        h.host()
    ));
    assert_eq!(r.status, 400);
}

#[test]
fn stalled_client_does_not_block_others() {
    let limits = Limits {
        read_timeout: Duration::from_millis(700),
        ..Limits::default()
    };
    let h = Hosted::start_with(false, limits);
    let mut stalled = TcpStream::connect(h.addr).unwrap();
    stalled.write_all(b"POST /mcp HTTP/1.1\r\nHo").unwrap();
    stalled
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let t = Instant::now();
    let r = h.rpc("tools/list", json!({}));
    assert!(r["result"]["tools"].is_array());
    assert!(
        t.elapsed() < Duration::from_millis(600),
        "blocked by the stalled client"
    );
    // The stalled connection is cut off at the read deadline with a 408.
    let mut buf = Vec::new();
    let _ = stalled.read_to_end(&mut buf);
    assert_eq!(parse_reply(&buf).status, 408);
}

#[test]
fn health_answers_while_the_catalog_loads() {
    let mut h = Hosted::start_with(true, Limits::default());
    let r = h.get("/health", &[]);
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["status"], "loading");
    assert_eq!(v["assets"], 0);
    // Protocol calls work, tool calls say BUSY.
    let l = h.rpc("tools/list", json!({}));
    assert!(l["result"]["tools"].is_array());
    let r = h.rpc(
        "tools/call",
        json!({"name": "search_assets", "arguments": {"query": "grass"}}),
    );
    assert_eq!(r["result"]["isError"], true);
    assert!(tool_text(&r).contains("BUSY"));
    h.release();
    h.wait_ready();
    let v = h.get("/health", &[]).json();
    assert_eq!(v["assets"], 3);
    let r = h.rpc(
        "tools/call",
        json!({"name": "search_assets", "arguments": {"query": "grass"}}),
    );
    assert_eq!(r["result"]["isError"], false);
}

#[test]
fn background_refresh_swaps_and_keeps_the_last_good_catalog() {
    let limits = Limits {
        refresh_interval: Duration::from_millis(150),
        min_reload_gap: Duration::ZERO,
        ..Limits::default()
    };
    let h = Hosted::start_with(false, limits);
    let generation = |h: &Hosted| {
        h.get("/health", &[]).json()["catalog_generation"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(generation(&h), 1);
    make_pack(
        &h.lib,
        "Garden Kit",
        'e',
        &[("/Game/Garden/SM_Fence", "StaticMesh")],
    );
    let end = Instant::now() + Duration::from_secs(10);
    while generation(&h) < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(generation(&h), 2, "the new pack was never swapped in");
    assert_eq!(h.get("/health", &[]).json()["assets"], 4);
    // An emptied library never replaces the good snapshot.
    for e in fs::read_dir(&h.lib).unwrap() {
        let _ = fs::remove_file(e.unwrap().path());
    }
    std::thread::sleep(Duration::from_millis(900));
    let v = h.get("/health", &[]).json();
    assert_eq!(v["assets"], 4);
    assert_eq!(v["catalog_generation"], 2);
    let r = h.rpc(
        "tools/call",
        json!({"name": "search_assets", "arguments": {"query": "fence"}}),
    );
    let v: Value = serde_json::from_str(&tool_text(&r)).unwrap();
    assert_eq!(v["total"], 1);
    let _ = &h.tmp;
}

#[test]
fn a_changing_library_is_reloaded_at_most_once_per_gap() {
    let limits = Limits {
        refresh_interval: Duration::from_millis(100),
        min_reload_gap: Duration::from_millis(2500),
        ..Limits::default()
    };
    let h = Hosted::start_with(false, limits);
    let generation = |h: &Hosted| {
        h.get("/health", &[]).json()["catalog_generation"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(generation(&h), 1);
    make_pack(
        &h.lib,
        "Garden Kit",
        'e',
        &[("/Game/Garden/SM_Fence", "StaticMesh")],
    );
    // Well inside the gap the change is seen but not yet loaded.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(generation(&h), 1, "reloaded inside the minimum gap");
    // After the gap the pending change is picked up without another edit.
    let end = Instant::now() + Duration::from_secs(10);
    while generation(&h) < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(generation(&h), 2, "the change was never loaded");
    let _ = &h.tmp;
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
fn startup_flag_errors() {
    let tmp = Tmp::new();
    let lib = tmp.dir("lib");
    let lib_s = lib.to_string_lossy().to_string();
    let missing = tmp.0.join("absent.token");
    let empty = tmp.0.join("empty.token");
    fs::write(&empty, "  \r\n").unwrap();
    // No token file / unreadable / empty: exit 2.
    let (code, _) = run_bin(&["--library", &lib_s, "--http", "127.0.0.1:0"]);
    assert_eq!(code, Some(2));
    let (code, err) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "127.0.0.1:0",
        "--token-file",
        &missing.to_string_lossy(),
    ]);
    assert_eq!(code, Some(2), "{err}");
    let (code, _) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "127.0.0.1:0",
        "--token-file",
        &empty.to_string_lossy(),
    ]);
    assert_eq!(code, Some(2));
    // All interfaces are refused; read-only excludes overlay writes.
    let (code, err) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "0.0.0.0:8433",
        "--token-file",
        "x",
    ]);
    assert_eq!(code, Some(1));
    assert!(err.contains("never 0.0.0.0"), "{err}");
    let (code, _) = run_bin(&["--library", &lib_s, "--read-only", "--write-overlays"]);
    assert_eq!(code, Some(1));
    let (code, _) = run_bin(&[
        "--library",
        &lib_s,
        "--http",
        "127.0.0.1:0",
        "--token-file",
        "x",
        "--write-overlays",
    ]);
    assert_eq!(code, Some(1));
}

#[test]
fn stdio_profile_is_unchanged_without_read_only() {
    // The default (stdio) configuration still lists every tool.
    let app = App::with_catalog(Catalog::empty(), AppConfig::default());
    let names: Vec<String> = app
        .visible_tools()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "import_assets"));
    assert!(names.iter().any(|n| n == "refresh_catalog"));
}
