//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0): initialize,
//! ping, tools/list and tools/call. stdout carries protocol messages only;
//! diagnostics go to stderr.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::dispatch::Dispatcher;

const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "deskpilot reads and drives desktop applications. Work from cheap to expensive and from least to most invasive:\n\
1. Discover with list_windows / list_apps, then find_elements or get_tree (compact accessibility text; [ref] values identify elements).\n\
2. Change things with `act` (press, select, toggle, check, uncheck, expand, collapse, set_value, focus, scroll_into_view). It escalates by itself: accessibility action → window messages → keyboard with verified focus navigation → real mouse, and reports each attempt. Lower `max_strategy` to forbid stealing the user's focus.\n\
3. Only when an app exposes no accessibility data: screenshot / ocr (expensive in tokens) and `mouse` (moves the user's pointer).\n\
Windows of processes running as administrator (flag `admin` in list_windows) need \"elevated\": true; the first such call shows a UAC prompt to the user.";

pub fn serve(dispatcher: &Dispatcher) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch
                    .into_iter()
                    .filter_map(|m| handle(dispatcher, m))
                    .collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(message) => handle(dispatcher, message),
            Err(e) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Handles one JSON-RPC message; `None` for notifications and responses.
pub fn handle(dispatcher: &Dispatcher, message: Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?.to_string();
    let id = message.get("id").cloned()?; // no id = notification
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    Some(match method.as_str() {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(SUPPORTED_VERSIONS[0]);
            let version = SUPPORTED_VERSIONS
                .iter()
                .find(|v| **v == requested)
                .copied()
                .unwrap_or(SUPPORTED_VERSIONS[0]);
            ok_response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "deskpilot", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => ok_response(id, json!({})),
        "tools/list" => {
            let tools: Vec<Value> = dispatcher
                .tools()
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                        "annotations": {
                            "readOnlyHint": !t.mutating,
                            "destructiveHint": t.mutating,
                            "openWorldHint": false,
                        },
                    })
                })
                .collect();
            ok_response(id, json!({ "tools": tools }))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_response(id, -32602, "tools/call needs params.name"));
            };
            if !dispatcher.tools().iter().any(|t| t.name == name) {
                return Some(error_response(
                    id,
                    -32602,
                    &format!("unknown tool '{name}'"),
                ));
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = match dispatcher.call(name, args) {
                Ok(out) => {
                    let mut content = vec![json!({ "type": "text", "text": out.agent_text() })];
                    content.extend(out.images.iter().map(
                        |img| json!({ "type": "image", "data": img.data, "mimeType": img.mime }),
                    ));
                    json!({ "content": content, "isError": false })
                }
                Err(e) => json!({
                    "content": [{ "type": "text", "text": serde_json::to_string(&e.to_json()).unwrap_or_default() }],
                    "isError": true,
                }),
            };
            ok_response(id, result)
        }
        other => error_response(id, -32601, &format!("method not found: {other}")),
    })
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
