use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// An image attached to a user message (clipboard paste / file drop).
    /// Bytes are stored as base64 PNG so the session log stays text-only.
    Image {
        media_type: String,
        data: String,
        #[serde(default)]
        width: u32,
        #[serde(default)]
        height: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, blocks: vec![Block::Text { text: text.into() }] }
    }
    /// A user message built from explicit blocks (text and/or images).
    pub fn user_blocks(blocks: Vec<Block>) -> Self {
        Self { role: Role::User, blocks }
    }
    pub fn tool_results(results: Vec<Block>) -> Self {
        Self { role: Role::User, blocks: results }
    }
    pub fn text_content(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                Block::Image { .. } => Some("[image]"),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub max_tokens: u32,
    /// Static system prompt. Kept byte-stable across turns so provider prompt
    /// caches (Anthropic `cache_control`, OpenAI automatic prefix caching) hit.
    pub system: Option<String>,
    /// Volatile system suffix (e.g. the skills index, re-scanned every turn).
    /// Providers append it after the cacheable static prefix so churn here
    /// doesn't invalidate the cached system/tools prefix.
    pub system_tail: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
}

/// Guarantee every `tool_use` has a `tool_result`: an interrupted run can
/// leave a dangling call in the session log, which providers reject
/// ("Tool result is missing for tool call …"). Missing results are backfilled
/// with a synthetic error result in the next user message, or a synthetic
/// trailing user message if the log ends with un-answered tool calls.
pub fn ensure_tool_results(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut pending: Vec<String> = vec![]; // tool_use ids awaiting a result
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages {
        match m.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    out.push(ChatMessage::tool_results(
                        pending.drain(..).map(mk_missing_result).collect(),
                    ));
                }
                for b in &m.blocks {
                    if let Block::ToolUse { id, .. } = b {
                        pending.push(id.clone());
                    }
                }
                out.push(m.clone());
            }
            Role::User => {
                for b in &m.blocks {
                    if let Block::ToolResult { tool_use_id, .. } = b {
                        used.insert(tool_use_id.clone());
                    }
                }
                pending.retain(|id| !used.contains(id));
                let mut blocks = m.blocks.clone();
                if !pending.is_empty() {
                    let mut pre: Vec<Block> = pending.drain(..).map(mk_missing_result).collect();
                    pre.extend(blocks);
                    blocks = pre;
                }
                out.push(ChatMessage { role: Role::User, blocks });
            }
        }
    }
    if !pending.is_empty() {
        out.push(ChatMessage::tool_results(
            pending.drain(..).map(mk_missing_result).collect(),
        ));
    }
    out
}

/// Byte-length-limited preview that never splits a UTF-8 character.
pub fn truncate_preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn mk_missing_result(id: String) -> Block {
    Block::ToolResult {
        tool_use_id: id,
        content: "(interrupted — this tool call was cancelled before it ran)".into(),
        is_error: true,
    }
}

/// Token accounting for one provider call, as reported by the provider.
/// Cache fields are what make Command Code (and Anthropic) usage legible: a
/// long agent turn re-reads its whole prefix every step, so `cache_read`
/// usually dwarfs `input` and is the only number that explains the bill.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub reasoning: u64,
}

impl Usage {
    /// Input tokens actually paid at the input rate (uncached only).
    pub fn uncached_input(&self) -> u64 {
        self.input.saturating_sub(self.cache_read)
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }

    /// Sum another call's usage into this running total.
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.reasoning += o.reasoning;
    }
}

/// Events flowing out of a streaming provider call.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Text(String),
    ToolUse { id: String, name: String, input: serde_json::Value },
    /// Per-call token usage, emitted by providers that report it.
    Usage(Usage),
    Done,
    Err(String),
}

/// App-level events broadcast to the GUI, SSE clients and the MCP facade.
#[derive(Clone, Debug, Serialize)]
pub struct AppEvent {
    pub session: String,
    #[serde(flatten)]
    pub kind: EvKind,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum EvKind {
    Text { text: String },
    ToolStart { name: String, detail: String },
    ToolEnd { name: String, ok: bool, preview: String },
    ApprovalNeeded { approval_id: String, tool: String, detail: String },
    ApprovalResolved { approval_id: String, allowed: bool },
    Running { running: bool },
    /// Token usage for the whole turn so far (emitted per provider call).
    Usage { usage: Usage, turns: u32, cumulative: Usage },
    Done,
    Error { message: String },
    /// Auto-compaction replaced `dropped` messages with a summary.
    Compacted { dropped: usize },
    /// The self-evolution pass updated memory/skills/config.
    Evolved { summary: String },
    Touched,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dangling_tool_use_gets_backfilled() {
        // session interrupted after a tool call: no result message exists
        let msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![
                    Block::Text { text: "x".into() },
                    Block::ToolUse { id: "t1".into(), name: "shell".into(), input: json!({}) },
                ],
            },
            ChatMessage::user("again"),
        ];
        let out = ensure_tool_results(&msgs);
        // synthetic tool_result must precede the "again" user text inside it
        assert_eq!(out.len(), 3);
        match &out[2].blocks[0] {
            Block::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, "t1");
                assert!(*is_error);
            }
            _ => panic!("expected tool_result first"),
        }
    }

    #[test]
    fn trailing_tool_use_gets_trailing_result() {
        let msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse { id: "t9".into(), name: "s".into(), input: json!({}) }],
            },
        ];
        let out = ensure_tool_results(&msgs);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[2].blocks[0], Block::ToolResult { .. }));
    }

    #[test]
    fn paired_messages_pass_through() {
        let msgs = vec![
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse { id: "t1".into(), name: "s".into(), input: json!({}) }],
            },
            ChatMessage::tool_results(vec![Block::ToolResult {
                tool_use_id: "t1".into(), content: "ok".into(), is_error: false,
            }]),
        ];
        let out = ensure_tool_results(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].blocks.len(), 1); // no synthetic block added
    }
}
