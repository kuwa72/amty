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
