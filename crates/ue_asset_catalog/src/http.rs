//! A deliberately tiny blocking HTTP/1.1 client over `std::net` (plain `http://` only, one request
//! per connection, `Connection: close`). Enough for the LAN portal (NocoBase) and Ollama; it keeps
//! the crate free of an HTTP/TLS dependency tree. Never use it for the open internet.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Responses bigger than this are refused (a runaway server must not eat memory).
const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body).map_err(|e| format!("response is not JSON: {e}"))
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub target: String,
}

/// Parse `http://host[:port][/path?query]`.
pub fn parse_url(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only plain http:// URLs are supported: {url}"))?;
    let (authority, target) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in URL: {url}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("no host in URL: {url}"));
    }
    Ok(Url { host, port, target })
}

/// Percent-encode a query component.
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn connect(url: &Url, timeout: Duration) -> Result<TcpStream, String> {
    let addrs: Vec<_> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", url.host))?
        .collect();
    let mut last = format!("no address for {}", url.host);
    for a in addrs {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = format!("cannot connect to {a}: {e}"),
        }
    }
    Err(last)
}

/// Send one request and read the whole response.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Response, String> {
    let u = parse_url(url)?;
    let mut stream = connect(&u, timeout)?;
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: ue_asset_catalog\r\nAccept: application/json\r\nConnection: close\r\n",
        u.target, u.host, u.port
    );
    for (k, v) in headers {
        if v.contains('\r') || v.contains('\n') {
            return Err(format!("invalid header value for {k}"));
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("send failed: {e}"))?;
    if let Some(b) = body {
        stream
            .write_all(b)
            .map_err(|e| format!("send failed: {e}"))?;
    }
    stream.flush().map_err(|e| format!("send failed: {e}"))?;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_BODY {
                    return Err("response too large".to_string());
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    parse_response(&raw)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse a raw HTTP/1.x response (content-length, chunked, or read-to-close bodies).
pub fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let split = find(raw, b"\r\n\r\n").ok_or("malformed HTTP response (no header end)")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[split + 4..];
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty HTTP response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
            if k == "transfer-encoding" && v.contains("chunked") {
                chunked = true;
            } else if k == "content-length" {
                length = v.parse().ok();
            }
        }
    }
    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(n) = length {
        body[..n.min(body.len())].to_vec()
    } else {
        body.to_vec()
    };
    Ok(Response { status, body })
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = find(data, b"\r\n").ok_or("bad chunked body")?;
        let size_text = String::from_utf8_lossy(&data[..eol]).to_string();
        let size_hex = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| "bad chunk size")?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size {
            return Err("truncated chunked body".to_string());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        data = data.strip_prefix(b"\r\n").unwrap_or(data);
    }
}

pub fn get(url: &str, headers: &[(&str, String)], timeout: Duration) -> Result<Response, String> {
    request("GET", url, headers, None, timeout)
}

pub fn post_json(
    url: &str,
    headers: &[(&str, String)],
    body: &Value,
    timeout: Duration,
) -> Result<Response, String> {
    let mut h: Vec<(&str, String)> = headers.to_vec();
    h.push(("Content-Type", "application/json".to_string()));
    request("POST", url, &h, Some(body.to_string().as_bytes()), timeout)
}

/// One part of a multipart/form-data body.
pub struct Part<'a> {
    pub name: &'a str,
    pub filename: &'a str,
    pub content_type: &'a str,
    pub data: &'a [u8],
}

/// Build a multipart/form-data body; returns (content type header value, body).
pub fn multipart_body(parts: &[Part]) -> (String, Vec<u8>) {
    let boundary = format!("----ueassetcatalog{:016x}", {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            .hash(&mut h);
        h.finish()
    });
    let mut body: Vec<u8> = Vec::new();
    for p in parts {
        let safe = |s: &str| s.replace(['"', '\r', '\n'], "_");
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                safe(p.name),
                safe(p.filename),
                p.content_type
            )
            .as_bytes(),
        );
        body.extend_from_slice(p.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub fn post_multipart(
    url: &str,
    headers: &[(&str, String)],
    parts: &[Part],
    timeout: Duration,
) -> Result<Response, String> {
    let (ct, body) = multipart_body(parts);
    let mut h: Vec<(&str, String)> = headers.to_vec();
    h.push(("Content-Type", ct));
    request("POST", url, &h, Some(&body), timeout)
}

/// A one-shot fake server for tests: serves the canned responses in order, recording the raw
/// requests it received.
#[cfg(test)]
pub(crate) mod fake {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub struct FakeServer {
        pub url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    /// `responses`: full raw HTTP responses (headers + body) served to consecutive connections.
    pub fn serve(responses: Vec<String>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let rec = requests.clone();
        std::thread::spawn(move || {
            for resp in responses {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut data: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                // Read until the headers and the announced body are complete.
                loop {
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data).to_string();
                    if let Some(he) = text.find("\r\n\r\n") {
                        let cl = text[..he]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= he + 4 + cl {
                            break;
                        }
                    }
                }
                rec.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&data).to_string());
                let _ = s.write_all(resp.as_bytes());
            }
        });
        FakeServer {
            url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    pub fn ok_json(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_urls() {
        let u = parse_url("http://192.168.12.201:13000/api/x:list?a=1").unwrap();
        assert_eq!(u.host, "192.168.12.201");
        assert_eq!(u.port, 13000);
        assert_eq!(u.target, "/api/x:list?a=1");
        let u = parse_url("http://host").unwrap();
        assert_eq!((u.port, u.target.as_str()), (80, "/"));
        assert!(parse_url("https://x").is_err());
        assert!(parse_url("http://:80/").is_err());
        assert!(parse_url("http://h:notaport/").is_err());
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(
            urlencode("{\"state\":\"completed\"}"),
            "%7B%22state%22%3A%22completed%22%7D"
        );
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn parses_length_chunked_and_close_bodies() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "hello"));
        let r = parse_response(
            b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((r.status, r.text().as_str()), (201, "hello world"));
        let r = parse_response(b"HTTP/1.0 404 Not Found\r\n\r\nnope").unwrap();
        assert_eq!(
            (r.status, r.text().as_str(), r.is_success()),
            (404, "nope", false)
        );
        assert!(parse_response(b"garbage").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err()
        );
    }

    #[test]
    fn talks_to_a_local_server() {
        let srv = fake::serve(vec![
            fake::ok_json(r#"{"data":[1,2]}"#),
            fake::ok_json(r#"{"ok":true}"#),
            fake::ok_json(r#"{"up":1}"#),
        ]);
        let t = Duration::from_secs(5);
        let r = get(&format!("{}/api/x:list?pageSize=2", srv.url), &[], t).unwrap();
        assert_eq!(r.json().unwrap()["data"], json!([1, 2]));
        let r = post_json(
            &format!("{}/api/y", srv.url),
            &[("Authorization", "Bearer secret".to_string())],
            &json!({"a": 1}),
            t,
        )
        .unwrap();
        assert!(r.is_success());
        let part = Part {
            name: "image",
            filename: "a.png",
            content_type: "image/png",
            data: b"PNGDATA",
        };
        post_multipart(&format!("{}/api/z", srv.url), &[], &[part], t).unwrap();
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(reqs[0].starts_with("GET /api/x:list?pageSize=2 HTTP/1.1"));
        assert!(reqs[1].starts_with("POST /api/y HTTP/1.1"));
        assert!(reqs[1].contains("Authorization: Bearer secret"));
        assert!(reqs[1].ends_with("{\"a\":1}"));
        assert!(reqs[2].contains("name=\"image\"; filename=\"a.png\""));
        assert!(reqs[2].contains("PNGDATA"));
        assert!(reqs[2].contains("multipart/form-data; boundary="));
    }

    #[test]
    fn connection_failures_are_errors_not_panics() {
        // Port 1 on localhost: refused (or filtered) quickly.
        let e = get("http://127.0.0.1:1/", &[], Duration::from_millis(500));
        assert!(e.is_err());
    }
}
