//! MCP stdio adapter: `drowse mcp-serve`.
//!
//! The Model Context Protocol is JSON-RPC 2.0, and drowsed already speaks
//! JSON-RPC 2.0 over a unix socket — so this adapter is a pure translator. It
//! holds no state and adds no daemon verbs: each `tools/call` opens a
//! short-lived socket connection to drowsed (reusing [`crate::connect`] /
//! [`crate::call_once`]), makes exactly one verb call, and wraps the result as
//! an MCP tool content block.
//!
//! Framing: newline-delimited JSON (the MCP stdio transport). One JSON-RPC
//! message per line, no embedded newlines in the envelope — so responses are
//! serialized compact; any pretty-printed payload rides escaped inside a JSON
//! string. We do not implement the legacy LSP-style `Content-Length` header
//! framing: the current stdio transport does not use it, and real MCP clients
//! (Claude Desktop/Code, the SDKs) are newline-delimited.
//!
//! Protocol version targeted: 2026-07-28 (the latest revision as of this
//! build). On `initialize` we echo the client's requested `protocolVersion`
//! when it sends one, so we interoperate across revisions for our tiny surface.
//!
//! TODO(M3 policy.* verbs): once the daemon grows the policy engine, add three
//! more tools here, each a one-verb translation like the read tools below:
//!   - `plan_fixes`  -> policy.plan   {window_s?} : propose reversible fixes
//!                      with expected battery impact + confidence, no changes.
//!   - `apply_fix`   -> policy.apply  {action_id, dry_run?} : enact one planned
//!                      fix by its stable id; return the measured-after delta.
//!   - `revert_fix`  -> policy.revert {action_id} : undo a previously applied
//!                      fix. Keep these read-vs-mutate honest in the tool
//!                      descriptions so an agent knows apply/revert change state.

use crate::{Fail, call_once, connect};
use drowse_api::ErrorCode;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Latest MCP revision as of this build. Echoed back only when the client does
/// not request its own version on `initialize`.
const MCP_PROTOCOL_VERSION: &str = "2026-07-28";
const SERVER_NAME: &str = "drowse";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the stdio MCP server until stdin reaches EOF. One line in, at most one
/// line out (notifications produce none).
pub fn serve(socket: Option<PathBuf>) -> Result<i32, Fail> {
    let mut reader = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(0), // EOF: client closed the pipe.
            Ok(_) => {}
            Err(e) => {
                return Err(Fail::new(
                    ErrorCode::Internal.exit_code(),
                    format!("stdin read error ({e})"),
                ));
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(trimmed, socket.as_deref()) {
            let mut s = serde_json::to_string(&resp).expect("response serializes");
            s.push('\n');
            out.write_all(s.as_bytes())
                .and_then(|_| out.flush())
                .map_err(|e| {
                    Fail::new(
                        ErrorCode::Internal.exit_code(),
                        format!("stdout write error ({e})"),
                    )
                })?;
        }
    }
}

/// Parse and route one JSON-RPC message. Returns the response envelope, or
/// `None` for notifications (no `id`) which must never be answered.
fn handle_message(line: &str, socket: Option<&Path>) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        // Parse error: id is unknown, so null per JSON-RPC.
        Err(e) => return Some(error_resp(Value::Null, -32700, format!("parse error: {e}"))),
    };
    let id = msg.get("id").cloned();
    let is_notification = id.is_none();
    let id = id.unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => Some(ok_resp(id, initialize_result(&params))),
        // Lifecycle + keepalive notifications/requests we simply acknowledge.
        "notifications/initialized" => None,
        "ping" => Some(ok_resp(id, json!({}))),
        "tools/list" => Some(ok_resp(id, json!({ "tools": tool_list() }))),
        "tools/call" => Some(handle_tools_call(id, &params, socket)),
        // Unknown notification: ignore. Unknown request: method-not-found.
        _ if is_notification => None,
        _ => Some(error_resp(id, -32601, format!("method not found: {method}"))),
    }
}

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        "capabilities": { "tools": {} }
    })
}

/// The three read tools. Descriptions are written for an agent deciding when to
/// call; inputSchema is JSON Schema so arguments validate client-side.
fn tool_list() -> Value {
    json!([
        {
            "name": "sleep_efficiency",
            "description": "How well is the machine's CPU package sleeping right now? \
Returns the Sleep Efficiency scorecard: SE score (0-100) and letter grade, \
idle power draw, estimated runtime, and per-source telemetry status. The \
'blood-pressure reading' of battery health. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window_s": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "description": "Sampling window in seconds. Omit to use the daemon's default."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "wakeup_offenders",
            "description": "What is keeping the CPU awake, ranked by battery cost? \
Returns the top processes/IRQs/devices that break the package out of deep \
sleep, ranked by package-sleep-breaks per second (the battery-costly metric, \
not raw wakeups). Requires the eBPF attribution path; degrades honestly when \
unprivileged. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window_s": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "description": "Sampling window in seconds. Omit to use the daemon's default."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max offenders to return (top-K by package-sleep-break cost)."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "check_sources",
            "description": "Which telemetry sources are available on this machine? \
Returns a one-shot scorecard plus the state of each data source (cpuidle, \
rapl, battery, amdgpu, ebpf): ok, degraded, permission_denied, or not_present. \
Call this first to learn what the other tools can measure here. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }
    ])
}

fn handle_tools_call(id: Value, params: &Value, socket: Option<&Path>) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => return error_resp(id, -32602, "tools/call missing 'name'"),
    };
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let (verb, verb_params) = match build_call(name, &args) {
        Some(v) => v,
        None => return error_resp(id, -32602, format!("unknown tool: {name}")),
    };

    // One short-lived connection, one verb. A daemon failure is a TOOL error
    // (isError:true in a successful response), not a JSON-RPC protocol error —
    // the agent asked a valid question, the environment couldn't answer it.
    match call_daemon(socket, verb, verb_params) {
        Ok(result) => ok_resp(id, tool_success(&result)),
        Err(f) => ok_resp(id, tool_error(&f.msg)),
    }
}

/// Map an MCP tool name + arguments to a daemon (verb, params). `None` = no
/// such tool. Only number-typed, known arguments are forwarded.
fn build_call(name: &str, args: &Value) -> Option<(&'static str, Option<Value>)> {
    match name {
        "sleep_efficiency" => Some(("score.get", number_params(args, &["window_s"]))),
        "wakeup_offenders" => Some(("offenders.get", number_params(args, &["window_s", "limit"]))),
        "check_sources" => Some(("system.check", None)),
        _ => None,
    }
}

/// Collect the named number-valued keys from `args` into a params object, or
/// `None` if none are present. Non-numbers and unknown keys are dropped (the
/// inputSchema already rejects them client-side).
fn number_params(args: &Value, keys: &[&str]) -> Option<Value> {
    let mut obj = serde_json::Map::new();
    for &k in keys {
        if let Some(v) = args.get(k).filter(|v| v.is_number()) {
            obj.insert(k.to_string(), v.clone());
        }
    }
    if obj.is_empty() {
        None
    } else {
        Some(Value::Object(obj))
    }
}

fn call_daemon(
    socket: Option<&Path>,
    verb: &str,
    params: Option<Value>,
) -> Result<Value, Fail> {
    let stream = connect(socket)?;
    call_once(stream, verb, params)
}

// --- MCP envelope helpers ---------------------------------------------------

fn ok_resp(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_resp(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// Successful tool result: the daemon's JSON as a pretty-printed text block.
fn tool_success(result: &Value) -> Value {
    let text = serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": message } ],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_client_version() {
        let r = initialize_result(&json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(r["protocolVersion"], "2025-06-18");
        assert_eq!(r["serverInfo"]["name"], "drowse");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_our_version() {
        let r = initialize_result(&json!({}));
        assert_eq!(r["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_exposes_the_three_read_tools() {
        let tools = tool_list();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["sleep_efficiency", "wakeup_offenders", "check_sources"]);
        // Every tool must carry a schema so an agent can form valid arguments.
        for t in tools.as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().unwrap().len() > 20);
        }
    }

    #[test]
    fn build_call_maps_tools_to_verbs() {
        assert_eq!(build_call("check_sources", &json!({})).unwrap().0, "system.check");
        assert_eq!(build_call("sleep_efficiency", &json!({})).unwrap().0, "score.get");
        assert_eq!(build_call("wakeup_offenders", &json!({})).unwrap().0, "offenders.get");
        assert!(build_call("apply_fix", &json!({})).is_none());
    }

    #[test]
    fn build_call_forwards_only_known_numbers() {
        let (_, p) = build_call("wakeup_offenders", &json!({ "window_s": 30, "limit": 5 })).unwrap();
        assert_eq!(p.unwrap(), json!({ "window_s": 30, "limit": 5 }));

        // No args -> no params object.
        let (_, p) = build_call("sleep_efficiency", &json!({})).unwrap();
        assert!(p.is_none());

        // Wrong type is dropped (schema would reject it anyway).
        let (_, p) = build_call("sleep_efficiency", &json!({ "window_s": "lots" })).unwrap();
        assert!(p.is_none());
    }

    #[test]
    fn tools_list_request_routes_without_a_daemon() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            None,
        )
        .expect("request gets a response");
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn initialized_notification_gets_no_reply() {
        assert!(handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, None).is_none());
    }

    #[test]
    fn unknown_request_is_method_not_found() {
        let resp = handle_message(r#"{"jsonrpc":"2.0","id":9,"method":"frobnicate"}"#, None).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn parse_error_reports_null_id() {
        let resp = handle_message("{not json", None).unwrap();
        assert_eq!(resp["error"]["code"], -32700);
        assert_eq!(resp["id"], Value::Null);
    }

    #[test]
    fn unknown_tool_is_invalid_params() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope"}}"#,
            None,
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn tool_error_shape_is_iserror() {
        let e = tool_error("daemon down");
        assert_eq!(e["isError"], true);
        assert_eq!(e["content"][0]["type"], "text");
        assert_eq!(e["content"][0]["text"], "daemon down");
    }

    #[test]
    fn tool_success_embeds_pretty_json() {
        let s = tool_success(&json!({ "schema": 1, "grade": "C" }));
        assert_eq!(s["isError"], false);
        let text = s["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"grade\""));
    }
}
