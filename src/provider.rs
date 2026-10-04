use crate::config::ProviderConf;
use crate::types::*;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>);
}

/// Connect fails fast (20s); a stalled stream gives up after 180s without data.
fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(Into::into)
}

pub fn build(conf: &ProviderConf) -> Box<dyn Provider> {
    match conf.kind {
        crate::config::ProviderKind::Anthropic => Box::new(Anthropic { conf: conf.clone() }),
        crate::config::ProviderKind::OpenAi => Box::new(OpenAi { conf: conf.clone() }),
        crate::config::ProviderKind::CommandCode => Box::new(CommandCode { conf: conf.clone() }),
    }
}

/// Incremental SSE decoder: yields (event, data) pairs.
struct Sse {
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    ev: String,
    data: String,
    have: bool,
}

impl Sse {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(resp.bytes_stream()),
            buf: Vec::new(),
            ev: String::new(),
            data: String::new(),
            have: false,
        }
    }

    /// Returns Ok(None) at end of stream.
    async fn next(&mut self) -> Result<Option<(String, String)>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..pos).collect();
                self.buf.drain(..1); // consume the '\n'
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches('\r');
                if line.is_empty() {
                    if self.have {
                        let out = (std::mem::take(&mut self.ev), std::mem::take(&mut self.data));
                        self.have = false;
                        return Ok(Some(out));
                    }
                    continue;
                }
                if line.starts_with(':') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("event:") {
                    self.ev = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    self.have = true;
                }
                continue;
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(anyhow!("stream error: {e}")),
                None => {
                    if self.have {
                        let out = (std::mem::take(&mut self.ev), std::mem::take(&mut self.data));
                        self.have = false;
                        return Ok(Some(out));
                    }
                    return Ok(None);
                }
            }
        }
    }
}

// ---------------- Anthropic ----------------

struct Anthropic {
    conf: ProviderConf,
}

fn anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    ensure_tool_results(messages)
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content: Vec<Value> = m
                .blocks
                .iter()
                .map(|b| match b {
                    Block::Text { text } => json!({"type": "text", "text": text}),
                    Block::ToolUse { id, name, input } => {
                        json!({"type": "tool_use", "id": id, "name": name, "input": input})
                    }
                    Block::ToolResult { tool_use_id, content, is_error } => {
                        json!({"type": "tool_result", "tool_use_id": tool_use_id,
                               "content": content, "is_error": is_error})
                    }
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

#[async_trait]
impl Provider for Anthropic {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) {
        if let Err(e) = self.run(req, &tx).await {
            let _ = tx.send(StreamEvent::Err(e.to_string()));
        }
    }
}

impl Anthropic {
    async fn run(&self, req: &ChatRequest, tx: &mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let key = self.conf.resolved_key().ok_or_else(|| anyhow!("no api key for anthropic provider"))?;
        let body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "system": req.system,
            "messages": anthropic_messages(&req.messages),
            "tools": req.tools.iter().map(|t| json!({
                "name": t.name, "description": t.description, "input_schema": t.schema,
            })).collect::<Vec<_>>(),
        });
        let client = http()?;
        let mut rb = client
            .post(format!("{}/v1/messages", self.conf.resolved_base()))
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        if key.starts_with("sk-ant-oat") {
            rb = rb
                .bearer_auth(&key)
                .header("anthropic-beta", "oauth-2025-04-20");
        }
        for (k, v) in &self.conf.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.json(&body).send().await.context("anthropic request failed (check network/proxy)")?;
        if !resp.status().is_success() {
            bail!("anthropic HTTP {}: {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        let mut sse = Sse::new(resp);
        let mut tools: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut index = 0usize;
        while let Some((ev, data)) = sse.next().await? {
            match ev.as_str() {
                "content_block_start" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        index = v["index"].as_u64().unwrap_or(0) as usize;
                        if v["content_block"]["type"] == "tool_use" {
                            tools.insert(
                                index,
                                (
                                    v["content_block"]["id"].as_str().unwrap_or("").into(),
                                    v["content_block"]["name"].as_str().unwrap_or("").into(),
                                    String::new(),
                                ),
                            );
                        }
                    }
                }
                "content_block_delta" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        let delta = &v["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                if let Some(t) = delta["text"].as_str() {
                                    let _ = tx.send(StreamEvent::Text(t.into()));
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(t) = delta["partial_json"].as_str() {
                                    if let Some(slot) = tools.get_mut(&index) {
                                        slot.2.push_str(t);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "message_stop" => {
                    for (_, (id, name, js)) in std::mem::take(&mut tools) {
                        let input = if js.is_empty() { json!({}) } else { serde_json::from_str(&js).unwrap_or(json!({})) };
                        let _ = tx.send(StreamEvent::ToolUse { id, name, input });
                    }
                    let _ = tx.send(StreamEvent::Done);
                    return Ok(());
                }
                "error" => {
                    bail!("anthropic stream error: {data}");
                }
                _ => {}
            }
        }
        let _ = tx.send(StreamEvent::Done);
        Ok(())
    }
}

// ---------------- OpenAI-compatible ----------------

struct OpenAi {
    conf: ProviderConf,
}

fn openai_messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(sys) = &req.system {
        out.push(json!({"role": "system", "content": sys}));
    }
    for m in &ensure_tool_results(&req.messages) {
        match m.role {
            Role::User => {
                let mut text = String::new();
                for b in &m.blocks {
                    match b {
                        Block::Text { text: t } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                        Block::ToolResult { tool_use_id, content, .. } => {
                            if !text.is_empty() {
                                out.push(json!({"role": "user", "content": std::mem::take(&mut text)}));
                            }
                            out.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut calls = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::Text { text: t } => text.push_str(t),
                        Block::ToolUse { id, name, input } => calls.push(json!({
                            "id": id, "type": "function",
                            "function": {"name": name, "arguments": serde_json::to_string(input).unwrap_or_default()},
                        })),
                        _ => {}
                    }
                }
                let mut msg = json!({"role": "assistant"});
                if text.is_empty() {
                    msg["content"] = Value::Null;
                } else {
                    msg["content"] = json!(text);
                }
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                out.push(msg);
            }
        }
    }
    out
}

#[async_trait]
impl Provider for OpenAi {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) {
        if let Err(e) = self.run(req, &tx).await {
            let _ = tx.send(StreamEvent::Err(e.to_string()));
        }
    }
}

impl OpenAi {
    async fn run(&self, req: &ChatRequest, tx: &mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let body = json!({
            "model": req.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": openai_messages(req),
            "tools": req.tools.iter().map(|t| json!({
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.schema},
            })).collect::<Vec<_>>(),
        });
        let client = http()?;
        let mut rb = client
            .post(format!("{}/chat/completions", self.conf.resolved_base()))
            .header("content-type", "application/json");
        if let Some(key) = self.conf.resolved_key() {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &self.conf.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.json(&body).send().await.context("openai request failed (check network/proxy)")?;
        if !resp.status().is_success() {
            bail!("openai HTTP {}: {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        let mut sse = Sse::new(resp);
        let mut tools: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        while let Some((_ev, data)) = sse.next().await? {
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            let Some(choice) = v["choices"].get(0) else { continue };
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str() {
                if !t.is_empty() {
                    let _ = tx.send(StreamEvent::Text(t.into()));
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for c in calls {
                    let idx = c["index"].as_u64().unwrap_or(0) as usize;
                    let slot = tools.entry(idx).or_default();
                    if let Some(id) = c["id"].as_str() {
                        slot.0 = id.into();
                    }
                    if let Some(n) = c["function"]["name"].as_str() {
                        slot.1 = n.into();
                    }
                    if let Some(a) = c["function"]["arguments"].as_str() {
                        slot.2.push_str(a);
                    }
                }
            }
        }
        for (_, (id, name, js)) in std::mem::take(&mut tools) {
            let input = if js.is_empty() { json!({}) } else { serde_json::from_str(&js).unwrap_or(json!({})) };
            let id = if id.is_empty() { format!("call_{}", uuid::Uuid::new_v4()) } else { id };
            let _ = tx.send(StreamEvent::ToolUse { id, name, input });
        }
        let _ = tx.send(StreamEvent::Done);
        Ok(())
    }
}

// ---------------- Command Code ----------------
//
// POST {base}/alpha/generate with NDJSON (line-delimited JSON) streaming —
// the same protocol the `cmdc` CLI speaks. Auth: Bearer token from
// ~/.commandcode/auth.json (or api_key / api_key_env overrides).

struct CommandCode {
    conf: ProviderConf,
}

fn chrono_iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = {
        let days = secs / 86400;
        let rem = secs % 86400;
        let z = days as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = (z - era * 146097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mo <= 2 { y + 1 } else { y };
        (y, mo, d, rem / 3600, rem % 3600 / 60, rem % 60)
    };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn cc_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    // tool_call_id -> toolName, needed to name tool-result blocks
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in &ensure_tool_results(messages) {
        match m.role {
            Role::Assistant => {
                let mut content = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::Text { text } => content.push(json!({"type": "text", "text": text})),
                        Block::ToolUse { id, name, input } => {
                            names.insert(id.clone(), name.clone());
                            content.push(json!({"type": "tool-call", "toolCallId": id,
                                                "toolName": name, "input": input}));
                        }
                        Block::ToolResult { .. } => {}
                    }
                }
                out.push(json!({"role": "assistant", "content": content}));
            }
            Role::User => {
                let mut user = Vec::new();
                let mut results = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::Text { text } => user.push(json!({"type": "text", "text": text})),
                        Block::ToolResult { tool_use_id, content, is_error } => results.push(json!({
                            "type": "tool-result",
                            "toolCallId": tool_use_id,
                            "toolName": names.get(tool_use_id).cloned().unwrap_or_else(|| "unknown".into()),
                            "output": if *is_error {
                                json!({"type": "error-text", "value": content})
                            } else {
                                json!({"type": "text", "value": content})
                            },
                        })),
                        _ => {}
                    }
                }
                if !results.is_empty() {
                    out.push(json!({"role": "tool", "content": results}));
                }
                if !user.is_empty() {
                    out.push(json!({"role": "user", "content": user}));
                }
            }
        }
    }
    out
}

#[async_trait]
impl Provider for CommandCode {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) {
        if let Err(e) = self.run(req, &tx).await {
            let _ = tx.send(StreamEvent::Err(e.to_string()));
        }
    }
}

/// Locate the `cmdc` CLI. On Windows the npm install drops a `cmdc.cmd` shim
/// under %APPDATA%\npm that a bare `Command::new("cmdc")` won't resolve
/// (no .cmd extension lookup), and a GUI process's PATH may predate the
/// install — probe `where` first, then the well-known locations.
fn cmdc_exe() -> Option<String> {
    #[cfg(not(target_os = "windows"))]
    {
        Some("cmdc".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(o) = std::process::Command::new("where").arg("cmdc").output() {
            if o.status.success() {
                // npm's dir holds `cmdc` (sh script), `cmdc.cmd`, `cmdc.ps1`;
                // only the .cmd/.exe forms are spawnable directly
                if let Some(p) = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .find(|l| l.ends_with(".cmd") || l.ends_with(".exe") || l.ends_with(".bat"))
                {
                    return Some(p);
                }
            }
        }
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let user = std::env::var("USERPROFILE").unwrap_or_default();
        for p in [
            format!(r"{appdata}\npm\cmdc.cmd"),
            format!(r"{local}\Programs\cmdc\cmdc.exe"),
            format!(r"{user}\scoop\shims\cmdc.exe"),
            format!(r"{local}\fnm\aliases\default\cmdc.cmd"),
        ] {
            if std::path::Path::new(&p).exists() {
                return Some(p);
            }
        }
        None
    }
}

/// `cmdc <args>` → stdout, "" on any failure. When cmdc was resolved to a
/// fallback path instead of PATH, prepend the nodejs dirs so the .cmd shim
/// can still find `node`.
fn cmdc_output(args: &[&str]) -> Result<String, String> {
    let Some(exe) = cmdc_exe() else { return Err("cmdc not found".into()) };
    let mut c = std::process::Command::new(&exe);
    c.args(args);
    inject_node_path(&exe, &mut c);
    match c.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.is_empty() {
                s.push_str(&String::from_utf8_lossy(&o.stderr));
            }
            Ok(s)
        }
        Err(e) => Err(format!("spawn {}: {}", exe, e)),
    }
}

/// When cmdc was resolved to a fallback path instead of PATH, prepend the
/// nodejs dirs so the .cmd shim can still find `node`.
#[cfg(target_os = "windows")]
fn inject_node_path(exe: &str, c: &mut std::process::Command) {
    let mut path = std::env::var("PATH").unwrap_or_default();
    if let Some(dir) = std::path::Path::new(exe).parent() {
        path = format!("{};{path}", dir.display());
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        path = format!(r"{pf}\nodejs;{path}");
    }
    c.env("PATH", path);
}

#[cfg(not(target_os = "windows"))]
fn inject_node_path(_exe: &str, _c: &mut std::process::Command) {}

fn cmdc_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let s = cmdc_output(&["--version"]).unwrap_or_default().trim().to_string();
        if s.is_empty() { "1.74.0".into() } else { s }
    })
}

impl CommandCode {
    async fn run(&self, req: &ChatRequest, tx: &mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let key = self.conf.resolved_key()
            .ok_or_else(|| anyhow!("no Command Code api key — run `cmdc` once to sign in, or set providers.commandcode.api_key"))?;
        let system: Value = match &req.system {
            Some(s) => json!([{ "type": "text", "text": s }]),
            None => Value::Null,
        };
        // Server validates this `config` env block and rejects missing fields.
        let cwd = std::env::current_dir().unwrap_or_default();
        let git = |args: &[&str]| -> String {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        let is_repo = !branch.is_empty();
        let body = json!({
            "config": {
                "workingDir": cwd.to_string_lossy(),
                "date": chrono_iso_now(),
                "environment": std::env::consts::OS,
                "structure": [],
                "isGitRepo": is_repo,
                "currentBranch": branch,
                "mainBranch": "main",
                "gitStatus": git(&["status", "--porcelain"]),
                "recentCommits": git(&["log", "--oneline", "-5"]).lines().collect::<Vec<_>>(),
            },
            "memory": null, "taste": null, "skills": null,
            "permissionMode": "standard",
            "mode": "agent",
            "promptCache": "off",
            "params": {
                "model": req.model,
                "messages": cc_messages(&req.messages),
                "tools": req.tools.iter().map(|t| json!({
                    "name": t.name, "description": t.description, "input_schema": t.schema,
                })).collect::<Vec<_>>(),
                "system": system,
                "max_tokens": req.max_tokens,
                "stream": true,
            },
        });
        let client = http()?;
        let mut rb = client
            .post(format!("{}/alpha/generate", self.conf.resolved_base()))
            .header("content-type", "application/json")
            .header("user-agent", "cli")
            .header("x-cli-environment", "production")
            .header("x-command-code-version", cmdc_version())
            .bearer_auth(&key);
        for (k, v) in &self.conf.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.json(&body).send().await.context("commandcode request failed (check network/proxy; base_url should be https://api.commandcode.ai)")?;
        if !resp.status().is_success() {
            bail!("commandcode HTTP {}: {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        // NDJSON stream — one JSON object per line.
        let mut stream = Box::pin(resp.bytes_stream());
        let mut buf = Vec::<u8>::new();
        let mut finished = false;
        'outer: loop {
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
                match ev["type"].as_str() {
                    Some("text-delta") => {
                        if let Some(t) = ev["text"].as_str() {
                            if !t.is_empty() {
                                let _ = tx.send(StreamEvent::Text(t.into()));
                            }
                        }
                    }
                    Some("tool-call") => {
                        let input = ev.get("input").or_else(|| ev.get("args")).cloned().unwrap_or(json!({}));
                        let _ = tx.send(StreamEvent::ToolUse {
                            id: ev["toolCallId"].as_str().unwrap_or("").into(),
                            name: ev["toolName"].as_str().unwrap_or("").into(),
                            input,
                        });
                    }
                    Some("finish") => {
                        finished = true;
                        break 'outer;
                    }
                    Some("error") => {
                        let msg = if let Some(s) = ev["error"].as_str() {
                            s.to_string()
                        } else {
                            ev["error"]["message"].as_str().unwrap_or("stream error").to_string()
                        };
                        bail!("commandcode stream error: {msg}");
                    }
                    Some("abort") => break 'outer,
                    _ => {}
                }
            }
            match stream.next().await {
                Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(anyhow!("stream error: {e}")),
                None => break,
            }
        }
        if !finished {
            // tolerate EOF without an explicit finish event, but surface it
            let _ = tx.send(StreamEvent::Text("\n[stream ended without finish event]".into()));
        }
        let _ = tx.send(StreamEvent::Done);
        Ok(())
    }
}

/// Known model ids for the model picker. Command Code is enumerated live via
/// `cmdc --list-models` (cached); other providers use a static shortlist —
/// the field stays free-text so anything works.
pub fn model_suggestions(kind: crate::config::ProviderKind) -> Vec<String> {
    use crate::config::ProviderKind as K;
    match kind {
        K::CommandCode => FALLBACK_CC_MODELS.iter().map(|s| s.to_string()).collect(),
        K::Anthropic => [
            "claude-sonnet-4-5",
            "claude-opus-4-1",
            "claude-haiku-4-5",
            "claude-sonnet-4-0",
        ].iter().map(|s| s.to_string()).collect(),
        K::OpenAi => [
            "gpt-5", "gpt-5-mini", "gpt-5-nano", "o4-mini",
        ].iter().map(|s| s.to_string()).collect(),
    }
}

/// Returns (models, error) — error=Some(msg) means the API fetch failed.
/// Fetches the live model list from GET {base}/provider/v1/models (no `cmdc`
/// CLI needed). Cached on success so the GUI doesn't hit the API every frame.
pub fn cmdc_models(conf: &crate::config::ProviderConf) -> (Vec<String>, Option<String>) {
    static CACHE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);
    if let Some(models) = CACHE.lock().unwrap().clone() {
        return (models, None);
    }
    let url = format!("{}/provider/v1/models", conf.resolved_base().trim_end_matches('/'));
    let key = conf.resolved_key();
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let models = FALLBACK_CC_MODELS.iter().map(|s| s.to_string()).collect();
            return (models, Some(format!("http client: {e}")));
        }
    };
    let mut req = client.get(&url);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            let models = FALLBACK_CC_MODELS.iter().map(|s| s.to_string()).collect();
            return (models, Some(format!("fetch {url}: {e}")));
        }
    };
    let value: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            let models = FALLBACK_CC_MODELS.iter().map(|s| s.to_string()).collect();
            return (models, Some(format!("json parse: {e}")));
        }
    };
    let models: Vec<String> = value["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    if models.is_empty() {
        let models = FALLBACK_CC_MODELS.iter().map(|s| s.to_string()).collect();
        (models, Some("empty model list".into()))
    } else {
        *CACHE.lock().unwrap() = Some(models.clone());
        (models, None)
    }
}

/// Last-resort model list when `cmdc` can't be run (e.g. not installed on
/// Windows). The model field stays free-text; these are just menu entries.
const FALLBACK_CC_MODELS: &[&str] = &[
    "deepseek/deepseek-v4-flash",
    "deepseek/deepseek-v4-pro",
    "moonshotai/kimi-k3",
    "moonshotai/kimi-k2.5",
    "z-ai/glm-5.3-flash",
    "zai-org/glm-5.3",
    "claude-sonnet-5-5",
    "claude-opus-5-5",
    "claude-haiku-4-5",
    "gpt-5.5",
    "gpt-5.4-mini",
    "google/gemini-3.8-flash",
    "xai/grok-4.7",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn req_msgs() -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("hi"),
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![
                    Block::Text { text: "reading".into() },
                    Block::ToolUse { id: "t1".into(), name: "fs_read".into(), input: json!({"path": "/a"}) },
                ],
            },
            ChatMessage::tool_results(vec![Block::ToolResult {
                tool_use_id: "t1".into(), content: "data".into(), is_error: false,
            }]),
        ]
    }

    #[test]
    fn anthropic_shape() {
        let v = anthropic_messages(&req_msgs());
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[1]["content"][0]["type"], "text");
        assert_eq!(v[1]["content"][1]["type"], "tool_use");
        assert_eq!(v[2]["role"], "user");
        assert_eq!(v[2]["content"][0]["type"], "tool_result");
        assert_eq!(v[2]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn openai_shape() {
        let r = ChatRequest {
            model: "m".into(), max_tokens: 1, system: Some("sys".into()),
            messages: req_msgs(), tools: vec![],
        };
        let v = openai_messages(&r);
        assert_eq!(v[0]["role"], "system");
        assert_eq!(v[1]["content"], "hi");
        assert_eq!(v[2]["tool_calls"][0]["id"], "t1");
        assert_eq!(v[2]["tool_calls"][0]["function"]["name"], "fs_read");
        assert_eq!(v[3]["role"], "tool");
        assert_eq!(v[3]["tool_call_id"], "t1");
    }

    #[test]
    #[ignore] // requires network + api key
    fn cmdc_models_real() {
        let conf = crate::config::ProviderConf {
            kind: crate::config::ProviderKind::CommandCode,
            model: "deepseek/deepseek-v4-flash".into(),
            ..Default::default()
        };
        let (models, err) = cmdc_models(&conf);
        eprintln!("parsed {} models (err={:?})", models.len(), err);
        eprintln!("{:?}", models);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::config::{ProviderConf, ProviderKind};

    // Requires mock_openai.py running on :18311
    #[tokio::test]
    #[ignore]
    async fn openai_stream_against_mock() {
        let conf = ProviderConf {
            kind: ProviderKind::OpenAi,
            model: "mock-model".into(),
            base_url: Some("http://127.0.0.1:18311/v1".into()),
            ..Default::default()
        };
        let p = build(&conf);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = ChatRequest {
            model: "mock-model".into(),
            max_tokens: 100,
            system: None,
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
        };
        p.stream(&req, tx).await;
        while let Some(ev) = rx.recv().await {
            println!("EV: {ev:?}");
            if matches!(ev, StreamEvent::Done | StreamEvent::Err(_)) {
                break;
            }
        }
    }
}
