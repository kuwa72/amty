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
        crate::config::ProviderKind::OpenCode => Box::new(OpenCode { conf: conf.clone() }),
    }
}

/// Permanent-looking failures (4xx, missing credentials) are not retried.
fn retryable(e: &str) -> bool {
    !(e.contains("HTTP 4") || e.contains("no api key") || e.contains("not defined"))
}

/// Run a provider attempt with retry on transient failures (network resets,
/// decode errors, 5xx). `f` replays the whole request; already-forwarded
/// events are deduplicated — text is emitted only as the suffix beyond what
/// was sent (prefix-matched), tool_use beyond a name+input signature (the
/// agent layer also dedups execution). Mid-stream divergence after a retry
/// forwards raw deltas, which may duplicate a few tokens — preferable to a
/// dead run.
async fn retry_stream<F, Fut>(mut f: F, tx: &mpsc::UnboundedSender<StreamEvent>)
where
    F: FnMut(mpsc::UnboundedSender<StreamEvent>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    const MAX_ATTEMPTS: usize = 3;
    let mut forwarded = String::new();
    let mut cur = String::new();
    let mut tool_sigs = std::collections::HashSet::new();
    let mut delay = std::time::Duration::from_millis(400);
    for attempt in 1..=MAX_ATTEMPTS {
        let (rtx, mut rrx) = mpsc::unbounded_channel::<StreamEvent>();
        let mut run = std::pin::pin!(f(rtx));
        let mut res: Option<Result<()>> = None;
        let mut ev_err: Option<String> = None;
        // usage is accumulated per attempt and forwarded only if the attempt
        // succeeds: a retried attempt's tokens were still billed, but replaying
        // them into the sum would double-count the request the caller sees.
        let mut attempt_usage = crate::types::Usage::default();
        // `Done` is held back until the attempt is known to have succeeded, so
        // usage can be emitted *before* it: consumers stop at Done, so anything
        // queued after it is lost.
        let mut saw_done = false;
        cur.clear();
        loop {
            tokio::select! {
                biased;
                r = &mut run, if res.is_none() => res = Some(r),
                ev = rrx.recv() => match ev {
                    None => break,
                    Some(StreamEvent::Text(t)) => {
                        cur.push_str(&t);
                        if cur.starts_with(&forwarded) && cur.len() > forwarded.len() {
                            let d = cur[forwarded.len()..].to_string();
                            forwarded.push_str(&d);
                            let _ = tx.send(StreamEvent::Text(d));
                        } else if !cur.starts_with(&forwarded) {
                            forwarded.push_str(&t);
                            let _ = tx.send(StreamEvent::Text(t));
                        }
                    }
                    Some(StreamEvent::ToolUse { id, name, input }) => {
                        if tool_sigs.insert(format!("{name}{input}")) {
                            let _ = tx.send(StreamEvent::ToolUse { id, name, input });
                        }
                    }
                    Some(StreamEvent::Usage(u)) => {
                        // Buffer; forwarded once below only when this attempt is
                        // the one that survives (see `attempt_usage`).
                        attempt_usage.add(&u);
                    }
                    Some(StreamEvent::Done) => { saw_done = true; }
                    Some(StreamEvent::Err(e)) => { ev_err = Some(e); }
                }
            }
        }
        let res = match res {
            Some(r) => r,
            None => run.await,
        };
        let err = ev_err.or_else(|| res.err().map(|e| e.to_string()));
        match err {
            None => {
                // order matters: usage, then the terminal Done
                if !attempt_usage.is_zero() {
                    let _ = tx.send(StreamEvent::Usage(attempt_usage));
                }
                if saw_done {
                    let _ = tx.send(StreamEvent::Done);
                }
                return;
            }
            Some(e) if attempt == MAX_ATTEMPTS || !retryable(&e) => {
                let _ = tx.send(StreamEvent::Err(e));
                return;
            }
            Some(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(8));
            }
        }
    }
}

/// Incremental SSE decoder: yields (event, data) pairs.
struct Sse {
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    ev: String,
    pub data: String,
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
                    Block::Image { media_type, data, .. } => json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media_type, "data": data},
                    }),
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

#[async_trait]
impl Provider for Anthropic {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) {
        retry_stream(|rtx| self.run(req, rtx), &tx).await;
    }
}

/// system + volatile tail combined for providers without block-level caching.
fn system_full(req: &ChatRequest) -> Option<String> {
    let mut s = req.system.clone().unwrap_or_default();
    if let Some(t) = &req.system_tail {
        s.push_str(t);
    }
    (!s.is_empty()).then_some(s)
}

/// Prompt-cache-aware request body: Anthropic caches everything up to each
/// `cache_control` breakpoint (order: tools → system → messages). Breakpoints
/// sit at the end of the tools list, on the static system block, and on the
/// last message, so the tools+static-system prefix stays cached while the
/// volatile skills tail and new messages roll forward.
fn anthropic_body(req: &ChatRequest) -> Value {
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
        .collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    let mut system: Vec<Value> = Vec::new();
    if let Some(s) = req.system.as_deref().filter(|s| !s.is_empty()) {
        system.push(json!({"type": "text", "text": s, "cache_control": {"type": "ephemeral"}}));
    }
    if let Some(t) = req.system_tail.as_deref().filter(|t| !t.is_empty()) {
        system.push(json!({"type": "text", "text": t}));
    }
    let mut messages = anthropic_messages(&req.messages);
    if let Some(b) = messages
        .last_mut()
        .and_then(|m| m["content"].as_array_mut())
        .and_then(|c| c.last_mut())
    {
        b["cache_control"] = json!({"type": "ephemeral"});
    }
    json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "system": if system.is_empty() { Value::Null } else { json!(system) },
        "messages": messages,
        "tools": tools,
    })
}

/// Parse an Anthropic `usage` object. `phase` selects which half of the
/// response we are looking at: 0 = `message_start` (input + cache only, output
/// is not final yet), 1 = `message_delta` (output tokens). Splitting this way
/// keeps input tokens from being counted twice.
fn anthropic_usage(v: &Value, phase: u8) -> Option<crate::types::Usage> {
    if !v.is_object() {
        return None;
    }
    let num = |x: &Value| x.as_u64().unwrap_or(0);
    let u = if phase == 0 {
        crate::types::Usage {
            input: num(&v["input_tokens"]) + num(&v["cache_read_input_tokens"])
                + num(&v["cache_creation_input_tokens"]),
            output: 0,
            cache_read: num(&v["cache_read_input_tokens"]),
            cache_write: num(&v["cache_creation_input_tokens"]),
            reasoning: 0,
        }
    } else {
        crate::types::Usage { output: num(&v["output_tokens"]), ..Default::default() }
    };
    (!u.is_zero()).then_some(u)
}

impl Anthropic {
    async fn run(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let key = self.conf.resolved_key().ok_or_else(|| anyhow!("no api key for anthropic provider"))?;
        let body = anthropic_body(req);
        let client = http()?;
        // prompt caching is GA but older gateways still want the beta header
        let beta = if key.starts_with("sk-ant-oat") {
            "prompt-caching-2024-07-31,oauth-2025-04-20"
        } else {
            "prompt-caching-2024-07-31"
        };
        let mut rb = client
            .post(format!("{}/v1/messages", self.conf.resolved_base()))
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta)
            .header("content-type", "application/json");
        if key.starts_with("sk-ant-oat") {
            rb = rb.bearer_auth(&key);
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
                "message_start" => {
                    // Input-side usage arrives on message_start; output is only
                    // known at message_delta, so both are forwarded and the
                    // receiver sums them.
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        if let Some(u) = anthropic_usage(&v["message"]["usage"], 0) {
                            let _ = tx.send(StreamEvent::Usage(u));
                        }
                    }
                }
                "message_delta" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        if let Some(u) = anthropic_usage(&v["usage"], 1) {
                            let _ = tx.send(StreamEvent::Usage(u));
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
    if let Some(sys) = system_full(req) {
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
                        Block::Image { media_type, data, .. } => {
                            if !text.is_empty() {
                                out.push(json!({"role": "user", "content": std::mem::take(&mut text)}));
                            }
                            out.push(json!({"role": "user", "content": [{
                                "type": "image_url",
                                "image_url": {"url": format!("data:{media_type};base64,{data}")},
                            }]}));
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
        retry_stream(|rtx| self.run(req, rtx), &tx).await;
    }
}

impl OpenAi {
    async fn run(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
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
        let mut usage: Option<crate::types::Usage> = None;
        let mut saw_usage = false;
        while let Some((_ev, data)) = sse.next().await? {
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            // `include_usage` puts a usage-only frame at the end (no choices).
            if let Some(u) = openai_usage(&v["usage"]) {
                // keep the last (final) frame only
                usage = Some(u);
                saw_usage = true;
            }
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
        if saw_usage {
            if let Some(u) = usage {
                let _ = tx.send(StreamEvent::Usage(u));
            }
        }
        let _ = tx.send(StreamEvent::Done);
        Ok(())
    }
}

/// Parse an OpenAI-compatible `usage` object (`prompt_tokens` /
/// `completion_tokens` / `prompt_tokens_details.cached_tokens`).
fn openai_usage(v: &Value) -> Option<crate::types::Usage> {
    if !v.is_object() {
        return None;
    }
    let num = |x: &Value| x.as_u64().unwrap_or(0);
    let u = crate::types::Usage {
        input: num(&v["prompt_tokens"]),
        output: num(&v["completion_tokens"]),
        cache_read: num(&v["prompt_tokens_details"]["cached_tokens"]),
        cache_write: 0,
        reasoning: num(&v["completion_tokens_details"]["reasoning_tokens"]),
    };
    (!u.is_zero()).then_some(u)
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
                        // images are only meaningful on user turns; the agent
                        // never emits them, but ignore rather than panic
                        Block::Image { .. } => {}
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
                        // The /alpha/generate schema wants the Anthropic-style
                        // snake_case `media_type`; `mediaType` is rejected with
                        // a 400 "expected one of image/jpeg|png|gif|webp".
                        Block::Image { media_type, data, .. } => user.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": media_type, "data": data},
                        })),
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
        retry_stream(|rtx| self.run(req, rtx), &tx).await;
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
        if let Some(p) = crate::catalog::shim_where("cmdc") {
            return Some(p);
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

/// Parse a Command Code usage object into [`Usage`].
///
/// The server reports input split into cached and uncached halves
/// (`inputTokenDetails.cacheReadTokens` / `.noCacheTokens`); `inputTokens` is
/// their sum. Accepts the older flat `prompt_tokens`/`completion_tokens` shape
/// too, and returns `None` when nothing usable is present.
fn cc_usage(v: &Value) -> Option<crate::types::Usage> {
    if !v.is_object() {
        return None;
    }
    let num = |x: &Value| x.as_u64().unwrap_or(0);
    let input = num(&v["inputTokens"]).max(num(&v["prompt_tokens"]));
    let output = num(&v["outputTokens"]).max(num(&v["completion_tokens"]));
    let details = &v["inputTokenDetails"];
    let mut cache_read = num(&details["cacheReadTokens"]);
    let mut cache_write = num(&details["cacheWriteTokens"]);
    if cache_read == 0 {
        // Anthropic-style naming, seen on some routed models
        cache_read = num(&v["cacheRead"])
            .max(num(&details["cachedTokens"]))
            .max(num(&v["cache_read_input_tokens"]));
        cache_write = cache_write.max(num(&v["cache_creation_input_tokens"]));
    }
    let reasoning = num(&v["outputTokenDetails"]["reasoningTokens"])
        .max(num(&v["reasoningTokens"]));
    let u = crate::types::Usage { input, output, cache_read, cache_write, reasoning };
    (!u.is_zero()).then_some(u)
}

/// `cmdc <args>` → stdout, "" on any failure. When cmdc was resolved to a
/// fallback path instead of PATH, prepend the nodejs dirs so the .cmd shim
/// can still find `node`.
fn cmdc_output(args: &[&str]) -> Result<String, String> {
    let Some(exe) = cmdc_exe() else { return Err("cmdc not found".into()) };
    let mut c = std::process::Command::new(&exe);
    c.args(args);
    crate::catalog::no_window(&mut c);
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
    async fn run(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let key = self.conf.resolved_key()
            .ok_or_else(|| anyhow!("no Command Code api key — run `cmdc` once to sign in, or set providers.commandcode.api_key"))?;
        let system: Value = match system_full(req) {
            Some(s) => json!([{ "type": "text", "text": s }]),
            // the schema wants a string or an array, never null
            None => json!([]),
        };
        // Server validates this `config` env block and rejects missing fields.
        let cwd = std::env::current_dir().unwrap_or_default();
        let git = |args: &[&str]| -> String {
            let mut c = std::process::Command::new("git");
            crate::catalog::no_window(&mut c);
            c.args(args)
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
        // whether a per-step usage event was seen (see the `finish` arm)
        let mut saw_step_usage = false;
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
                    Some("finish-step") => {
                        // The token accounting the server has always sent and amty
                        // used to drop on the floor. Shape:
                        //   {"usage":{"inputTokens":N,
                        //             "inputTokenDetails":{"noCacheTokens":N,"cacheReadTokens":N},
                        //             "outputTokens":N,
                        //             "outputTokenDetails":{"textTokens":N,"reasoningTokens":N}}}
                        if let Some(u) = cc_usage(&ev["usage"]) {
                            saw_step_usage = true;
                            let _ = tx.send(StreamEvent::Usage(u));
                        }
                    }
                    Some("finish") => {
                        // Fallback only: `totalUsage` here duplicates the sum of
                        // the per-step events, so prefer those to avoid counting
                        // every token twice.
                        if !saw_step_usage {
                            if let Some(u) = cc_usage(&ev["totalUsage"]) {
                                let _ = tx.send(StreamEvent::Usage(u));
                            }
                        }
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

// ---------------- OpenCode ----------------
//
// Talks to a running `opencode` v2 server (`opencode serve` or the
// CLI-managed background service from `opencode service start`) over its
// HTTP API. There is no streaming generate endpoint, so the chat turn is a
// single request: POST {base}/api/experimental/generate with the whole
// transcript flattened into one `prompt` string; the response is
// `{data:{text}}` — no usage is reported.
//
// Endpoint resolution: `providers.opencode.base_url` wins; otherwise the
// background service is discovered via `opencode service status` (started
// once via `opencode service start` when absent). v2 servers always need
// HTTP basic auth — user "opencode", password taken from
// api_key/api_key_env or the service's ~/.config/opencode/service.json.
//
// Model picker strings are "providerID/modelID" as listed by GET
// /api/model; an empty/unqualified model omits `model` so the server uses
// its configured default.

struct OpenCode {
    conf: ProviderConf,
}

#[async_trait]
impl Provider for OpenCode {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) {
        retry_stream(|rtx| self.run(req, rtx), &tx).await;
    }
}

/// Flatten system + transcript into the single `prompt` string
/// /api/experimental/generate accepts. Ends on an `assistant:` cue so a
/// chat model continues the conversation rather than summarizing it.
fn oc_prompt(req: &ChatRequest) -> String {
    let mut s = String::new();
    if let Some(sys) = system_full(req) {
        s.push_str(&sys);
        s.push_str("\n\n");
    }
    for m in &ensure_tool_results(&req.messages) {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut wrote = false;
        for b in &m.blocks {
            let piece = match b {
                Block::Text { text } => text.clone(),
                Block::ToolUse { name, input, .. } => format!("[tool call: {name} {input}]"),
                Block::ToolResult { tool_use_id, content, is_error } => format!(
                    "[tool result{} {tool_use_id}]: {content}",
                    if *is_error { " (error)" } else { "" },
                ),
                Block::Image { .. } => "[image]".into(),
            };
            if piece.is_empty() {
                continue;
            }
            if !wrote {
                s.push_str(role);
                s.push_str(":\n");
                wrote = true;
            }
            s.push_str(&piece);
            s.push('\n');
        }
        if wrote {
            s.push('\n');
        }
    }
    s.push_str("assistant:\n");
    s
}

/// "providerID/modelID" → Model.Ref. Model ids may themselves contain '/'
/// (e.g. "commandcode/deepseek/deepseek-v4-flash"), so split once.
fn oc_model_ref(model: &str) -> Option<Value> {
    let (p, id) = model.split_once('/')?;
    if p.is_empty() || id.is_empty() {
        return None;
    }
    Some(json!({"providerID": p, "id": id}))
}

/// Locate the `opencode` CLI. Same PATHEXT problem as `cmdc` on Windows —
//  probe `where`, then the well-known install dirs.
fn opencode_exe() -> Option<String> {
    #[cfg(not(target_os = "windows"))]
    {
        Some("opencode".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(p) = crate::catalog::shim_where("opencode") {
            return Some(p);
        }
        let user = std::env::var("USERPROFILE").unwrap_or_default();
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        for p in [
            format!(r"{user}\.opencode\bin\opencode.exe"),
            format!(r"{appdata}\npm\opencode.cmd"),
            format!(r"{user}\scoop\shims\opencode.exe"),
        ] {
            if std::path::Path::new(&p).exists() {
                return Some(p);
            }
        }
        None
    }
}

fn opencode_output(args: &[&str]) -> Result<String, String> {
    let Some(exe) = opencode_exe() else { return Err("opencode not found".into()) };
    let mut c = std::process::Command::new(&exe);
    c.args(args);
    crate::catalog::no_window(&mut c);
    match c.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.is_empty() {
                s.push_str(&String::from_utf8_lossy(&o.stderr));
            }
            Ok(s)
        }
        Err(e) => Err(format!("spawn {exe}: {e}")),
    }
}

/// The background service's basic-auth password, written by `opencode
/// service start` to <config>/opencode/service.json.
fn opencode_service_password() -> Option<String> {
    let mut paths = Vec::new();
    if let Some(c) = dirs::config_dir() {
        paths.push(c.join("opencode/service.json"));
    }
    if let Some(h) = dirs::home_dir() {
        paths.push(h.join(".config/opencode/service.json"));
    }
    for p in paths {
        let Ok(t) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
        if let Some(pw) = v["password"].as_str().filter(|s| !s.is_empty()) {
            return Some(pw.into());
        }
    }
    None
}

#[derive(Clone)]
struct OcEndpoint {
    base: String,
    password: Option<String>,
    /// true when discovered via `opencode service` (not a configured
    /// base_url) — only these are cached, and evicted on connect failure.
    discovered: bool,
}

/// Cached service-discovery result; the GUI model picker calls this per
/// frame, so the `opencode service status` subprocess must not re-run each
/// time. Evicted on connection failure so a service restart re-discovers.
static OC_ENDPOINT: std::sync::Mutex<Option<OcEndpoint>> = std::sync::Mutex::new(None);

fn opencode_endpoint(conf: &ProviderConf) -> Result<OcEndpoint> {
    let password = conf.resolved_key().or_else(opencode_service_password);
    if let Some(b) = conf.base_url.as_deref().filter(|s| !s.is_empty()) {
        return Ok(OcEndpoint { base: b.trim_end_matches('/').into(), password, discovered: false });
    }
    if let Some(ep) = OC_ENDPOINT.lock().unwrap().clone() {
        return Ok(ep);
    }
    let status_url = || {
        opencode_output(&["service", "status"]).ok().and_then(|s| {
            s.split_whitespace().find(|t| t.starts_with("http")).map(str::to_string)
        })
    };
    let base = status_url().or_else(|| {
        // not running — `service start` is idempotent and prints the URL
        let out = opencode_output(&["service", "start"]).ok()?;
        out.split_whitespace().find(|t| t.starts_with("http")).map(str::to_string)
            .or_else(|| {
                for _ in 0..16 {
                    if let Some(u) = status_url() {
                        return Some(u);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                None
            })
    });
    match base {
        Some(base) => {
            let ep = OcEndpoint { base, password, discovered: true };
            *OC_ENDPOINT.lock().unwrap() = Some(ep.clone());
            Ok(ep)
        }
        None => bail!(
            "no opencode server — run `opencode service start` (or `opencode serve --port N` and set providers.opencode.base_url)"
        ),
    }
}

/// Drop the cached service endpoint (e.g. after a connect failure) so the
/// next call re-runs discovery.
fn oc_endpoint_evict() {
    *OC_ENDPOINT.lock().unwrap() = None;
}

/// A model entry from GET {base}/api/model: picker id ("providerID/modelID")
/// + context window (`limit.context`).
#[derive(Clone)]
struct OcModel {
    id: String,
    context: u64,
}

/// (base_url, models) — cached per endpoint so a service restart re-fetches.
static OC_MODELS: std::sync::Mutex<Option<(String, Vec<OcModel>)>> = std::sync::Mutex::new(None);

fn oc_parse_models(value: &Value) -> Vec<OcModel> {
    value["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| m["enabled"].as_bool().unwrap_or(true) && m["status"].as_str() != Some("deprecated"))
                .filter_map(|m| {
                    let p = m["providerID"].as_str()?;
                    let id = m["modelID"].as_str().or_else(|| m["id"].as_str())?;
                    Some(OcModel {
                        id: format!("{p}/{id}"),
                        context: m["limit"]["context"].as_u64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn oc_models(conf: &ProviderConf) -> (Vec<OcModel>, Option<String>) {
    let ep = match opencode_endpoint(conf) {
        Ok(e) => e,
        Err(e) => return (vec![], Some(e.to_string())),
    };
    if let Some((b, ms)) = OC_MODELS.lock().unwrap().clone() {
        if b == ep.base {
            return (ms, None);
        }
    }
    let url = format!("{}/api/model", ep.base);
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (vec![], Some(format!("http client: {e}"))),
    };
    let mut req = client.get(&url);
    if let Some(pw) = &ep.password {
        req = req.basic_auth("opencode", Some(pw));
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            if ep.discovered {
                oc_endpoint_evict();
            }
            return (vec![], Some(format!("fetch {url}: {e}")));
        }
    };
    let value: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => return (vec![], Some(format!("json parse: {e}"))),
    };
    let models = oc_parse_models(&value);
    *OC_MODELS.lock().unwrap() = Some((ep.base, models.clone()));
    (models, None)
}

/// Populate the model cache if empty. Blocking — call via spawn_blocking.
pub fn ensure_oc_models(conf: &ProviderConf) {
    let _ = oc_models(conf);
}

/// Model picker entries for the opencode provider: "providerID/modelID"
/// strings from GET {base}/api/model, or (empty, err) on failure.
pub fn opencode_models(conf: &ProviderConf) -> (Vec<String>, Option<String>) {
    let (models, err) = oc_models(conf);
    (models.into_iter().map(|m| m.id).collect(), err)
}

impl OpenCode {
    async fn run(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let ep = opencode_endpoint(&self.conf)?;
        let mut body = json!({ "prompt": oc_prompt(req) });
        if let Some(m) = oc_model_ref(&req.model) {
            body["model"] = m;
        }
        let client = http()?;
        let mut rb = client
            .post(format!("{}/api/experimental/generate", ep.base))
            .header("content-type", "application/json");
        if let Some(pw) = &ep.password {
            rb = rb.basic_auth("opencode", Some(pw));
        }
        for (k, v) in &self.conf.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = match rb.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                if ep.discovered {
                    oc_endpoint_evict();
                }
                return Err(e).context("opencode request failed (is `opencode serve`/`opencode service` running?)");
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or(text);
            bail!("opencode HTTP {status}: {msg}");
        }
        let v: Value = serde_json::from_str(&text).context("opencode response parse")?;
        let out = v["data"]["text"].as_str().unwrap_or_default();
        if out.is_empty() {
            bail!("opencode returned no text: {text}");
        }
        let _ = tx.send(StreamEvent::Text(out.into()));
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
        // live list comes from opencode_models(); static guesses would be wrong
        K::OpenCode => vec![],
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

/// A Command Code model entry: id + context window (the API reports a
/// `context_length` field per model).
#[derive(Clone)]
struct CcModel {
    id: String,
    context_length: u64,
}

/// Cached result of GET {base}/provider/v1/models.
static CC_MODELS: std::sync::Mutex<Option<Vec<CcModel>>> = std::sync::Mutex::new(None);

fn fallback_cc_models() -> Vec<CcModel> {
    FALLBACK_CC_MODELS
        .iter()
        .map(|s| CcModel { id: s.to_string(), context_length: 0 })
        .collect()
}

fn cc_models(conf: &crate::config::ProviderConf) -> (Vec<CcModel>, Option<String>) {
    if let Some(models) = CC_MODELS.lock().unwrap().clone() {
        return (models, None);
    }
    let url = format!("{}/provider/v1/models", conf.resolved_base().trim_end_matches('/'));
    let key = conf.resolved_key();
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (fallback_cc_models(), Some(format!("http client: {e}"))),
    };
    let mut req = client.get(&url);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => return (fallback_cc_models(), Some(format!("fetch {url}: {e}"))),
    };
    let value: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => return (fallback_cc_models(), Some(format!("json parse: {e}"))),
    };
    let models: Vec<CcModel> = value["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some(CcModel {
                        id: m["id"].as_str()?.to_string(),
                        context_length: m["context_length"].as_u64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        (fallback_cc_models(), Some("empty model list".into()))
    } else {
        *CC_MODELS.lock().unwrap() = Some(models.clone());
        (models, None)
    }
}

/// Populate the Command Code model cache if empty. Blocking HTTP — call via
/// `tokio::task::spawn_blocking` from async contexts.
pub fn ensure_cc_models(conf: &crate::config::ProviderConf) {
    let _ = cc_models(conf);
}

/// Returns (models, error) — error=Some(msg) means the API fetch failed.
/// Fetches the live model list from GET {base}/provider/v1/models (no `cmdc`
/// CLI needed). Cached on success so the GUI doesn't hit the API every frame.
pub fn cmdc_models(conf: &crate::config::ProviderConf) -> (Vec<String>, Option<String>) {
    let (models, err) = cc_models(conf);
    (models.into_iter().map(|m| m.id).collect(), err)
}

/// Effective context window in tokens for a provider+model, used by
/// auto-compaction. Config override (`provider.<name>.context_window`) wins;
/// Command Code uses the cached model list's `context_length` (their smallest
/// listed model is 200K, which is also the fallback); other providers get a
/// deliberately low default so compaction triggers before the API rejects.
pub fn context_window(conf: &ProviderConf, model: &str) -> u64 {
    if let Some(n) = conf.context_window {
        return n;
    }
    match conf.kind {
        crate::config::ProviderKind::CommandCode => CC_MODELS
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|ms| ms.iter().find(|m| m.id == model))
            .map(|m| m.context_length)
            .filter(|n| *n > 0)
            .unwrap_or(200_000),
        crate::config::ProviderKind::OpenCode => OC_MODELS
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|(_, ms)| ms.iter().find(|m| m.id == model))
            .map(|m| m.context)
            .filter(|n| *n > 0)
            .unwrap_or(200_000),
        crate::config::ProviderKind::Anthropic => 200_000,
        crate::config::ProviderKind::OpenAi => 128_000,
    }
}

/// One-shot text completion: stream a system+user exchange with no tools and
/// collect the text. Used by auto-compaction and post-run reflection — jobs
/// that need the model outside the agent loop. Polls `cancelled` the same
/// way the agent loop does so callers can abort a hung request.
pub async fn stream_text(
    prov: &dyn Provider,
    model: &str,
    system: &str,
    user_text: String,
    max_tokens: u32,
    cancelled: impl Fn() -> bool,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
    let req = ChatRequest {
        model: model.into(),
        max_tokens,
        system: Some(system.into()),
        system_tail: None,
        messages: vec![ChatMessage::user(user_text)],
        tools: vec![],
    };
    let fwd = tokio::spawn(async move {
        let mut out = String::new();
        let mut err = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Text(t) => out.push_str(&t),
                StreamEvent::Err(e) => {
                    err = Some(e);
                    break;
                }
                StreamEvent::Done => break,
                _ => {}
            }
        }
        (out, err)
    });
    {
        let mut fut = std::pin::pin!(prov.stream(&req, tx));
        loop {
            tokio::select! {
                _ = &mut fut => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if cancelled() {
                        break;
                    }
                }
            }
        }
    }
    let (out, err) = fwd.await.map_err(|e| e.to_string())?;
    match err {
        Some(e) => Err(e),
        None if out.trim().is_empty() => Err("empty response".into()),
        None => Ok(out),
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

    /// The Command Code usage shape: `inputTokens` is the sum of the cached and
    /// uncached halves, so `input` must not be added to `cache_read` again.
    #[test]
    fn cc_usage_splits_cached_from_uncached_input() {
        let v = json!({
            "inputTokens": 4443,
            "inputTokenDetails": {"noCacheTokens": 91, "cacheReadTokens": 4352},
            "outputTokens": 15,
            "outputTokenDetails": {"textTokens": 2, "reasoningTokens": 13},
        });
        let u = cc_usage(&v).expect("usage parsed");
        assert_eq!(u.input, 4443, "inputTokens is the total, not the uncached part");
        assert_eq!(u.cache_read, 4352);
        assert_eq!(u.uncached_input(), 91);
        assert_eq!(u.output, 15);
        assert_eq!(u.reasoning, 13);
    }

    #[test]
    fn cc_usage_falls_back_to_openai_and_anthropic_names() {
        // OpenAI-ish flat shape
        let u = cc_usage(&json!({"prompt_tokens": 100, "completion_tokens": 7})).unwrap();
        assert_eq!((u.input, u.output), (100, 7));
        // Anthropic-ish cache naming
        let u = cc_usage(&json!({
            "inputTokens": 50, "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 20,
        }))
        .unwrap();
        assert_eq!(u.cache_read, 900);
        assert_eq!(u.cache_write, 20);
    }

    #[test]
    fn cc_usage_none_when_absent_or_zero() {
        assert!(cc_usage(&json!({})).is_none());
        assert!(cc_usage(&json!(null)).is_none());
        assert!(cc_usage(&json!({"inputTokens": 0, "outputTokens": 0})).is_none());
    }

    #[test]
    fn openai_usage_reads_cached_and_reasoning_tokens() {
        let u = openai_usage(&json!({
            "prompt_tokens": 1200,
            "completion_tokens": 80,
            "prompt_tokens_details": {"cached_tokens": 1024},
            "completion_tokens_details": {"reasoning_tokens": 32},
        }))
        .unwrap();
        assert_eq!(u.input, 1200);
        assert_eq!(u.cache_read, 1024);
        assert_eq!(u.output, 80);
        assert_eq!(u.reasoning, 32);
        assert!(openai_usage(&json!({})).is_none());
    }

    /// Anthropic splits usage across two events; the phases must not overlap or
    /// input/output would be counted twice.
    #[test]
    fn anthropic_usage_splits_input_and_output_phases() {
        let start = json!({
            "input_tokens": 200, "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 100, "output_tokens": 1,
        });
        let u = anthropic_usage(&start, 0).unwrap();
        assert_eq!(u.input, 1100, "input + cache read + cache write");
        assert_eq!(u.output, 0, "output is not final on message_start");
        assert_eq!(u.cache_read, 800);
        assert_eq!(u.cache_write, 100);

        let delta = json!({"output_tokens": 250});
        let u = anthropic_usage(&delta, 1).unwrap();
        assert_eq!((u.input, u.output), (0, 250));
    }

    #[test]
    fn usage_accumulates_and_reports_uncached() {
        let mut a = crate::types::Usage {
            input: 1000, output: 10, cache_read: 900, cache_write: 5, reasoning: 3,
        };
        let b = crate::types::Usage { input: 500, output: 20, cache_read: 400, ..Default::default() };
        a.add(&b);
        assert_eq!(a.input, 1500);
        assert_eq!(a.output, 30);
        assert_eq!(a.cache_read, 1300);
        assert_eq!(a.uncached_input(), 200);
        assert!(!a.is_zero());
        assert!(crate::types::Usage::default().is_zero());
    }

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

    /// Command Code's `/alpha/generate` schema is Anthropic-shaped: an image
    /// block needs `source.media_type` (snake_case). Sending `mediaType` (the
    /// AI-SDK/OpenAI spelling) gets the whole request rejected with a 400
    /// "Invalid option: expected one of image/jpeg|png|gif|webp", and a
    /// `system: null` is rejected too.
    #[test]
    fn cc_wire_shape_is_anthropic_style() {
        let msgs = vec![ChatMessage {
            role: Role::User,
            blocks: vec![
                Block::Image { media_type: "image/png".into(), data: "AAAA".into(), width: 2, height: 2 },
                Block::Text { text: "what is this?".into() },
            ],
        }];
        let cc = cc_messages(&msgs);
        assert_eq!(cc[0]["role"], "user");
        assert_eq!(cc[0]["content"][0]["type"], "image");
        assert_eq!(cc[0]["content"][0]["source"]["type"], "base64");
        assert_eq!(cc[0]["content"][0]["source"]["media_type"], "image/png");
        assert_eq!(cc[0]["content"][0]["source"]["data"], "AAAA");
        assert!(cc[0]["content"][0]["source"].get("mediaType").is_none());
        assert_eq!(cc[0]["content"][1]["text"], "what is this?");
    }

    /// Switching model/provider mid-run makes the agent re-convert the whole
    /// session for the *new* provider on the next turn. The two wire formats
    /// differ (Anthropic `tool_use`/`tool_result` blocks vs OpenAI
    /// `tool_calls`/`role:"tool"`), so a transcript produced by one provider
    /// must convert cleanly for the other. If this breaks, agent.rs's
    /// per-turn provider switch would send an invalid body.
    #[test]
    fn transcript_converts_across_providers() {
        let msgs = req_msgs();

        // Anthropic-produced log -> OpenAI wire format
        let oai = openai_messages(&ChatRequest {
            model: "m".into(), max_tokens: 1, system: Some("sys".into()),
            system_tail: None, messages: msgs.clone(), tools: vec![],
        });
        assert_eq!(oai[2]["tool_calls"][0]["id"], "t1");
        assert_eq!(oai[3]["role"], "tool");
        assert_eq!(oai[3]["tool_call_id"], "t1");

        // OpenAI-produced log -> Anthropic wire format
        let ant = anthropic_messages(&msgs);
        assert_eq!(ant[1]["content"][1]["type"], "tool_use");
        assert_eq!(ant[2]["content"][0]["type"], "tool_result");

        // and every tool_use is answered in both directions, so neither
        // provider rejects the body with "missing tool_result"
        for (name, has_res) in [
            ("anthropic", ant[2]["content"][0]["tool_use_id"] == "t1"),
            ("openai", oai[3]["tool_call_id"] == "t1"),
        ] {
            assert!(has_res, "{name}: tool result not paired");
        }
    }

    /// /api/experimental/generate takes one prompt string; the transcript is
    /// flattened into a `role:`-headed chat that ends on an `assistant:` cue.
    #[test]
    fn oc_prompt_flattens_transcript() {
        let r = ChatRequest {
            model: "m".into(), max_tokens: 1,
            system: Some("be brief".into()),
            system_tail: Some("skill idx".into()),
            messages: req_msgs(), tools: vec![],
        };
        let p = oc_prompt(&r);
        assert!(p.starts_with("be briefskill idx\n\n")); // system_tail is appended raw
        assert!(p.contains("user:\nhi\n"));
        assert!(p.contains("assistant:\nreading\n[tool call: fs_read {\"path\":\"/a\"}]"));
        assert!(p.contains("user:\n[tool result t1]: data"));
        assert!(p.ends_with("assistant:\n"));
    }

    #[test]
    fn oc_model_ref_splits_once() {
        // model ids may contain '/' — only the first separates providerID
        let v = oc_model_ref("commandcode/deepseek/deepseek-v4-flash").unwrap();
        assert_eq!(v["providerID"], "commandcode");
        assert_eq!(v["id"], "deepseek/deepseek-v4-flash");
        assert!(oc_model_ref("").is_none());
        assert!(oc_model_ref("noslash").is_none());
        assert!(oc_model_ref("/id").is_none());
        assert!(oc_model_ref("p/").is_none());
    }

    #[test]
    fn oc_parse_models_filters_and_joins() {
        let v = json!({"data": [
            {"providerID": "commandcode", "modelID": "deepseek/deepseek-v4-flash",
             "enabled": true, "status": "active", "limit": {"context": 200000, "output": 32000}},
            {"providerID": "opencode", "modelID": "free-x",
             "enabled": false, "status": "active", "limit": {"context": 128000, "output": 1}},
            {"providerID": "anthropic", "modelID": "old",
             "enabled": true, "status": "deprecated", "limit": {"context": 1, "output": 1}},
            {"providerID": "broken"},
        ]});
        let ms = oc_parse_models(&v);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].id, "commandcode/deepseek/deepseek-v4-flash");
        assert_eq!(ms[0].context, 200000);
        assert!(oc_parse_models(&json!({})).is_empty());
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
            system_tail: None,
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
    fn anthropic_body_cache_breakpoints() {
        let r = ChatRequest {
            model: "m".into(), max_tokens: 1,
            system: Some("static".into()),
            system_tail: Some("skills index".into()),
            messages: req_msgs(),
            tools: vec![
                ToolSpec { name: "a".into(), description: "d".into(), schema: json!({}) },
                ToolSpec { name: "b".into(), description: "d".into(), schema: json!({}) },
            ],
        };
        let b = anthropic_body(&r);
        // last tool carries the tools-prefix breakpoint
        assert!(b["tools"][0]["cache_control"].is_null());
        assert_eq!(b["tools"][1]["cache_control"]["type"], "ephemeral");
        // static system block cached; volatile tail block is not
        assert_eq!(b["system"][0]["text"], "static");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["system"][1]["text"], "skills index");
        assert!(b["system"][1]["cache_control"].is_null());
        // newest message's last block holds the history breakpoint
        let last = b["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"].as_array().unwrap().last().unwrap()["cache_control"]["type"], "ephemeral");
    }

    #[tokio::test]
    async fn retry_stream_replays_transient_and_dedups_text() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        retry_stream(
            move |rtx| {
                let n = c.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        let _ = rtx.send(StreamEvent::Text("he".into()));
                        bail!("connection reset")
                    } else {
                        let _ = rtx.send(StreamEvent::Text("hello".into()));
                        let _ = rtx.send(StreamEvent::Done);
                        Ok(())
                    }
                }
            },
            &tx,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let mut text = String::new();
        let mut done = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::Text(t) => text.push_str(&t),
                StreamEvent::Done => done = true,
                _ => {}
            }
        }
        assert_eq!(text, "hello"); // "he" + suffix "llo" — no duplication
        assert!(done);
    }

    #[tokio::test]
    async fn retry_stream_gives_up_on_permanent_error() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        retry_stream(
            move |rtx| {
                c.fetch_add(1, Ordering::SeqCst);
                async move {
                    let _ = rtx;
                    bail!("anthropic HTTP 400: bad request")
                }
            },
            &tx,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        match rx.try_recv() {
            Ok(StreamEvent::Err(e)) => assert!(e.contains("400")),
            other => panic!("expected Err, got {:?}", other.map(|_| "event")),
        }
    }

    #[test]
    #[ignore] // requires a running `opencode` service
    fn opencode_models_real() {
        let conf = crate::config::ProviderConf {
            kind: crate::config::ProviderKind::OpenCode,
            ..Default::default()
        };
        let (models, err) = opencode_models(&conf);
        eprintln!("parsed {} models (err={:?})", models.len(), err);
        eprintln!("{:?}", &models[..models.len().min(10)]);
        assert!(err.is_none() && !models.is_empty());
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
            system_tail: None,
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

    /// End-to-end chat turn through a running opencode server (background
    /// service discovered via `opencode service status`, or set base_url).
    ///
    ///   cargo test opencode_live_generate -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn opencode_live_generate() {
        let conf = ProviderConf {
            kind: ProviderKind::OpenCode,
            model: "commandcode/deepseek/deepseek-v4-flash".into(),
            ..Default::default()
        };
        let p = build(&conf);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = ChatRequest {
            model: conf.model.clone(),
            max_tokens: 100,
            system: Some("Reply concisely.".into()),
            system_tail: None,
            messages: vec![ChatMessage::user("Say the word PONG and nothing else.")],
            tools: vec![],
        };
        p.stream(&req, tx).await;
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            println!("EV: {ev:?}");
            match ev {
                StreamEvent::Text(t) => text.push_str(&t),
                StreamEvent::Done => break,
                StreamEvent::Err(e) => panic!("{e}"),
                _ => {}
            }
        }
        assert!(text.to_uppercase().contains("PONG"), "got: {text}");
    }

    /// End-to-end check against the live Command Code API: the stream must
    /// report real usage, and the cached-token split must be non-zero once the
    /// same prefix is sent twice. Requires a signed-in `cmdc` / OpenCode
    /// credential; ignored by default because it spends credits.
    ///
    ///   cargo test cc_live_reports_usage -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn cc_live_reports_usage() {
        let conf = ProviderConf {
            kind: ProviderKind::CommandCode,
            model: "deepseek/deepseek-v4.1-flash".into(),
            // WSL has no ~/.commandcode/auth.json; reuse the OpenCode credential
            // (same Command Code account) so the test can run outside Windows.
            api_key: std::fs::read_to_string(
                dirs::home_dir().unwrap_or_default().join(".local/share/opencode/auth.json"),
            )
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["commandcode"]["key"].as_str().map(str::to_string))
            .or_else(|| std::env::var("CMD_API_KEY").ok()),
            ..Default::default()
        };
        let p = build(&conf);
        let pad = (0..200)
            .map(|i| format!("Rule {i}: a long stable prefix line used to exercise prompt caching."))
            .collect::<Vec<_>>()
            .join("\n");
        let mk_req = |tail: &str| ChatRequest {
            model: "deepseek/deepseek-v4.1-flash".into(),
            max_tokens: 16,
            system: Some(format!("You are a helpful assistant.\n{pad}")),
            system_tail: None,
            messages: vec![ChatMessage::user(tail)],
            tools: vec![],
        };

        let mut last = crate::types::Usage::default();
        for pass in 1..=2 {
            let (tx, mut rx) = mpsc::unbounded_channel();
            p.stream(&mk_req("Reply with the single word: OK"), tx).await;
            let mut u = crate::types::Usage::default();
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Usage(x) => u.add(&x),
                    StreamEvent::Err(e) => {
                        println!("pass {pass}: STREAM ERROR: {e}");
                        break;
                    }
                    StreamEvent::Done => break,
                    _ => {}
                }
            }
            println!("pass {pass}: {u:?}  (uncached input {})", u.uncached_input());
            assert!(!u.is_zero(), "pass {pass}: no usage reported — the stream dropped it");
            assert!(u.input > 0 && u.output > 0, "pass {pass}: implausible usage {u:?}");
            last = u;
        }
        // The second pass re-sends the identical prefix, so the provider should
        // serve most of it from cache. This is the number amty used to discard.
        assert!(
            last.cache_read > 0,
            "second pass reported no cache reads ({last:?}) — prefix caching regressed"
        );
    }
}
