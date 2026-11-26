//! Read-only MCP over HTTP (`ue_asset_mcp --http`): a hand-rolled blocking listener on
//! `std::net` (no async runtime, no HTTP crate).
//!
//! Pattern: HTTP MCP gateway (DOC-0192; code refs DOC-0232): bearer token, Host/Origin checks,
//! `POST /mcp` carries exactly one JSON-RPC message per request, `GET /health` is open. Differences
//! from the gateway: one shared [`Server`] (the catalog lives in this process), guarded by a
//! `Mutex` that is held only while a tool runs; the catalog is loaded and refreshed on a background
//! thread and swapped in (keep the last good snapshot), so `/health` answers while it loads.
//!
//! Rules (wire contract): `Connection: close` on every response, header block <= 16 KiB / 64
//! lines, a POST needs `Content-Length` (no `Transfer-Encoding`), body <= 1 MiB, 10 s read
//! deadline, 60 s write timeout, separate worker pools for `/mcp`+`/health` and `/bundles`
//! (pool full = 503). The access log has method, path, status and milliseconds only: never the
//! token, a header value or a body.

use crate::bundles::{self, BundleStore, Lease, LeaseError};
use crate::catalog::{fingerprint, Catalog, LoadReport};
use crate::generated::FetchReport;
use crate::mcp::{Server, PROTOCOL_VERSIONS};
use crate::tools::App;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Tunables; the defaults are the wire contract. Tests shorten the deadlines.
#[derive(Clone, Debug)]
pub struct Limits {
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_head: usize,
    pub max_header_lines: usize,
    pub max_body: usize,
    /// Workers for `/mcp` and `/health`.
    pub mcp_pool: usize,
    /// Workers for `/bundles/<id>` downloads.
    pub bundle_pool: usize,
    /// Open connections (including ones still sending their request).
    pub max_connections: usize,
    /// How often the background thread looks for a changed library.
    pub refresh_interval: Duration,
    /// Minimum time between two reloads of a library that keeps changing (the enrichment task
    /// rewrites sidecars all day; each reload re-reads the whole library). An empty catalog is
    /// retried every `refresh_interval` regardless.
    pub min_reload_gap: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(60),
            max_head: 16 * 1024,
            max_header_lines: 64,
            max_body: 1024 * 1024,
            mcp_pool: 8,
            bundle_pool: 2,
            max_connections: 64,
            refresh_interval: crate::tools::REFRESH_INTERVAL,
            min_reload_gap: Duration::from_secs(300),
        }
    }
}

pub struct StartOptions {
    /// The bearer token (already trimmed, non-empty); `None` = no authentication (`--no-auth`).
    pub token: Option<Vec<u8>>,
    /// Accepted `Host` header values; empty = the listener's own `ip:port`.
    pub allowed_hosts: Vec<String>,
    pub limits: Limits,
}

/// Produces a freshly loaded catalog (runs on the background thread, never under the lock).
pub type Loader = Box<dyn Fn() -> (Catalog, LoadReport, FetchReport) + Send + 'static>;

/// Read the bearer token file: trimmed, non-empty. The error never contains token bytes.
pub fn read_token_file(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read token file {}: {}", path.display(), e.kind()))?;
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    let token = bytes[start..end].to_vec();
    if token.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    Ok(token)
}

// ---------------------------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------------------------

struct Pool {
    cap: usize,
    used: AtomicUsize,
}

struct Permit<'a>(&'a AtomicUsize);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Pool {
    fn new(cap: usize) -> Pool {
        Pool {
            cap,
            used: AtomicUsize::new(0),
        }
    }

    fn acquire(&self) -> Option<Permit<'_>> {
        let prev = self.used.fetch_add(1, Ordering::SeqCst);
        if prev >= self.cap {
            self.used.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Permit(&self.used))
    }
}

struct Shared {
    server: Mutex<Server>,
    token_digest: Option<[u8; 32]>,
    allowed_hosts: Vec<String>,
    limits: Limits,
    read_only: bool,
    /// The first catalog load finished.
    ready: AtomicBool,
    packs: AtomicUsize,
    assets: AtomicUsize,
    generation: AtomicU64,
    loaded_at: Mutex<String>,
    mcp_pool: Pool,
    bundle_pool: Pool,
    connections: AtomicUsize,
    /// Hosted bundles (`prepare_import`, `GET /bundles/<id>`); `None` = every id is unknown.
    bundles: Option<Arc<BundleStore>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct ConnGuard(Arc<Shared>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Constant-time equality of two digests.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    bundles::rfc3339_unix(secs)
}

// ---------------------------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------------------------

struct Resp {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    extra: Vec<(&'static str, String)>,
    /// The response was already streamed to the peer (a bundle download).
    sent: bool,
}

impl Resp {
    fn json(status: u16, v: &Value) -> Resp {
        Resp {
            status,
            content_type: "application/json",
            body: v.to_string().into_bytes(),
            extra: Vec::new(),
            sent: false,
        }
    }

    fn empty(status: u16) -> Resp {
        Resp {
            status,
            content_type: "application/json",
            body: Vec::new(),
            extra: Vec::new(),
            sent: false,
        }
    }

    /// Log-only marker for a response that was streamed.
    fn streamed(status: u16) -> Resp {
        Resp {
            sent: true,
            ..Resp::empty(status)
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Resp {
        Resp::json(
            status,
            &json!({"error": {"code": code, "message": message}}),
        )
    }

    fn with(mut self, name: &'static str, value: &str) -> Resp {
        self.extra.push((name, value.to_string()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        410 => "Gone",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Service Unavailable",
    }
}

enum ReqErr {
    /// The peer went away before sending anything.
    Closed,
    Timeout,
    Bad(&'static str),
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(n, _)| n == name).count()
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn remaining(deadline: Instant) -> Result<Duration, ReqErr> {
    let now = Instant::now();
    if now >= deadline {
        Err(ReqErr::Timeout)
    } else {
        Ok(deadline - now)
    }
}

/// Fill `buf` with one read, honouring the total deadline.
fn read_some(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
) -> Result<usize, ReqErr> {
    let mut chunk = [0u8; 4096];
    loop {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|_| ReqErr::Closed)?;
        match stream.read(&mut chunk) {
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                return Ok(n);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ReqErr::Timeout)
            }
            Err(_) => return Err(ReqErr::Closed),
        }
    }
}

/// The header block (text) and any body bytes that arrived with it.
fn read_head(
    stream: &mut TcpStream,
    deadline: Instant,
    max: usize,
) -> Result<(String, Vec<u8>), ReqErr> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            if pos > max {
                return Err(ReqErr::Bad("headers too large"));
            }
            let head = String::from_utf8(buf[..pos].to_vec())
                .map_err(|_| ReqErr::Bad("headers are not valid text"))?;
            return Ok((head, buf[pos + 4..].to_vec()));
        }
        if buf.len() > max {
            return Err(ReqErr::Bad("headers too large"));
        }
        if read_some(stream, &mut buf, deadline)? == 0 {
            return Err(if buf.is_empty() {
                ReqErr::Closed
            } else {
                ReqErr::Bad("incomplete request")
            });
        }
    }
}

fn parse_head(head: &str, max_lines: usize) -> Result<Request, ReqErr> {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let parts: Vec<&str> = first.split(' ').collect();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/1.") || parts[0].is_empty() {
        return Err(ReqErr::Bad("malformed request line"));
    }
    let target = parts[1];
    if !target.starts_with('/') {
        return Err(ReqErr::Bad("malformed request target"));
    }
    let path = target.split(['?', '#']).next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if headers.len() + 1 >= max_lines {
            return Err(ReqErr::Bad("too many header lines"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReqErr::Bad("malformed header line"));
        };
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace() || b < 0x21) {
            return Err(ReqErr::Bad("malformed header name"));
        }
        headers.push((
            name.to_ascii_lowercase(),
            value.trim_matches([' ', '\t']).to_string(),
        ));
    }
    let req = Request {
        method: parts[0].to_string(),
        path,
        headers,
    };
    for single in ["host", "content-length", "authorization"] {
        if req.count(single) > 1 {
            return Err(ReqErr::Bad("duplicate header"));
        }
    }
    Ok(req)
}

fn read_body(
    stream: &mut TcpStream,
    mut buf: Vec<u8>,
    len: usize,
    deadline: Instant,
) -> Result<Vec<u8>, ReqErr> {
    while buf.len() < len {
        if read_some(stream, &mut buf, deadline)? == 0 {
            return Err(ReqErr::Bad("body shorter than Content-Length"));
        }
    }
    buf.truncate(len);
    Ok(buf)
}

fn write_response(stream: &mut TcpStream, resp: &Resp) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len()
    );
    for (n, v) in &resp.extra {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&resp.body);
    let _ = stream.write_all(&out);
    let _ = stream.flush();
    linger(stream);
}

/// Close our side and swallow what the client may still be sending.
fn linger(stream: &mut TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    // Swallow what the client may still be sending so an early error is not turned into a reset.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 4096];
    let mut total = 0usize;
    while total < 64 * 1024 {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

fn log_safe(s: &str, cap: usize) -> String {
    s.chars()
        .take(cap)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// request handling
// ---------------------------------------------------------------------------------------------

enum Route {
    Mcp,
    Health,
    Bundle,
    Unknown,
}

fn jsonrpc_error(code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": code, "message": message}})
}

fn health(shared: &Shared) -> Resp {
    let ready = shared.ready.load(Ordering::SeqCst);
    Resp::json(
        200,
        &json!({
            "status": if ready { "ok" } else { "loading" },
            "server": crate::tools::SERVER_NAME,
            "version": crate::tools::SERVER_VERSION,
            "read_only": shared.read_only,
            "packs": shared.packs.load(Ordering::SeqCst),
            "assets": shared.assets.load(Ordering::SeqCst),
            "catalog_generation": shared.generation.load(Ordering::SeqCst),
            "catalog_loaded_at": lock(&shared.loaded_at).clone(),
            "bundles_pending": shared.bundles.as_ref().map_or(0, |b| b.pending()),
        }),
    )
}

/// `tools/call prepare_import`: resolve the request under the catalog lock (short), then build
/// and publish the bundle WITHOUT it, so searches keep answering while a bundle is built.
/// `None` = not a prepare_import call (the generic handler answers it).
fn prepare_import_reply(shared: &Shared, store: &BundleStore, msg: &Value) -> Option<Value> {
    if msg.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let id = msg.get("id")?.clone();
    let params = msg.get("params")?;
    if params.get("name").and_then(Value::as_str) != Some("prepare_import") {
        return None;
    }
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let args = match args {
        Value::Null => json!({}),
        Value::Object(_) => args,
        _ => return None,
    };
    let generation = shared.generation.load(Ordering::SeqCst);
    let job = lock(&shared.server).app.prepare_job(&args, generation);
    let result: Result<Value, String> = match job {
        Err(e) => Err(e),
        Ok(job) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bundles::prepare(store, job)
        })) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(refusal)) => Err(refusal.text()),
            Err(_) => {
                eprintln!("[ue_asset_mcp] tool `prepare_import` panicked");
                Err("internal error while running `prepare_import`".to_string())
            }
        },
    };
    let (text, is_error) = match result {
        Ok(v) => (v.to_string(), false),
        Err(e) => (format!("error: {e}"), true),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    }}))
}

fn mcp_post(
    shared: &Shared,
    req: &Request,
    stream: &mut TcpStream,
    rest: Vec<u8>,
    deadline: Instant,
) -> Resp {
    if req.header("transfer-encoding").is_some() {
        return Resp::error(400, "BAD_REQUEST", "Transfer-Encoding is not supported");
    }
    let Some(len) = req.header("content-length") else {
        return Resp::error(411, "LENGTH_REQUIRED", "Content-Length is required");
    };
    let Ok(len) = len.parse::<u64>() else {
        return Resp::error(400, "BAD_REQUEST", "invalid Content-Length");
    };
    if len > shared.limits.max_body as u64 {
        return Resp::error(413, "TOO_LARGE", "request body is too large");
    }
    if let Some(v) = req.header("mcp-protocol-version") {
        if !PROTOCOL_VERSIONS.contains(&v) {
            return Resp::error(400, "BAD_REQUEST", "unsupported MCP-Protocol-Version");
        }
    }
    let body = match read_body(stream, rest, len as usize, deadline) {
        Ok(b) => b,
        Err(ReqErr::Timeout) => {
            return Resp::error(408, "TIMEOUT", "request body not received in time")
        }
        Err(ReqErr::Bad(m)) => return Resp::error(400, "BAD_REQUEST", m),
        Err(ReqErr::Closed) => return Resp::empty(400),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Resp::json(400, &jsonrpc_error(crate::mcp::PARSE_ERROR, "parse error")),
    };
    if !msg.is_object() {
        return Resp::json(
            400,
            &jsonrpc_error(
                crate::mcp::INVALID_REQUEST,
                "the body must be one JSON-RPC message object (batches are not supported)",
            ),
        );
    }
    let is_call = msg.get("method").and_then(Value::as_str) == Some("tools/call");
    if is_call && !shared.ready.load(Ordering::SeqCst) {
        if let Some(id) = msg.get("id") {
            return Resp::json(
                200,
                &json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": "error: BUSY: the catalog is still loading, retry shortly"}],
                    "isError": true
                }}),
            );
        }
    }
    if let Some(store) = &shared.bundles {
        if let Some(reply) = prepare_import_reply(shared, store, &msg) {
            return Resp::json(200, &reply);
        }
    }
    // The lock covers only the message handling (a tool's catalog work).
    let reply = lock(&shared.server).handle_value(msg);
    match reply {
        Some(v) => Resp::json(200, &v),
        None => Resp::empty(202),
    }
}

/// Everything after the head is read. `None` = nothing to answer (peer closed).
fn process(shared: &Shared, stream: &mut TcpStream) -> Option<(String, String, Resp)> {
    let deadline = Instant::now() + shared.limits.read_timeout;
    let (head, rest) = match read_head(stream, deadline, shared.limits.max_head) {
        Ok(h) => h,
        Err(ReqErr::Closed) => return None,
        Err(ReqErr::Timeout) => {
            return Some((
                "-".into(),
                "-".into(),
                Resp::error(408, "TIMEOUT", "request not received in time"),
            ))
        }
        Err(ReqErr::Bad(m)) => {
            return Some(("-".into(), "-".into(), Resp::error(400, "BAD_REQUEST", m)))
        }
    };
    let req = match parse_head(&head, shared.limits.max_header_lines) {
        Ok(r) => r,
        Err(ReqErr::Bad(m)) => {
            return Some(("-".into(), "-".into(), Resp::error(400, "BAD_REQUEST", m)))
        }
        Err(_) => return None,
    };
    let method = log_safe(&req.method, 8);
    let route = match req.path.as_str() {
        "/mcp" => Route::Mcp,
        "/health" => Route::Health,
        p if p.starts_with("/bundles/") && p.len() > "/bundles/".len() => Route::Bundle,
        _ => Route::Unknown,
    };
    let log_path = match route {
        Route::Bundle => "/bundles/*".to_string(),
        _ => log_safe(&req.path, 64),
    };
    let resp = respond(shared, &req, route, stream, rest, deadline);
    Some((method, log_path, resp))
}

fn respond(
    shared: &Shared,
    req: &Request,
    route: Route,
    stream: &mut TcpStream,
    rest: Vec<u8>,
    deadline: Instant,
) -> Resp {
    // 1. Host, 2. Origin.
    let host_ok = req.header("host").is_some_and(|h| {
        shared
            .allowed_hosts
            .iter()
            .any(|a| a.eq_ignore_ascii_case(h))
    });
    if !host_ok {
        return Resp::error(403, "FORBIDDEN", "host not allowed");
    }
    if req.header("origin").is_some() {
        return Resp::error(403, "FORBIDDEN", "browser origins are not allowed");
    }
    // 3. Route and method.
    let allow = match route {
        Route::Mcp => "POST",
        Route::Health | Route::Bundle => "GET",
        Route::Unknown => return Resp::error(404, "NOT_FOUND", "no such path"),
    };
    if req.method != allow {
        return Resp::error(405, "METHOD", "method not allowed").with("Allow", allow);
    }
    // 4. Bearer (everything except /health).
    if let (false, Some(want)) = (matches!(route, Route::Health), shared.token_digest.as_ref()) {
        let presented = req
            .header("authorization")
            .and_then(|a| a.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, t)| t.trim());
        let ok = presented.is_some_and(|t| ct_eq(&digest(t.as_bytes()), want));
        if !ok {
            return Resp::error(401, "UNAUTHORIZED", "a valid bearer token is required")
                .with("WWW-Authenticate", "Bearer");
        }
    }
    // 5. Worker pool, then the route.
    match route {
        Route::Health | Route::Mcp => {
            let Some(_permit) = shared.mcp_pool.acquire() else {
                return busy();
            };
            if matches!(route, Route::Health) {
                health(shared)
            } else {
                mcp_post(shared, req, stream, rest, deadline)
            }
        }
        Route::Bundle => {
            let Some(_permit) = shared.bundle_pool.acquire() else {
                return busy();
            };
            let id = req.path.strip_prefix("/bundles/").unwrap_or("");
            let Some(store) = &shared.bundles else {
                return Resp::error(404, "NOT_FOUND", "no such bundle");
            };
            match store.lease(id) {
                Err(LeaseError::NotFound) => Resp::error(404, "NOT_FOUND", "no such bundle"),
                Err(LeaseError::Expired) => Resp::error(
                    410,
                    "BUNDLE_EXPIRED",
                    "the bundle has expired; call prepare_import again",
                ),
                // The worker permit is held until the last byte is written.
                Ok(lease) => stream_bundle(stream, lease),
            }
        }
        Route::Unknown => Resp::error(404, "NOT_FOUND", "no such path"),
    }
}

/// Stream an admitted bundle: headers, then the file in chunks (never held in memory, never under
/// the catalog lock). Each write call has the write deadline; a peer that stops reading is cut off.
fn stream_bundle(stream: &mut TcpStream, lease: Lease) -> Resp {
    let Lease {
        id,
        file,
        size,
        sha256,
    } = lease;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {size}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Bundle-Sha256: {sha256}\r\nX-Bundle-Id: {id}\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return Resp::streamed(499);
    }
    let mut src = file.take(size);
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Resp::streamed(500),
        };
        if stream.write_all(&buf[..n]).is_err() {
            return Resp::streamed(499);
        }
        sent += n as u64;
    }
    let _ = stream.flush();
    // A short file leaves the client with fewer bytes than Content-Length: it re-prepares.
    Resp::streamed(if sent == size { 200 } else { 500 })
}

fn busy() -> Resp {
    Resp::error(503, "BUSY", "all workers are busy, retry shortly").with("Retry-After", "5")
}

fn handle_conn(shared: &Shared, mut stream: TcpStream) {
    let start = Instant::now();
    let _ = stream.set_write_timeout(Some(shared.limits.write_timeout));
    let _ = stream.set_nodelay(true);
    let Some((method, path, resp)) = process(shared, &mut stream) else {
        return;
    };
    if resp.sent {
        linger(&mut stream);
    } else {
        write_response(&mut stream, &resp);
    }
    eprintln!(
        "[ue_asset_mcp] http {method} {path} {} {}ms",
        resp.status,
        start.elapsed().as_millis()
    );
}

// ---------------------------------------------------------------------------------------------
// catalog loading (background)
// ---------------------------------------------------------------------------------------------

fn install(shared: &Shared, catalog: Catalog, report: LoadReport, fetch: FetchReport) -> bool {
    let st = catalog.stats();
    let elapsed = report.elapsed_ms;
    let (cached, overlay, built) = (report.from_cache, report.from_overlay, report.built);
    for (name, why) in &report.failed {
        eprintln!("[ue_asset_mcp] index failed for {name}: {why}");
    }
    let swapped = lock(&shared.server)
        .app
        .install_catalog(catalog, report, fetch);
    if swapped {
        shared.packs.store(st.packs, Ordering::SeqCst);
        shared.assets.store(st.assets, Ordering::SeqCst);
        *lock(&shared.loaded_at) = rfc3339_now();
        shared.generation.fetch_add(1, Ordering::SeqCst);
        eprintln!(
            "[ue_asset_mcp] catalog: {} packs ({} indexed, {} failed, {} plugins skipped), {} assets; load {elapsed} ms (cache {cached}, overlay {overlay}, built {built})",
            st.packs, st.indexed_packs, st.failed_packs, st.skipped_plugins, st.assets
        );
    } else {
        eprintln!("[ue_asset_mcp] warning: reload found no assets; keeping the last good catalog");
    }
    swapped
}

fn background(shared: Arc<Shared>, loader: Loader, stop: Arc<AtomicBool>) {
    let (catalog, report, fetch) = loader();
    let mut dirs = catalog.dirs.clone();
    let mut seen = catalog.fingerprint;
    let mut empty = catalog.assets.is_empty();
    install(&shared, catalog, report, fetch);
    shared.ready.store(true, Ordering::SeqCst);
    let mut last_reload = Instant::now();
    loop {
        let wake = Instant::now() + shared.limits.refresh_interval;
        while Instant::now() < wake {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        // An empty catalog (share unreachable at start) is retried; otherwise only a changed
        // library triggers a reload. The probe needs no lock.
        let now = if dirs.is_empty() {
            seen
        } else {
            fingerprint(&dirs)
        };
        if !empty && now == seen {
            continue;
        }
        // A library that keeps changing is reloaded at most once per `min_reload_gap`; the change
        // is picked up by a later probe, nothing is lost.
        if !empty && last_reload.elapsed() < shared.limits.min_reload_gap {
            continue;
        }
        eprintln!("[ue_asset_mcp] library changed, reloading catalog");
        let (catalog, report, fetch) = loader();
        last_reload = Instant::now();
        let fp = catalog.fingerprint;
        let dirs_new = catalog.dirs.clone();
        let assets_empty = catalog.assets.is_empty();
        if install(&shared, catalog, report, fetch) {
            seen = fp;
            dirs = dirs_new;
            empty = assets_empty;
        } else {
            seen = now; // do not hammer a broken library every interval
        }
    }
}

// ---------------------------------------------------------------------------------------------
// start / stop
// ---------------------------------------------------------------------------------------------

pub struct Running {
    pub local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Running {
    /// Block until the listener stops (normally never).
    pub fn wait(mut self) {
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }

    fn stop_now(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.local_addr, Duration::from_millis(500));
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }

    /// Stop accepting and wait for the accept thread (tests).
    pub fn shutdown(mut self) {
        self.stop_now();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.stop_now();
        }
    }
}

/// Serve `app` (its catalog may be empty: `loader` fills it in on a background thread) on
/// `listener`. Returns at once.
pub fn start(
    listener: TcpListener,
    app: App,
    loader: Loader,
    opts: StartOptions,
) -> io::Result<Running> {
    let local_addr = listener.local_addr()?;
    let mut allowed_hosts: Vec<String> = opts
        .allowed_hosts
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    if allowed_hosts.is_empty() {
        allowed_hosts.push(local_addr.to_string());
    }
    let limits = opts.limits;
    let bundles = match app.cfg.bundle.clone() {
        Some(cfg) => Some(Arc::new(BundleStore::open(cfg)?)),
        None => None,
    };
    let shared = Arc::new(Shared {
        read_only: app.cfg.read_only,
        server: Mutex::new(Server::new(app)),
        token_digest: opts.token.as_deref().map(digest),
        allowed_hosts,
        mcp_pool: Pool::new(limits.mcp_pool),
        bundle_pool: Pool::new(limits.bundle_pool),
        limits,
        ready: AtomicBool::new(false),
        packs: AtomicUsize::new(0),
        assets: AtomicUsize::new(0),
        generation: AtomicU64::new(0),
        loaded_at: Mutex::new(String::new()),
        connections: AtomicUsize::new(0),
        bundles,
    });
    let stop = Arc::new(AtomicBool::new(false));

    if let Some(store) = shared.bundles.clone() {
        let stop = stop.clone();
        thread::Builder::new()
            .name("bundle-sweep".into())
            .spawn(move || {
                let every = store.config().sweep_interval;
                let mut next = Instant::now() + every;
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(25));
                    if Instant::now() >= next {
                        store.sweep();
                        next = Instant::now() + every;
                    }
                }
            })?;
    }

    {
        let shared = shared.clone();
        let stop = stop.clone();
        thread::Builder::new()
            .name("catalog-loader".into())
            .spawn(move || background(shared, loader, stop))?;
    }

    let accept = {
        let shared = shared.clone();
        let stop = stop.clone();
        thread::Builder::new()
            .name("http-accept".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = conn else { continue };
                    if shared.connections.fetch_add(1, Ordering::SeqCst)
                        >= shared.limits.max_connections
                    {
                        shared.connections.fetch_sub(1, Ordering::SeqCst);
                        let mut s = stream;
                        let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
                        let resp = busy();
                        let head = format!(
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 5\r\n\r\n",
                            resp.body.len()
                        );
                        let _ = s.write_all(head.as_bytes());
                        let _ = s.write_all(&resp.body);
                        continue;
                    }
                    let guard = ConnGuard(shared.clone());
                    let sh = shared.clone();
                    let _ = thread::Builder::new()
                        .name("http-conn".into())
                        .spawn(move || {
                            let _guard = guard;
                            handle_conn(&sh, stream);
                        });
                }
            })?
    };
    Ok(Running {
        local_addr,
        stop,
        accept: Some(accept),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_compare_is_exact() {
        assert!(ct_eq(&digest(b"abc"), &digest(b"abc")));
        assert!(!ct_eq(&digest(b"abc"), &digest(b"abd")));
    }

    #[test]
    fn pool_caps_and_releases() {
        let p = Pool::new(2);
        let a = p.acquire();
        let b = p.acquire();
        assert!(a.is_some() && b.is_some());
        assert!(p.acquire().is_none());
        drop(a);
        assert!(p.acquire().is_some());
    }

    #[test]
    fn utc_formatting() {
        assert!(rfc3339_now().ends_with('Z'));
        assert_eq!(rfc3339_now().len(), 20);
    }

    #[test]
    fn token_file_is_trimmed_and_never_empty() {
        let t = crate::testutil::Tmp::new();
        let f = t.path().join("tok");
        std::fs::write(&f, " secret-value \r\n").unwrap();
        assert_eq!(read_token_file(&f).unwrap(), b"secret-value");
        std::fs::write(&f, "\r\n").unwrap();
        let e = read_token_file(&f).unwrap_err();
        assert!(e.contains("empty"));
        assert!(read_token_file(&t.path().join("none")).is_err());
    }
}
