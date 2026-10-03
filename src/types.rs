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
    pub fn tool_results(results: Vec<Block>) -> Self {
        Self { role: Role::User, blocks: results }
    }
    pub fn text_content(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
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
    pub system: Option<String>,
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

fn mk_missing_result(id: String) -> Block {
    Block::ToolResult {
        tool_use_id: id,
        content: "(interrupted — this tool call was cancelled before it ran)".into(),
        is_error: true,
    }
}

/// Events flowing out of a streaming provider call.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Text(String),
    ToolUse { id: String, name: String, input: serde_json::Value },
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
    Done,
    Error { message: String },
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
