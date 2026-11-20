//! MCP over stdio: newline-delimited JSON-RPC 2.0, hand-rolled (no async runtime).
//!
//! Rules: only protocol messages go to the output stream (logs go to stderr); malformed input
//! never ends the loop; notifications (no `id`) get no reply; unknown methods answer -32601;
//! tool failures are `isError` results, an unknown tool name is -32602.

use crate::tools::{self, App, Content};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

/// Newest first. The client's version is echoed when we know it, else the newest is offered.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub struct Server {
    pub app: App,
    pub initialized: bool,
    pub protocol: Option<String>,
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn result_response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

impl Server {
    pub fn new(app: App) -> Server {
        Server {
            app,
            initialized: false,
            protocol: None,
        }
    }

    /// Handle one line of input. `None` = nothing to send (notification, blank line, or a
    /// response from the client).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            return None;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    error_response(&Value::Null, PARSE_ERROR, &format!("parse error: {e}"))
                        .to_string(),
                );
            }
        };
        if let Value::Array(items) = msg {
            if items.is_empty() {
                return Some(
                    error_response(&Value::Null, INVALID_REQUEST, "empty batch").to_string(),
                );
            }
            let replies: Vec<Value> = items
                .into_iter()
                .filter_map(|m| self.handle_value(m))
                .collect();
            return if replies.is_empty() {
                None
            } else {
                Some(Value::Array(replies).to_string())
            };
        }
        self.handle_value(msg).map(|v| v.to_string())
    }

    /// Handle one decoded JSON-RPC message.
    pub fn handle_value(&mut self, msg: Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            return Some(error_response(
                &Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message must be an object",
            ));
        };
        let id = obj.get("id").cloned();
        let method = obj
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(method) = method else {
            // A response from the client (we send no requests) or garbage.
            if obj.contains_key("result") || obj.contains_key("error") {
                return None;
            }
            return id.map(|id| error_response(&id, INVALID_REQUEST, "missing `method`"));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            self.on_notification(&method);
            return None;
        };
        Some(self.on_request(&id, &method, &params))
    }

    fn on_notification(&mut self, method: &str) {
        if method == "notifications/initialized" {
            self.initialized = true;
        }
        // Everything else (cancelled, progress, roots/list_changed, unknown) is ignored.
    }

    fn on_request(&mut self, id: &Value, method: &str, params: &Value) -> Value {
        match method {
            "initialize" => {
                let want = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = if PROTOCOL_VERSIONS.contains(&want) {
                    want
                } else {
                    PROTOCOL_VERSIONS[0]
                };
                self.protocol = Some(version.to_string());
                result_response(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {
                            "name": tools::SERVER_NAME,
                            "version": tools::SERVER_VERSION,
                        },
                        "instructions": self.app.instructions(),
                    }),
                )
            }
            "ping" => result_response(id, json!({})),
            "tools/list" => result_response(id, json!({"tools": self.app.visible_tools()})),
            "tools/call" => self.on_tool_call(id, params),
            _ => error_response(id, METHOD_NOT_FOUND, &format!("method not found: {method}")),
        }
    }

    fn on_tool_call(&mut self, id: &Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return error_response(id, INVALID_PARAMS, "tools/call needs a string `name`");
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        if !(args.is_null() || args.is_object()) {
            return error_response(id, INVALID_PARAMS, "`arguments` must be an object");
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.app.call_tool(name, &args)
        }));
        match outcome {
            Ok(Some(out)) => {
                let content: Vec<Value> = out
                    .content
                    .iter()
                    .map(|c| match c {
                        Content::Text(t) => json!({"type": "text", "text": t}),
                        Content::Image { data, mime } => {
                            json!({"type": "image", "data": data, "mimeType": mime})
                        }
                    })
                    .collect();
                result_response(id, json!({"content": content, "isError": out.is_error}))
            }
            Ok(None) => error_response(id, INVALID_PARAMS, &format!("unknown tool: {name}")),
            Err(_) => {
                eprintln!("[ue_asset_mcp] tool `{name}` panicked");
                // A failing tool is a tool error, not a protocol error.
                result_response(
                    id,
                    json!({
                        "content": [{"type": "text", "text": format!("error: internal error while running `{name}`")}],
                        "isError": true
                    }),
                )
            }
        }
    }
}

/// Read newline-delimited messages until end of input; write replies (one per line) and flush.
/// Invalid UTF-8 and malformed JSON are answered with errors, never fatal.
pub fn serve<R: BufRead, W: Write>(
    server: &mut Server,
    mut input: R,
    mut output: W,
) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = input.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf);
        if let Some(reply) = server.handle_line(&line) {
            output.write_all(reply.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::tools::AppConfig;

    fn server() -> Server {
        Server::new(App::with_catalog(Catalog::empty(), AppConfig::default()))
    }

    fn call(s: &mut Server, line: &str) -> Value {
        serde_json::from_str(&s.handle_line(line).expect("a reply")).unwrap()
    }

    #[test]
    fn initialize_negotiates_the_version() {
        let mut s = server();
        let r = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "ue_asset_mcp");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("search_assets"));
        let r = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        );
        assert_eq!(r["id"], "a");
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_reply() {
        let mut s = server();
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
        assert!(s.initialized);
        // Unknown notification: still silent. Unknown request: -32601.
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"foo/bar"}"#)
            .is_none());
        let r = call(&mut s, r#"{"jsonrpc":"2.0","id":7,"method":"foo/bar"}"#);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(r["id"], 7);
        // A client response is ignored.
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#)
            .is_none());
        assert!(s.handle_line("   ").is_none());
    }

    #[test]
    fn malformed_input_is_answered_and_survived() {
        let mut s = server();
        let r = call(&mut s, "{not json");
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert!(r["id"].is_null());
        let r = call(&mut s, "42");
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = call(&mut s, "[]");
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = call(&mut s, r#"{"jsonrpc":"2.0","id":3}"#);
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        // Still alive.
        let r = call(&mut s, r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#);
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn tools_call_errors() {
        let mut s = server();
        let r = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope"}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_asset","arguments":[]}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        // A tool failure is an isError result.
        let r = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_asset","arguments":{"id":"zzz"}}}"#,
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("error:"));
    }

    #[test]
    fn batches_and_serve_loop() {
        let mut s = server();
        let r = call(
            &mut s,
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#,
        );
        assert_eq!(r.as_array().unwrap().len(), 2);
        assert!(r[1]["result"]["tools"].as_array().unwrap().len() >= 9);

        let input = b"{bad\n\xff\xfe\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let mut out = Vec::new();
        serve(&mut s, &input[..], &mut out).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("-32700"));
        assert!(lines[1].contains("-32700"));
        assert!(lines[2].contains("\"result\""));
    }
}
