//! Auto-compaction: when the estimated prompt approaches the model's context
//! window, the older span of the session is summarized by the provider and
//! replaced in-place, keeping a recent tail of messages verbatim.
//!
//! Trigger: estimated input tokens > (context_window - max_tokens) * 0.9.
//! The tail keeps at most ~25% of the input budget so the summary + tail fit
//! comfortably, and it always opens on a plain user-text message — a tail
//! starting on tool results would orphan them (providers reject results
//! whose tool_use was summarized away).

use crate::app::App;
use crate::config::{ProviderConf, ProviderKind};
use crate::provider::Provider;
use crate::types::*;
use std::sync::Arc;
use tokio::sync::mpsc;

const SUMMARY_SYSTEM: &str = "You are compacting a chat session between a user \
and an AI coding assistant. Summarize the transcript below into a compact brief \
that preserves: the user's goals and explicit requests, decisions made, \
files/paths created or modified, key findings from tool output, errors \
encountered, and work still pending. Omit filler and greetings. Write in the \
same language the transcript mostly uses.";

/// Rough token estimate without a tokenizer dep: ASCII ≈ 4 chars/token,
/// non-ASCII ≈ 1 char/token. Biased high so we compact before the API
/// rejects the request rather than after.
fn est_tokens(s: &str) -> u64 {
    let quarters: u64 = s.chars().map(|c| if c.is_ascii() { 1 } else { 4 }).sum();
    quarters.div_ceil(4)
}

fn msg_tokens(m: &ChatMessage) -> u64 {
    let mut t = 16;
    for b in &m.blocks {
        t += match b {
            Block::Text { text } => est_tokens(text),
            Block::ToolUse { name, input, .. } => {
                est_tokens(name) + est_tokens(&input.to_string())
            }
            Block::ToolResult { content, .. } => est_tokens(content),
        };
    }
    t
}

/// Estimated input tokens for one request: system prompt + tool specs +
/// message history.
pub fn estimate(messages: &[ChatMessage], system: &str, tools: &[ToolSpec]) -> u64 {
    let mut t = est_tokens(system) + 512;
    for s in tools {
        t += est_tokens(&s.name) + est_tokens(&s.description) + est_tokens(&s.schema.to_string());
    }
    t + messages.iter().map(msg_tokens).sum::<u64>()
}

/// A user message of plain text (no tool blocks) — a valid tail start.
fn is_plain_user_text(m: &ChatMessage) -> bool {
    m.role == Role::User
        && m.blocks.iter().any(|b| matches!(b, Block::Text { .. }))
        && !m.blocks.iter().any(|b| matches!(b, Block::ToolResult { .. } | Block::ToolUse { .. }))
}

/// Index where the verbatim tail begins: the largest suffix within `keep`
/// tokens, moved to a plain user-text boundary. Forward first (shrinks the
/// tail), else backward (grows it); returns messages.len() when no boundary
/// exists at all.
fn find_split(messages: &[ChatMessage], keep: u64) -> usize {
    let mut split = messages.len().saturating_sub(1);
    let mut acc = 0u64;
    while split > 0 {
        let cost = msg_tokens(&messages[split]);
        if acc + cost > keep {
            break;
        }
        acc += cost;
        split -= 1;
    }
    (split..messages.len())
        .find(|&i| is_plain_user_text(&messages[i]))
        .or_else(|| (0..split).rev().find(|&i| is_plain_user_text(&messages[i])))
        .unwrap_or(messages.len())
}

/// Flatten messages into a text transcript for the summarizer. Long tool
/// inputs/results are clipped — the summary needs their gist, not bytes.
fn transcript(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        for b in &m.blocks {
            match b {
                Block::Text { text } => {
                    out.push_str(&format!("{role}: {text}\n"));
                }
                Block::ToolUse { name, input, .. } => {
                    out.push_str(&format!(
                        "{role} [tool use {name}]: {}\n",
                        truncate_preview(&input.to_string(), 500)
                    ));
                }
                Block::ToolResult { content, is_error, .. } => {
                    out.push_str(&format!(
                        "{role} [tool result{}]: {}\n",
                        if *is_error { " (error)" } else { "" },
                        truncate_preview(content, 1000)
                    ));
                }
            }
        }
    }
    out
}

/// Ask the provider to summarize `head`. Streams like a normal run but with
/// no tools and a capped output; returns Err on stream errors / empty output
/// so the caller can fall back to a lossless-drop marker.
async fn summarize(
    prov: &dyn Provider,
    model: &str,
    head: &[ChatMessage],
    cancelled: impl Fn() -> bool,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
    let req = ChatRequest {
        model: model.into(),
        max_tokens: 2048,
        system: Some(SUMMARY_SYSTEM.into()),
        system_tail: None,
        messages: vec![ChatMessage::user(transcript(head))],
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
        None if out.trim().is_empty() => Err("empty summary".into()),
        None => Ok(out),
    }
}

/// Compact the session if its estimated size is near the context window.
/// Runs before each provider request inside the agent loop; a no-op when the
/// estimate fits. Rewrites the stored session so subsequent turns inherit the
/// compaction. Never fails the run — falls back to a plain "omitted" marker.
pub async fn maybe_compact(
    app: &Arc<App>,
    sid: &str,
    prov: &dyn Provider,
    conf: &ProviderConf,
    model: &str,
    system: &str,
    tools: &[ToolSpec],
    max_tokens: u32,
    cancelled: impl Fn() -> bool,
) {
    let Some(session) = app.sessions.get(sid) else { return };
    // The Command Code context length lives in the model list — warm the
    // cache once (blocking HTTP) so context_window() can read it cheaply.
    if conf.context_window.is_none() && conf.kind == ProviderKind::CommandCode {
        let c = conf.clone();
        let _ = tokio::task::spawn_blocking(move || crate::provider::ensure_cc_models(&c)).await;
    }
    let input_cap = crate::provider::context_window(conf, model).saturating_sub(max_tokens as u64);
    if estimate(&session.messages, system, tools) <= input_cap * 9 / 10 {
        return;
    }

    let split = find_split(&session.messages, input_cap / 4);
    if split == 0 || split >= session.messages.len() {
        return; // nothing safe to drop — let the provider report the overflow
    }

    let dropped = split;
    let head: Vec<ChatMessage> = session.messages[..split].to_vec();
    let summary = summarize(prov, model, &head, &cancelled).await.unwrap_or_default();
    let note = if summary.trim().is_empty() {
        format!("[amty: earlier conversation compacted — {dropped} messages omitted]")
    } else {
        format!("[amty: conversation so far, compacted]\n{}", summary.trim())
    };
    let mut msgs = Vec::with_capacity(session.messages.len() - split + 1);
    msgs.push(ChatMessage::user(note));
    msgs.extend_from_slice(&session.messages[split..]);
    if app.sessions.replace_messages(sid, msgs) {
        app.emit(sid, EvKind::Compacted { dropped });
        app.emit(sid, EvKind::Touched);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> ChatMessage {
        ChatMessage::user(t)
    }

    fn assistant_tool(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse { id: id.into(), name: "shell".into(), input: serde_json::json!({}) }],
        }
    }

    fn result(id: &str) -> ChatMessage {
        ChatMessage::tool_results(vec![Block::ToolResult {
            tool_use_id: id.into(), content: "ok".into(), is_error: false,
        }])
    }

    #[test]
    fn estimate_scales_with_content() {
        let small = estimate(&[user("hi")], "sys", &[]);
        let big = estimate(&[user(&"x".repeat(40_000))], "sys", &[]);
        assert!(big > small + 5_000);
    }

    #[test]
    fn split_lands_on_user_text_not_tool_result() {
        let msgs = vec![
            user("first"),
            assistant_tool("t1"),
            result("t1"),
            user("second"),
            assistant_tool("t2"),
            result("t2"),
        ];
        // tiny keep budget: tail can hold ~nothing, but must still open on a
        // plain user message rather than the trailing tool_result
        let split = find_split(&msgs, 1);
        assert_eq!(split, 3); // "second"
        assert!(is_plain_user_text(&msgs[split]));
    }

    #[test]
    fn split_backward_fallback_when_no_user_text_after() {
        // session ends mid tool-exchange: last user text is before the split
        let msgs = vec![
            user("big"),
            assistant_tool("t1"),
            result("t1"),
        ];
        let split = find_split(&msgs, 1);
        assert_eq!(split, 0); // falls back to the only user-text message
    }

    #[test]
    fn transcript_includes_tool_blocks() {
        let msgs = vec![
            assistant_tool("t1"),
            result("t1"),
        ];
        let t = transcript(&msgs);
        assert!(t.contains("tool use shell"));
        assert!(t.contains("tool result"));
    }
}
