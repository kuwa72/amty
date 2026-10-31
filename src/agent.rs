use crate::app::App;
use crate::config::ApprovalMode;
use crate::provider;
use crate::tools;
use crate::types::*;
use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc;

const MAX_TURNS: usize = 40;
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Turn cap for a fresh agent run: in bypass (`ApprovalMode::Auto`) there is no
/// practical reason to stop the loop, so allow a very high cap. The cap is
/// re-evaluated every turn, so toggling bypass mid-run takes effect immediately.
fn turn_limit(bypass: bool) -> usize {
    if bypass {
        usize::MAX
    } else {
        MAX_TURNS
    }
}

/// Start an agent run in the background. Returns error if already running
/// or the session/provider is invalid.
pub fn start_run(app: &Arc<App>, session_id: &str, text: String) -> Result<()> {
    start_run_blocks(app, session_id, vec![Block::Text { text }])
}

/// Like [`start_run`] but with explicit blocks, so a message can carry images
/// (clipboard paste / file drop) as well as text.
pub fn start_run_blocks(app: &Arc<App>, session_id: &str, blocks: Vec<Block>) -> Result<()> {
    let sid = app
        .sessions
        .resolve(session_id)
        .ok_or_else(|| anyhow!("no such session '{session_id}'"))?;
    if app.begin_run(&sid).is_none() {
        anyhow::bail!("session '{sid}' already has a running turn");
    }
    let app = app.clone();
    tokio::spawn(async move {
        run(&app, &sid, blocks).await;
        app.end_run(&sid);
        // self-evolution: idle-restart (self_update) takes priority, then the
        // post-run reflection pass may distill lessons into memory/skills
        crate::evolve::after_run(&app, &sid);
    });
    Ok(())
}

async fn run(app: &Arc<App>, sid: &str, blocks: Vec<Block>) {
    if let Err(e) = run_inner(app, sid, blocks).await {
        mark_end(app, sid, &format!("error: {e}"));
        app.emit(sid, EvKind::Error { message: e.to_string() });
    }
}

/// Persist why a run stopped — a clean Done is self-evident from the
/// trailing assistant text, but errors/cancels otherwise leave the session
/// looking "mid-thought" forever with no explanation.
fn mark_end(app: &Arc<App>, sid: &str, reason: &str) {
    app.sessions.push_message(
        sid,
        ChatMessage::user(format!("[amty: run ended — {reason}]")),
    );
}

async fn run_inner(app: &Arc<App>, sid: &str, blocks: Vec<Block>) -> Result<()> {
    let cancel = app.runs.lock().unwrap().get(sid).cloned().ok_or_else(|| anyhow!("run not registered"))?;
    let cancelled = || cancel.load(Ordering::SeqCst);

    app.sessions.push_message(sid, ChatMessage::user_blocks(blocks));
    app.emit(sid, EvKind::Touched);

    // ensure MCP servers are up (cheap after first connect)
    app.mcp.ensure_connected().await;

    let mut turn = 0usize;
    let stop_msg = loop {
        // re-read each turn: toggling bypass mid-run lifts/reinstates the cap
        let limit = turn_limit(app.cfg.read().unwrap().approval == ApprovalMode::Auto);
        if turn >= limit {
            break format!("stopped after {limit} turns");
        }
        turn += 1;
        if cancelled() {
            mark_end(app, sid, "cancelled");
            app.emit(sid, EvKind::Error { message: "cancelled".into() });
            return Ok(());
        }

        // routing fields only — a full `get` would clone the whole message
        // history; the messages themselves are fetched after compaction below
        let (s_provider, s_model) = app
            .sessions
            .overrides(sid)
            .ok_or_else(|| anyhow!("session gone"))?;
        let (prov_name, prov_conf, model, system, system_tail, self_modify) = {
            let cfg = app.cfg.read().unwrap();
            let name = s_provider.unwrap_or_else(|| cfg.provider.clone());
            let conf = cfg.providers.get(&name).cloned().ok_or_else(|| anyhow!("provider '{name}' not defined"))?;
            let model = s_model.unwrap_or_else(|| conf.model.clone());
            // skills index + memory are volatile (re-scanned each turn): send
            // them as a tail so the static system_prompt stays cache-stable
            let tail = format!(
                "{}{}{}",
                crate::config::skills_prompt(),
                crate::evolve::memory_prompt(),
                crate::notes::prompt(&cfg),
            );
            (name, conf, model, cfg.system_prompt.clone(), tail, cfg.evolve.self_modify)
        };
        if model.is_empty() {
            anyhow::bail!("no model configured for provider '{prov_name}'");
        }

        let mut specs = tools::builtin_specs();
        if !self_modify {
            specs.retain(|s| s.name != "self_update");
        }
        specs.extend(app.mcp.specs());
        let max_tokens = prov_conf.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        let prov = provider::build(&prov_conf);
        // shrink the prompt with an LLM summary if it's near the context
        // window; the session log itself is never trimmed
        crate::compact::maybe_compact(app, sid, prov.as_ref(), &prov_conf, &model, &format!("{system}{system_tail}"), &specs, max_tokens, &cancelled).await;
        let session = app.sessions.get(sid).ok_or_else(|| anyhow!("session gone"))?;

        let req = ChatRequest {
            model,
            max_tokens,
            system: Some(system),
            system_tail: (!system_tail.is_empty()).then_some(system_tail),
            messages: session.request_messages(),
            tools: specs,
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let app2 = app.clone();
        let sid2 = sid.to_string();
        let fwd = tokio::spawn(async move {
            let mut blocks: Vec<Block> = vec![];
            let mut cur_text = String::new();
            let mut usage = crate::types::Usage::default();
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Text(t) => {
                        cur_text.push_str(&t);
                        app2.emit(&sid2, EvKind::Text { text: t });
                    }
                    StreamEvent::Usage(u) => {
                        usage.add(&u);
                        // one frame per provider call so the UI settles fast
                        app2.emit(&sid2, EvKind::Usage { usage: u, turns: 1, cumulative: usage });
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
            (blocks, usage)
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
        let (blocks, call_usage) = blocks;
        // persist the turn's token usage so totals survive a restart
        if !call_usage.is_zero() {
            app.sessions.add_usage(sid, &call_usage);
        }

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
        let mut interrupted = false;
        for b in &tool_uses {
            if cancelled() {
                interrupted = true;
                break;
            }
            let Block::ToolUse { id, name, input } = b.clone() else { continue };
            let detail = tools::summarize(&name, &input);

            // Read-only calls are stable within a session (a file does not
            // change behind our back mid-loop), so a repeated identical call
            // is answered from the memo instead of costing another round trip.
            // Covers both exact dupes in one turn (the old behaviour) and
            // repeats on a later turn, which is where the real waste was.
            let cache_key = cacheable(&name, &input)
                .then(|| (sid.to_string(), name.clone(), input.to_string()));
            if let Some((content, is_error)) = cache_key
                .as_ref()
                .and_then(|k| app.tool_cache.lock().unwrap().get(k).cloned())
            {
                app.emit(sid, EvKind::ToolStart { name: name.clone(), detail: detail.clone() });
                app.emit(sid, EvKind::ToolEnd {
                    name: name.clone(),
                    ok: !is_error,
                    preview: format!("cached: {}", preview(&content)),
                });
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content: format!("(identical call already answered earlier in this session)\n{content}"),
                    is_error,
                });
                continue;
            }

            app.emit(sid, EvKind::ToolStart { name: name.clone(), detail: detail.clone() });

            if let Err(e) = approval_gate(app, sid, &name, &input, &detail, cancelled).await {
                if e == "denied by user" {
                    crate::evolve::record_denied(app, &name);
                }
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content: format!("denied: {e}"),
                    is_error: true,
                });
                app.emit(sid, EvKind::ToolEnd { name, ok: false, preview: "denied".into() });
                continue;
            }

            // a `shell` call can create files anywhere under its workdir, so
            // snapshot the tree first; the post-call diff is what it created
            let before = if name == "shell" {
                shell_workdir(&input).map(|wd| { let s = snapshot_tree(&wd); (wd, s) })
            } else {
                None
            };
            let out = run_tool(app, &name, &input, &cancelled).await;
            // a write or an exec can invalidate every memoized read
            if !cacheable(&name, &input) {
                invalidate_tool_cache(app, sid);
            }
            crate::evolve::record_tool(app, &name, out.is_ok());
            match out {
                Ok(text) => {
                    if record_created(app, sid, &name, &input, before) > 0 {
                        app.emit(sid, EvKind::Touched);
                    }
                    app.emit(sid, EvKind::ToolEnd {
                        name: name.clone(),
                        ok: true,
                        preview: preview(&text),
                    });
                    if let Some(k) = cache_key {
                        let mut cache = app.tool_cache.lock().unwrap();
                        if cache.len() >= TOOL_CACHE_MAX {
                            // memo only — a miss just re-runs the tool
                            cache.clear();
                        }
                        cache.insert(k, (text.clone(), false));
                    }
                    results.push(Block::ToolResult { tool_use_id: id, content: text, is_error: false });
                }
                Err(e) => {
                    app.emit(sid, EvKind::ToolEnd { name: name.clone(), ok: false, preview: preview(&e) });
                    // do not memoize failures: the retry may legitimately work
                    // (a transient timeout, a file that now exists)
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
            mark_end(app, sid, "cancelled");
            app.emit(sid, EvKind::Error { message: "cancelled".into() });
            return Ok(());
        }
    };
    mark_end(app, sid, &stop_msg);
    app.emit(sid, EvKind::Error { message: stop_msg });
    Ok(())
}

/// Run a tool call while polling the cancel flag — dropping the future on
/// cancel aborts the HTTP request / kills the shell child (kill_on_drop).
/// Hard cap of 5 min so a stuck tool can't wedge the run forever.
async fn run_tool(
    app: &Arc<App>,
    name: &str,
    input: &serde_json::Value,
    cancelled: impl Fn() -> bool,
) -> Result<String, String> {
    let fut = async {
        if name.starts_with("mcp__") {
            app.mcp.call_tool(name, input.clone()).await
        } else {
            tools::execute(app, name, input).await
        }
    };
    let out = tokio::time::timeout(std::time::Duration::from_secs(300), async {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                r = &mut fut => break r,
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if cancelled() {
                        break Err("cancelled".into());
                    }
                }
            }
        }
    })
    .await;
    out.unwrap_or_else(|_| Err("timed out after 300s".into()))
}

fn preview(s: &str) -> String {
    truncate_preview(s.trim(), 400)
}

/// Is this call safe to memoize for the rest of the session?
///
/// Only reads whose answer cannot change as a side effect of the agent's own
/// work. `shell` is excluded on purpose even for things like `git status`: it is
/// arbitrary code, and a stale `cat`/`git diff` is worse than a cheap re-run.
/// Writes invalidate the whole session memo (see `invalidate_tool_cache`), so a
/// read after an edit is never served stale.
fn cacheable(name: &str, input: &serde_json::Value) -> bool {
    match name {
        "fs_read" | "fs_list" | "web_fetch" | "config_get" => true,
        // `mcp list` / `mcp tools` are pure inspection; other actions mutate
        "mcp" => matches!(input["action"].as_str().unwrap_or("list"), "list" | "tools"),
        _ => false,
    }
}

/// Cap on memoized read-only tool results across all sessions. Each entry
/// holds a full tool output (≤ `tools::MAX_OUT` bytes) plus its canonicalized
/// input key, so an unbounded map would grow for the life of the process —
/// deleted sessions' entries included. On overflow we just clear the memo.
const TOOL_CACHE_MAX: usize = 256;

/// Drop memoized reads for a session after anything that could change their
/// answer. Called from the write/exec tools' call sites via `run_tool`.
fn invalidate_tool_cache(app: &App, sid: &str) {
    app.purge_tool_cache(sid);
}

// ---------- session file tracking (issue #19) ----------

/// Directory names skipped when snapshotting a shell workdir — build
/// artifacts and VCS internals would flood the session file list.
const SNAPSHOT_SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv"];
/// Entries walked per snapshot; bounds the cost on huge trees.
const SNAPSHOT_CAP: usize = 50_000;
/// New paths attributed to a single `shell` call.
const MAX_NEW_PER_CMD: usize = 64;

/// Every path (files and dirs, recursively) under `root`, minus noise dirs.
/// Errors are ignored — a partial snapshot still yields a useful diff.
/// Symlinked dirs are not descended into (cycles / tree escapes).
fn snapshot_tree(root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= SNAPSHOT_CAP {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if SNAPSHOT_SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            let p = e.path();
            out.insert(p.clone());
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            }
            if out.len() >= SNAPSHOT_CAP {
                break;
            }
        }
    }
    out
}

/// Working dir a `shell` call runs in (same resolution as
/// `tools::run_shell`), absolutized so recorded paths are usable from the
/// GUI. `None` = don't snapshot: on Windows a UNC workdir is remapped to
/// the home dir inside cmd.exe, so diffing it would record the wrong tree.
fn shell_workdir(input: &serde_json::Value) -> Option<PathBuf> {
    let wd = input["workdir"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    if cfg!(windows) && wd.to_string_lossy().starts_with("\\\\") {
        return None;
    }
    Some(absolutize(&wd))
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// `canonicalize` on Windows yields verbatim `\\?\…` paths, which the GUI
/// can't open/reveal and which paste badly into other tools. Normalize back
/// to the plain form for storage; `\\?\UNC\server\share` → `\\server\share`.
/// A no-op on paths that aren't verbatim.
fn plain_path_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

/// Attribute created paths to the session: fs_write/fs_edit carry the path
/// directly; `shell` is diffed against the pre-call workdir snapshot.
/// Returns the number of *new* paths recorded.
fn record_created(
    app: &App,
    sid: &str,
    name: &str,
    input: &serde_json::Value,
    before: Option<(PathBuf, HashSet<PathBuf>)>,
) -> usize {
    match name {
        "fs_write" | "fs_edit" => {
            let Some(p) = input["path"].as_str() else { return 0 };
            let abs = std::fs::canonicalize(p).unwrap_or_else(|_| absolutize(Path::new(p)));
            app.sessions.add_file(sid, &plain_path_str(&abs), name) as usize
        }
        "shell" => {
            let Some((wd, before)) = before else { return 0 };
            let mut new: Vec<PathBuf> = snapshot_tree(&wd).difference(&before).cloned().collect();
            new.sort();
            new.into_iter()
                .take(MAX_NEW_PER_CMD)
                .filter(|p| app.sessions.add_file(sid, &plain_path_str(p), "shell"))
                .count()
        }
        _ => 0,
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
    if !needs || crate::evolve::auto_approved(app, name, input) {
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
        crate::evolve::record_approved(app, name, input);
        Ok(())
    } else {
        Err("denied by user".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cacheable_covers_reads_only() {
        assert!(cacheable("fs_read", &json!({"path": "a"})));
        assert!(cacheable("fs_list", &json!({"path": "."})));
        assert!(cacheable("web_fetch", &json!({"url": "https://x"})));
        assert!(cacheable("config_get", &json!({"key": "approval"})));
        assert!(cacheable("mcp", &json!({"action": "tools"})));
        assert!(cacheable("mcp", &json!({})), "default action is list");
        // never cached: arbitrary code, writes, and state-changing actions
        assert!(!cacheable("shell", &json!({"command": "git status"})));
        assert!(!cacheable("fs_write", &json!({"path": "a", "content": "b"})));
        assert!(!cacheable("fs_edit", &json!({})));
        assert!(!cacheable("mcp", &json!({"action": "remove"})));
        assert!(!cacheable("mcp_install", &json!({})));
        assert!(!cacheable("self_update", &json!({"action": "status"})));
        assert!(!cacheable("mcp__github__get_file_contents", &json!({})));
    }

    #[test]
    fn snapshot_diff_finds_new_paths_and_skips_noise() {
        let dir = std::env::temp_dir().join(format!("amty-snap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let before = snapshot_tree(&dir);
        std::fs::write(dir.join("made.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("out/inner")).unwrap();
        // noise trees are invisible to the diff even when newly created
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/built.o"), "x").unwrap();
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        let new: Vec<PathBuf> = snapshot_tree(&dir).difference(&before).cloned().collect();
        assert!(new.contains(&dir.join("made.txt")));
        assert!(new.contains(&dir.join("out")));
        assert!(new.contains(&dir.join("out/inner")));
        assert!(!new.iter().any(|p| p.starts_with(dir.join("target"))));
        assert!(!new.iter().any(|p| p.starts_with(dir.join(".git"))));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn verbatim_paths_normalize_to_plain() {
        // canonicalize on Windows returns \\?\… — stored paths must be plain
        assert_eq!(plain_path_str(Path::new(r"\\?\C:\a\b.md")), r"C:\a\b.md");
        assert_eq!(plain_path_str(Path::new(r"\\?\UNC\srv\share\f.txt")), r"\\srv\share\f.txt");
        assert_eq!(plain_path_str(Path::new("/tmp/x")), "/tmp/x");
        assert_eq!(plain_path_str(Path::new(r"C:\plain\already")), r"C:\plain\already");
    }

    #[test]
    fn shell_workdir_resolves_relative_and_absolute() {
        let abs = shell_workdir(&json!({"command": "x", "workdir": "/tmp"})).unwrap();
        assert_eq!(abs, PathBuf::from("/tmp"));
        // missing workdir falls back to the process cwd
        let cwd = shell_workdir(&json!({"command": "x"})).unwrap();
        assert_eq!(cwd, std::env::current_dir().unwrap());
    }
}
