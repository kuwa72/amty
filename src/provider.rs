use crate::config::ProviderConf;
use crate::types::*;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>);
}

pub fn build(conf: &ProviderConf) -> Box<dyn Provider> {
    match conf.kind {
        crate::config::ProviderKind::Anthropic => Box::new(Anthropic { conf: conf.clone() }),
        crate::config::ProviderKind::OpenAi => Box::new(OpenAi { conf: conf.clone() }),
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
    messages
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
        let client = reqwest::Client::new();
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
        let resp = rb.json(&body).send().await?;
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
    for m in &req.messages {
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
        let client = reqwest::Client::new();
        let mut rb = client
            .post(format!("{}/chat/completions", self.conf.resolved_base()))
            .header("content-type", "application/json");
        if let Some(key) = self.conf.resolved_key() {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &self.conf.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.json(&body).send().await?;
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
