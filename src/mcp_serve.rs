//! `amty mcp-serve` — expose the running amty instance as an MCP server (stdio).
//! External agents (Claude Code etc.) can then drive amty: send prompts,
//! read sessions, change settings, resolve approvals.
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn tool_defs() -> Vec<Value> {
    [
    (
        "amty_status",
        "Get amty app status: version, active provider/model, sessions, MCP servers.",
        json!({"type": "object", "properties": {}}),
    ),
    (
        "amty_list_sessions",
        "List chat sessions (id, title, message count, provider).",
        json!({"type": "object", "properties": {}}),
    ),
    (
        "amty_new_session",
        "Create a new chat session. Returns its id.",
        json!({"type": "object", "properties": {
            "provider": {"type": "string"}, "model": {"type": "string"}, "title": {"type": "string"}}}),
    ),
    (
        "amty_send",
        "Send a prompt to a session and (by default) wait for the full reply. The agent can use file/shell/MCP tools, subject to the app's approval policy.",
        json!({"type": "object", "properties": {
            "session": {"type": "string", "description": "session id or unique prefix; empty = most recent"},
            "text": {"type": "string"},
            "wait": {"type": "boolean", "description": "wait for completion (default true)"}
        }, "required": ["text"]}),
    ),
    (
        "amty_read",
        "Read recent messages of a session.",
        json!({"type": "object", "properties": {
            "session": {"type": "string"}, "last": {"type": "integer", "default": 20}}}),
    ),
    (
        "amty_get_config",
        "Read amty configuration (api keys masked).",
        json!({"type": "object", "properties": {"key": {"type": "string"}}}),
    ),
    (
        "amty_set_config",
        "Change an amty setting. Keys: provider, model, approval, system_prompt, max_tokens, allow_commands, allow_paths, provider.<name>.<field>, mcp.<name>.enabled, search.<field>.",
        json!({"type": "object", "properties": {
            "key": {"type": "string"}, "value": {"type": "string"}}, "required": ["key", "value"]}),
    ),
    (
        "amty_unset_config",
        "Delete an amty setting (e.g. mcp.<name>, provider.<name>, provider.<name>.<field>, search.<field>).",
        json!({"type": "object", "properties": {
            "key": {"type": "string"}}, "required": ["key"]}),
    ),
    (
        "amty_mcp_remove",
        "Remove an MCP server from amty's config entirely (disconnects it).",
        json!({"type": "object", "properties": {
            "name": {"type": "string"}}, "required": ["name"]}),
    ),
    (
        "amty_list_approvals",
        "List pending tool approvals (file writes, shell commands) awaiting a decision.",
        json!({"type": "object", "properties": {}}),
    ),
    (
        "amty_resolve_approval",
        "Approve or deny a pending tool call.",
        json!({"type": "object", "properties": {
            "id": {"type": "string"}, "allow": {"type": "boolean"}}, "required": ["id", "allow"]}),
    ),
    (
        "amty_cancel",
        "Cancel a running turn in a session.",
        json!({"type": "object", "properties": {"session": {"type": "string"}}, "required": []}),
    ),
]
    .iter()
    .map(|(n, d, s): &(&str, &str, Value)| json!({"name": n, "description": d, "inputSchema": s}))
    .collect()
}

fn tool_result(v: Value) -> Value {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
    json!({"content": [{"type": "text", "text": text}]})
}

async fn call_tool(name: &str, args: &Value) -> Result<Value> {
    let (base, tok) = crate::cli::runtime_info()?;
    match name {
        "amty_status" => crate::cli::get(&base, &tok, "/v1/status").await,
        "amty_list_sessions" => crate::cli::get(&base, &tok, "/v1/sessions").await,
        "amty_new_session" => {
            crate::cli::post(&base, &tok, "/v1/sessions", json!({
                "provider": args["provider"], "model": args["model"], "title": args["title"],
            }))
            .await
        }
        "amty_send" => {
            let sid = args["session"].as_str().unwrap_or("");
            let sid = if sid.is_empty() {
                let v = crate::cli::get(&base, &tok, "/v1/sessions").await?;
                match v[0]["id"].as_str() {
                    Some(id) => id.to_string(),
                    None => {
                        let v = crate::cli::post(&base, &tok, "/v1/sessions", json!({})).await?;
                        v["id"].as_str().unwrap_or("").to_string()
                    }
                }
            } else {
                sid.to_string()
            };
            let wait = args["wait"].as_bool().unwrap_or(true);
            crate::cli::post(
                &base, &tok,
                &format!("/v1/sessions/{sid}/messages"),
                json!({"text": args["text"].as_str().unwrap_or(""), "wait": wait}),
            )
            .await
        }
        "amty_read" => {
            let sid = args["session"].as_str().unwrap_or("");
            let last = args["last"].as_u64().unwrap_or(20) as usize;
            let v = crate::cli::get(&base, &tok, &format!("/v1/sessions/{sid}")).await?;
            let msgs = v["messages"].as_array().cloned().unwrap_or_default();
            let tail: Vec<Value> = msgs.into_iter().rev().take(last).rev().collect();
            Ok(json!({"session": v["id"], "title": v["title"], "messages": tail}))
        }
        "amty_get_config" => {
            let v = crate::cli::get(&base, &tok, "/v1/config").await?;
            match args["key"].as_str() {
                None => Ok(v),
                Some(k) => {
                    let mut cur = &v;
                    for p in k.split('.') {
                        cur = cur.get(p).unwrap_or(&Value::Null);
                    }
                    Ok(cur.clone())
                }
            }
        }
        "amty_set_config" => {
            crate::cli::post(&base, &tok, "/v1/config", json!({
                "key": args["key"], "value": args["value"],
            }))
            .await
        }
        "amty_unset_config" => {
            let key = args["key"].as_str().unwrap_or("");
            crate::cli::delete(&base, &tok, &format!("/v1/config/{key}")).await
        }
        "amty_mcp_remove" => {
            let name = args["name"].as_str().unwrap_or("");
            crate::cli::delete(&base, &tok, &format!("/v1/mcp/{name}")).await
        }
        "amty_list_approvals" => crate::cli::get(&base, &tok, "/v1/approvals").await,
        "amty_resolve_approval" => {
            crate::cli::post(
                &base, &tok,
                &format!("/v1/approvals/{}", args["id"].as_str().unwrap_or("")),
                json!({"allow": args["allow"].as_bool().unwrap_or(false)}),
            )
            .await
        }
        "amty_cancel" => {
            let sid = args["session"].as_str().unwrap_or("");
            crate::cli::post(&base, &tok, &format!("/v1/sessions/{sid}/cancel"), json!({})).await
        }
        _ => Ok(json!({"error": format!("unknown tool {name}")})),
    }
}

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let id = v.get("id").cloned();
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let resp = match method {
            "initialize" => id.map(|id| json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": v["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "amty", "version": env!("CARGO_PKG_VERSION")},
                }
            })),
            "ping" => id.map(|id| json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            "tools/list" => id.map(|id| json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"tools": tool_defs()}
            })),
            "tools/call" => id.map(|id| {
                let name = v["params"]["name"].as_str().unwrap_or("");
                let args = v["params"]["arguments"].clone();
                let args = if args.is_null() { json!({}) } else { args };
                match rt.block_on(call_tool(name, &args)) {
                    Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": tool_result(v)}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": e.to_string()}], "isError": true}}),
                }
            }),
            _ => {
                if method.starts_with("notifications/") || id.is_none() {
                    None
                } else {
                    id.map(|id| json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}}))
                }
            }
        };
        if let Some(r) = resp {
            let _ = writeln!(out, "{}", serde_json::to_string(&r).unwrap_or_default());
            let _ = out.flush();
        }
    }
    Ok(())
}
