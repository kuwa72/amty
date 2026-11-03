//! TokenEconomizer, amty-native (port of microsoft/TokenEconomizer).
//!
//! Cost is paid on every turn (system prompt + tool definitions + history), so
//! the levers are:
//!
//! - **Micro-skills** (`te.enabled`): short per-repository facts learned in
//!   earlier sessions, injected into the system tail. Facts live in the notes
//!   store as `tokeneconomizer/<repo-slug>` — one `- support: N | fact` line
//!   each — and only facts confirmed `MIN_SUPPORT` times are injected, so a
//!   one-off guess never becomes standing advice.
//! - **Tool pruning** (`te.prune`): MCP tools offered `MIN_OFFERED` times and
//!   never called are dropped from the request. `te.keep` (or `amty te untool`)
//!   brings one back; builtins are never pruned.
//! - **Loop guard** (`te.loopguard`): a call that fails twice unchanged gets a
//!   one-time nudge, a fourth failure stops the turn.
//!
//! The repo key comes from `te.repo`, else the last `shell` workdir this
//! process saw, else the process cwd — the same "identify by its directory"
//! fallback upstream uses when the directory is not a git checkout.

use crate::app::App;
use crate::config::Config;
use crate::types::EvKind;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Note-name prefix holding repo facts.
pub const NOTE_PREFIX: &str = "tokeneconomizer/";
/// Independent confirmations before a fact is injected.
pub const MIN_SUPPORT: u32 = 3;
/// Char cap on the injected block — it rides every turn.
const INJECT_MAX: usize = 1500;
/// Times a tool must be offered with zero calls before it is pruned.
pub const MIN_OFFERED: u64 = 20;

/// The directory facts are keyed by: `te.repo`, else the last shell workdir,
/// else the process cwd.
pub fn current(cfg: &Config, last_shell_dir: Option<PathBuf>) -> PathBuf {
    if let Some(r) = cfg.te.repo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return PathBuf::from(r);
    }
    last_shell_dir
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// Nearest ancestor of `dir` holding `.git`, else `dir` itself.
pub fn repo_root(dir: &Path) -> PathBuf {
    let mut cur = dir.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return cur;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return dir.to_path_buf(),
        }
    }
}

/// Note slug for a repo: its directory name, lowercased, non-alnum → `-`.
pub fn slug(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "repo".into() } else { out }
}

/// One line of a repo note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fact {
    pub support: u32,
    pub text: String,
}

/// Parse `- support: N | text` lines; anything else (headings, prose) is
/// ignored so the note stays readable in an editor.
pub fn parse_facts(body: &str) -> Vec<Fact> {
    let mut out = Vec::new();
    for line in body.lines() {
        let Some(rest) = line.trim().strip_prefix("- ") else { continue };
        let Some((head, text)) = rest.split_once('|') else { continue };
        let Some(n) = head.trim().strip_prefix("support:").and_then(|n| n.trim().parse().ok()) else {
            continue;
        };
        let text = text.trim();
        if !text.is_empty() {
            out.push(Fact { support: n, text: text.to_string() });
        }
    }
    out
}

/// Body after learning `fact` again: the matching line gains one support
/// point, a new fact starts at 0 (so it must be confirmed before it is
/// injected). Everything else is kept verbatim.
pub fn merge_fact(body: &str, fact: &str) -> String {
    let fact = fact.trim();
    let mut lines: Vec<String> = body.lines().map(str::to_string).collect();
    for line in lines.iter_mut() {
        let Some(rest) = line.trim().strip_prefix("- ") else { continue };
        let Some((head, text)) = rest.split_once('|') else { continue };
        if text.trim() != fact {
            continue;
        }
        let Some(n) = head.trim().strip_prefix("support:").and_then(|n| n.trim().parse::<u32>().ok()) else {
            continue;
        };
        *line = format!("- support: {} | {fact}", n.saturating_add(1));
        return lines.join("\n");
    }
    if !lines.is_empty() && lines.last().is_some_and(|l| !l.trim().is_empty()) {
        lines.push(String::new());
    }
    lines.push(format!("- support: 0 | {fact}"));
    lines.join("\n")
}

/// Note name holding `root`'s facts.
pub fn note_name(root: &Path) -> String {
    format!("{NOTE_PREFIX}{}", slug(root))
}

/// Body of `root`'s facts note ("" when there is none).
pub fn note_body(cfg: &Config, root: &Path) -> String {
    crate::notes::read(&crate::notes::dir(cfg), &note_name(root)).unwrap_or_default()
}

/// Facts strong enough to inject.
pub fn trusted(body: &str) -> Vec<String> {
    parse_facts(body)
        .into_iter()
        .filter(|f| f.support >= MIN_SUPPORT)
        .map(|f| f.text)
        .collect()
}

/// System-tail block for `dir`; "" when disabled, unknown or empty.
pub fn prompt(cfg: &Config, dir: &Path) -> String {
    if !cfg.te.enabled || dir.as_os_str().is_empty() {
        return String::new();
    }
    let root = repo_root(dir);
    let facts = trusted(&note_body(cfg, &root));
    if facts.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n\nTokenEconomizer — facts learned in {} in earlier sessions \
         (apply them without re-deriving; drop any that no longer hold):\n",
        root.display()
    );
    for f in facts {
        if out.len() + f.len() + 3 > INJECT_MAX {
            break;
        }
        out.push_str("- ");
        out.push_str(&f);
        out.push('\n');
    }
    out
}

/// MCP tool names to leave out of the request: never called, offered at least
/// `MIN_OFFERED` times, and not in `te.keep`.
pub fn pruned(cfg: &Config, stats: &crate::evolve::Stats) -> Vec<String> {
    if !cfg.te.prune {
        return Vec::new();
    }
    stats
        .tools
        .iter()
        .filter(|(name, s)| {
            name.starts_with("mcp__")
                && s.calls == 0
                && s.offered >= MIN_OFFERED
                && !cfg.te.keep.iter().any(|k| k == *name)
        })
        .map(|(n, _)| n.clone())
        .collect()
}

/// What to do when the same call fails again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Nudge,
    Stop,
}

/// 2nd identical failure → one nudge; 4th → stop the turn.
pub fn repeat_action(failures: u32) -> Option<Repeat> {
    match failures {
        2 => Some(Repeat::Nudge),
        n if n >= 4 => Some(Repeat::Stop),
        _ => None,
    }
}

/// `GET /v1/te` / `amty te` — toggles, the repo facts in play, and the tool
/// table that pruning reads.
pub fn status_json(app: &App) -> Value {
    let cfg = app.cfg.read().unwrap();
    let dir = current(&cfg, app.te_dir.lock().unwrap().clone());
    let root = repo_root(&dir);
    let facts = parse_facts(&note_body(&cfg, &root));
    let trusted_n = facts.iter().filter(|f| f.support >= MIN_SUPPORT).count();
    let injected = prompt(&cfg, &dir).len();
    let stats = app.stats.lock().unwrap();
    let dropped = pruned(&cfg, &stats);
    let tools: Vec<Value> = stats
        .tools
        .iter()
        .filter(|(n, _)| n.starts_with("mcp__"))
        .map(|(n, t)| {
            json!({
                "name": n,
                "calls": t.calls,
                "errors": t.errors,
                "offered": t.offered,
                "pruned": dropped.iter().any(|p| p == n),
            })
        })
        .collect();
    json!({
        "enabled": cfg.te.enabled,
        "prune": cfg.te.prune,
        "loopguard": cfg.te.loopguard,
        "repo": {
            "dir": dir.display().to_string(),
            "root": root.display().to_string(),
            "note": note_name(&root),
            "pinned": cfg.te.repo.is_some(),
        },
        "facts": { "total": facts.len(), "trusted": trusted_n, "injected_chars": injected, "items": facts },
        "keep": cfg.te.keep,
        "pruned": dropped,
        "tools": tools,
    })
}

/// Bring a tool back: pin it in `te.keep` and reset its offer count so pruning
/// has to re-earn it.
pub fn untool(app: &Arc<App>, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("tool name required".into());
    }
    {
        let mut cfg = app.cfg.write().unwrap();
        if !cfg.te.keep.iter().any(|k| k == name) {
            cfg.te.keep.push(name.to_string());
            cfg.te.keep.sort();
        }
        cfg.save(&app.cfg_path).map_err(|e| e.to_string())?;
    }
    {
        let mut st = app.stats.lock().unwrap();
        if let Some(t) = st.tools.get_mut(name) {
            t.offered = 0;
        }
    }
    crate::evolve::save_stats(app);
    app.emit("", EvKind::Touched);
    Ok(format!("{name} is back in the request (te.keep)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_facts_reads_support_lines_only() {
        let body = "# facts\n\n- support: 3 | build: `cargo build`\n- support: 0 | maybe\nrandom prose\n- support: x | broken";
        let f = parse_facts(body);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0], Fact { support: 3, text: "build: `cargo build`".into() });
        assert_eq!(trusted(body), vec!["build: `cargo build`".to_string()]);
    }

    #[test]
    fn merge_fact_bumps_and_appends() {
        let body = "- support: 2 | uses pytest";
        let once = merge_fact(body, "uses pytest");
        assert_eq!(once, "- support: 3 | uses pytest");
        let new = merge_fact(&once, "venv at .venv");
        assert_eq!(new, "- support: 3 | uses pytest\n\n- support: 0 | venv at .venv");
        // an appended fact is not trusted until confirmed
        assert_eq!(trusted(&new), vec!["uses pytest".to_string()]);
    }

    #[test]
    fn slug_is_stable_and_filesystem_safe() {
        assert_eq!(slug(Path::new("/home/x/My Repo")), "my-repo");
        assert_eq!(slug(Path::new("/")), "repo");
    }

    #[test]
    fn repo_root_walks_up_to_the_git_dir() {
        let dir = std::env::temp_dir().join(format!("amty-te-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(repo_root(&dir.join("a/b")), dir);
        assert_eq!(repo_root(&dir.join("a")), dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_injects_only_trusted_facts() {
        let dir = std::env::temp_dir().join(format!("amty-te-prompt-{}", uuid::Uuid::new_v4()));
        let notes = dir.join("notes");
        std::fs::create_dir_all(notes.join("tokeneconomizer")).unwrap();
        let repo = dir.join("myrepo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            notes.join("tokeneconomizer/myrepo.md"),
            "# facts\n- support: 3 | build: `cargo build --release`\n- support: 1 | guess\n",
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.notes.dir = Some(notes.to_string_lossy().into_owned());
        let block = prompt(&cfg, &repo.join("sub"));
        assert!(block.contains("cargo build --release"), "{block}");
        assert!(!block.contains("guess"), "{block}");

        cfg.te.enabled = false;
        assert!(prompt(&cfg, &repo).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pruning_needs_offers_calls_and_keep_list() {
        use crate::evolve::{Stats, ToolStat};
        let mut cfg = Config::default();
        cfg.te.prune = true;
        let mut st = Stats::default();
        st.tools.insert("mcp__s__dead".into(), ToolStat { offered: 20, ..Default::default() });
        st.tools.insert("mcp__s__used".into(), ToolStat { offered: 40, calls: 1, ..Default::default() });
        st.tools.insert("mcp__s__new".into(), ToolStat { offered: 3, ..Default::default() });
        st.tools.insert("fs_read".into(), ToolStat { offered: 99, ..Default::default() });
        assert_eq!(pruned(&cfg, &st), vec!["mcp__s__dead".to_string()]);
        cfg.te.keep.push("mcp__s__dead".into());
        assert!(pruned(&cfg, &st).is_empty());
        cfg.te.prune = false;
        cfg.te.keep.clear();
        assert!(pruned(&cfg, &st).is_empty());
    }

    #[test]
    fn repeat_guard_nudges_once_then_stops() {
        assert_eq!(repeat_action(1), None);
        assert_eq!(repeat_action(2), Some(Repeat::Nudge));
        assert_eq!(repeat_action(3), None);
        assert_eq!(repeat_action(4), Some(Repeat::Stop));
        assert_eq!(repeat_action(9), Some(Repeat::Stop));
    }
}
