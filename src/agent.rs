use crate::app::App;
use crate::config::ApprovalMode;
use crate::provider;
use crate::tools;
use crate::types::*;
use anyhow::{anyhow, Result};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc;

const MAX_TURNS: usize = 40;
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Start an agent run in the background. Returns error if already running
/// or the session/provider is invalid.
pub fn start_run(app: &Arc<App>, session_id: &str, text: String) -> Result<()> {
    let sid = app
        .sessions
        .resolve(session_id)
        .ok_or_else(|| anyhow!("no such session '{session_id}'"))?;
    if app.begin_run(&sid).is_none() {
        anyhow::bail!("session '{sid}' already has a running turn");
    }
    let app = app.clone();
    tokio::spawn(async move {
        run(&app, &sid, text).await;
        app.end_run(&sid);
    });
    Ok(())
}

async fn run(app: &Arc<App>, sid: &str, text: String) {
    if let Err(e) = run_inner(app, sid, text).await {
        app.emit(sid, EvKind::Error { message: e.to_string() });
    }
}

async fn run_inner(app: &Arc<App>, sid: &str, text: String) -> Result<()> {
    let cancel = app.runs.lock().unwrap().get(sid).cloned().ok_or_else(|| anyhow!("run not registered"))?;
    let cancelled = || cancel.load(Ordering::SeqCst);

    app.sessions.push_message(sid, ChatMessage::user(text));
    app.emit(sid, EvKind::Touched);

    // ensure MCP servers are up (cheap after first connect)
    app.mcp.ensure_connected().await;

    for _turn in 0..MAX_TURNS {
        if cancelled() {
            app.emit(sid, EvKind::Error { message: "cancelled".into() });
            return Ok(());
        }

        let session = app.sessions.get(sid).ok_or_else(|| anyhow!("session gone"))?;
        let (prov_name, prov_conf, model, system) = {
            let cfg = app.cfg.read().unwrap();
            let name = session.provider.clone().unwrap_or_else(|| cfg.provider.clone());
            let conf = cfg.providers.get(&name).cloned().ok_or_else(|| anyhow!("provider '{name}' not defined"))?;
            let model = session.model.clone().unwrap_or_else(|| conf.model.clone());
            (name, conf, model, cfg.system_prompt.clone())
        };
        if model.is_empty() {
            anyhow::bail!("no model configured for provider '{prov_name}'");
        }

        let mut specs = tools::builtin_specs();
        specs.extend(app.mcp.specs());
        let req = ChatRequest {
            model,
            max_tokens: prov_conf.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            system: Some(system),
            messages: session.messages,
            tools: specs,
        };

        let prov = provider::build(&prov_conf);
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let app2 = app.clone();
        let sid2 = sid.to_string();
        let fwd = tokio::spawn(async move {
            let mut blocks: Vec<Block> = vec![];
            let mut cur_text = String::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Text(t) => {
                        cur_text.push_str(&t);
                        app2.emit(&sid2, EvKind::Text { text: t });
                    }
                    StreamEvent::ToolUse { id, name, input } => {
                        if !cur_text.is_empty() {
                            blocks.push(Block::Text { text: std::mem::take(&mut cur_text) });
                        }
                        blocks.push(Block::ToolUse { id, name, input });
                    }
                    StreamEvent::Done => break,
                    StreamEvent::Err(e) => {
                        app2.emit(&sid2, EvKind::Error { message: e });
                        break;
                    }
                }
            }
            if !cur_text.is_empty() {
                blocks.push(Block::Text { text: cur_text });
            }
            blocks
        });
        // stream is cancel-safe: dropping the future aborts the HTTP request
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
        let blocks = fwd.await.unwrap_or_default();

        let tool_uses: Vec<Block> = blocks
            .iter()
            .filter(|b| matches!(b, Block::ToolUse { .. }))
            .cloned()
            .collect();

        if !blocks.is_empty() {
            app.sessions.push_message(sid, ChatMessage { role: Role::Assistant, blocks });
            app.emit(sid, EvKind::Touched);
        }
        if tool_uses.is_empty() {
            app.emit(sid, EvKind::Done);
            return Ok(());
        }

        // execute tools, collect results — on cancel, still write results
        // for every tool_use so the session doesn't end up with a dangling
        // call the provider will reject next turn
        let mut results: Vec<Block> = vec![];
        // (name, input) -> (content, is_error) — identical calls in one turn
        // run once and share the result; models sometimes emit exact dupes
        let mut seen: std::collections::HashMap<String, (String, bool)> = std::collections::HashMap::new();
        let mut interrupted = false;
        for b in &tool_uses {
            if cancelled() {
                interrupted = true;
                break;
            }
            let Block::ToolUse { id, name, input } = b.clone() else { continue };
            let dup_key = format!("{name}\u{1}{input}");
            if let Some((content, is_error)) = seen.get(&dup_key) {
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content: format!("(deduplicated: identical call already ran)\n{content}"),
                    is_error: *is_error,
                });
                continue;
            }
            let detail = tools::summarize(&name, &input);
            app.emit(sid, EvKind::ToolStart { name: name.clone(), detail: detail.clone() });

            if let Err(e) = approval_gate(app, sid, &name, &input, &detail, cancelled).await {
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content: format!("denied: {e}"),
                    is_error: true,
                });
                app.emit(sid, EvKind::ToolEnd { name, ok: false, preview: "denied".into() });
                continue;
            }

            let out = if name.starts_with("mcp__") {
                app.mcp.call_tool(&name, input).await
            } else {
                tools::execute(app, &name, &input).await
            };
            match out {
                Ok(text) => {
                    app.emit(sid, EvKind::ToolEnd {
                        name: name.clone(),
                        ok: true,
                        preview: preview(&text),
                    });
                    seen.insert(dup_key, (text.clone(), false));
                    results.push(Block::ToolResult { tool_use_id: id, content: text, is_error: false });
                }
                Err(e) => {
                    app.emit(sid, EvKind::ToolEnd { name: name.clone(), ok: false, preview: preview(&e) });
                    seen.insert(dup_key, (e.clone(), true));
                    results.push(Block::ToolResult { tool_use_id: id, content: e, is_error: true });
                }
            }
        }
        // backfill results for tool calls never executed (interrupted mid-batch)
        for b in &tool_uses {
            if let Block::ToolUse { id, .. } = b {
                let answered = results.iter().any(|r| {
                    matches!(r, Block::ToolResult { tool_use_id, .. } if tool_use_id == id)
                });
                if !answered {
                    results.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: "(interrupted — tool call not executed)".into(),
                        is_error: true,
                    });
                }
            }
        }
        app.sessions.push_message(sid, ChatMessage::tool_results(results));
        app.emit(sid, EvKind::Touched);
        if interrupted {
            app.emit(sid, EvKind::Error { message: "cancelled".into() });
            return Ok(());
        }
    }
    app.emit(sid, EvKind::Error { message: format!("stopped after {MAX_TURNS} turns") });
    Ok(())
}

fn preview(s: &str) -> String {
    let s = s.trim();
    if s.len() > 400 {
        format!("{}…", &s[..400])
    } else {
        s.to_string()
    }
}

async fn approval_gate(
    app: &Arc<App>,
    sid: &str,
    name: &str,
    input: &serde_json::Value,
    detail: &str,
    cancelled: impl Fn() -> bool,
) -> Result<(), String> {
    let needs = {
        let cfg = app.cfg.read().unwrap();
        if cfg.approval == ApprovalMode::Auto {
            false
        } else {
            tools::needs_approval(&cfg, name, input)
        }
    };
    if !needs {
        return Ok(());
    }
    let (id, rx) = app.approvals.request(sid, name, detail);
    app.emit(sid, EvKind::ApprovalNeeded { approval_id: id.clone(), tool: name.into(), detail: detail.into() });
    let mut rx = rx;
    let allowed = loop {
        if cancelled() {
            app.approvals.resolve(&id, false);
            app.emit(sid, EvKind::ApprovalResolved { approval_id: id.clone(), allowed: false });
            return Err("cancelled".into());
        }
        match tokio::time::timeout(std::time::Duration::from_millis(200), &mut rx).await {
            Ok(Ok(v)) => break v,
            Ok(Err(_)) => break false,
            Err(_) => continue,
        }
    };
    app.emit(sid, EvKind::ApprovalResolved { approval_id: id, allowed });
    if allowed {
        Ok(())
    } else {
        Err("denied by user".into())
    }
}
