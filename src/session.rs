use crate::types::{Block, ChatMessage, Role, Usage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

/// A file or directory created by a tool call during the session
/// (fs_write/fs_edit directly, shell via a workdir snapshot diff).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionFile {
    /// Absolute path.
    pub path: String,
    /// Tool that created it: fs_write | fs_edit | shell.
    pub tool: String,
    /// Last touch time (unix secs).
    pub ts: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    /// Provider name override; None follows config default.
    pub provider: Option<String>,
    pub model: Option<String>,
    pub created: u64,
    pub updated: u64,
    /// Monotonic touch order — breaks ties when `updated` shares a second.
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Cumulative provider-reported token usage for the whole session,
    /// accumulated across turns and persisted in the meta line.
    #[serde(default)]
    pub usage: Usage,
    /// Paths created by tool calls during this session (deduped by path),
    /// persisted in the meta line and listed in the GUI files tray.
    #[serde(default)]
    pub files: Vec<SessionFile>,
}

impl Session {
    fn new(provider: Option<String>, model: Option<String>, seq: u64) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
            title: "new chat".into(),
            provider,
            model,
            created: now,
            updated: now,
            seq,
            messages: vec![],
            usage: crate::types::Usage::default(),
            files: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub updated: u64,
    pub n_messages: usize,
    /// Session-lifetime token totals (persisted in the meta line).
    pub usage: crate::types::Usage,
    /// Paths created by tool calls in this session.
    pub n_files: usize,
}

/// One cross-session search hit: a matching title or a matching message.
#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    /// Session `updated` stamp, so callers can age/sort hits.
    pub updated: u64,
    /// `title` for a title hit, otherwise the message role, or `tool` for a
    /// tool call/result block.
    pub role: String,
    /// Index into `Session::messages` — `None` for a title hit.
    pub msg_index: Option<usize>,
    /// Single-line excerpt around the first match.
    pub snippet: String,
}

/// Characters kept before / after the match inside a snippet.
const SNIPPET_BEFORE: usize = 40;
const SNIPPET_AFTER: usize = 120;
/// Hits reported per session, so one long conversation can't flood results.
const MAX_HITS_PER_SESSION: usize = 3;
/// Cap on `Session::files` — a runaway `shell` diff can't grow it forever.
const MAX_SESSION_FILES: usize = 500;

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM` (UTC) from a unix timestamp — no date crate dependency.
/// Shared by the GUI sidebar and `amty search`.
pub fn fmt_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (h, m) = ((secs / 3600) % 24, (secs / 60) % 60);
    // days since 1970-01-01 -> civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// Squash runs of whitespace (newlines included) into single spaces, so a
/// snippet stays one line in the sidebar and in `amty search` output.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut spaced = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !spaced {
                out.push(' ');
                spaced = true;
            }
        } else {
            out.push(c);
            spaced = false;
        }
    }
    out.trim().to_string()
}

/// Excerpt around the first case-insensitive match of `needle` (already
/// lowercased). Windows are measured in characters, not bytes, so CJK text
/// (~3 bytes/char) still gets a readable snippet. `None` = no match.
fn snippet_around(text: &str, needle: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find(needle)?;
    // Lowercasing can change byte lengths for a few exotic characters; only
    // trust the byte offset when the length is unchanged (ASCII/CJK are).
    let anchor = if lower.len() == text.len() { lower[..at].chars().count() } else { 0 };
    let chars: Vec<char> = text.chars().collect();
    let start = anchor.saturating_sub(SNIPPET_BEFORE);
    let end = (anchor + SNIPPET_AFTER).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    Some(collapse_ws(&out))
}

pub struct SessionStore {
    pub map: RwLock<BTreeMap<String, Session>>,
    dir: PathBuf,
    seq: std::sync::atomic::AtomicU64,
}

/// Sessions persist as `<id>.jsonl`: first line `{"meta":{...}}`, then one
/// ChatMessage per line.
impl SessionStore {
    pub fn load(dir: PathBuf) -> Self {
        let mut map = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let mut lines = text.lines();
                    let meta: Option<Session> = lines
                        .next()
                        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                        .and_then(|v| serde_json::from_value(v.get("meta").cloned()?).ok());
                    if let Some(mut s) = meta {
                        s.messages = lines
                            .filter_map(|l| serde_json::from_str::<ChatMessage>(l).ok())
                            .collect();
                        map.insert(s.id.clone(), s);
                    }
                }
            }
        }
        let max_seq = map.values().map(|s| s.seq).max().unwrap_or(0);
        Self {
            map: RwLock::new(map),
            dir,
            seq: std::sync::atomic::AtomicU64::new(max_seq + 1),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Write the session's jsonl file. Serialized straight into a BufWriter
    /// so persisting neither deep-copies the session nor materializes the
    /// whole file in memory — peak overhead is one serialized line. Safe to
    /// call while holding the map write guard: this touches no shared state.
    fn persist(&self, s: &Session) {
        use std::io::Write as _;
        let _ = std::fs::create_dir_all(&self.dir);
        let Ok(f) = std::fs::File::create(self.dir.join(format!("{}.jsonl", s.id))) else { return };
        let mut w = std::io::BufWriter::new(f);
        if let Ok(m) = serde_json::to_string(&serde_json::json!({"meta": MetaOnly::from(s)})) {
            let _ = w.write_all(m.as_bytes()).and_then(|_| w.write_all(b"\n"));
        }
        for msg in &s.messages {
            if let Ok(l) = serde_json::to_string(msg) {
                let _ = w.write_all(l.as_bytes()).and_then(|_| w.write_all(b"\n"));
            }
        }
    }

    pub fn create(&self, provider: Option<String>, model: Option<String>) -> String {
        let s = Session::new(provider, model, self.next_seq());
        let id = s.id.clone();
        self.persist(&s);
        self.map.write().unwrap().insert(id.clone(), s);
        id
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.map.read().unwrap().get(id).cloned()
    }

    /// Provider/model overrides only — avoids cloning the whole message
    /// history when the caller just needs routing fields (agent loop).
    pub fn overrides(&self, id: &str) -> Option<(Option<String>, Option<String>)> {
        self.map
            .read()
            .unwrap()
            .get(id)
            .map(|s| (s.provider.clone(), s.model.clone()))
    }

    /// `id` may be a full id, unique prefix, or empty (→ most recently updated).
    pub fn resolve(&self, id: &str) -> Option<String> {
        let map = self.map.read().unwrap();
        if id.is_empty() {
            return map.values().max_by_key(|s| (s.updated, s.seq)).map(|s| s.id.clone());
        }
        if map.contains_key(id) {
            return Some(id.to_string());
        }
        let matches: Vec<&String> = map.keys().filter(|k| k.starts_with(id)).collect();
        if matches.len() == 1 {
            Some(matches[0].clone())
        } else {
            None
        }
    }

    pub fn list(&self) -> Vec<SessionMeta> {
        let mut v: Vec<SessionMeta> = self
            .map
            .read()
            .unwrap()
            .values()
            .map(|s| SessionMeta {
                id: s.id.clone(),
                title: s.title.clone(),
                provider: s.provider.clone(),
                model: s.model.clone(),
                updated: s.updated,
                n_messages: s.messages.len(),
                usage: s.usage,
                n_files: s.files.len(),
            })
            .collect();
        v.sort_by(|a, b| b.updated.cmp(&a.updated));
        v
    }

    pub fn push_message(&self, id: &str, msg: ChatMessage) {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            if s.title == "new chat" && msg.role == Role::User {
                let t = msg.text_content();
                if !t.trim().is_empty() {
                    s.title = t.trim().chars().take(40).collect();
                }
            }
            s.messages.push(msg);
            s.updated = now_secs();
            s.seq = self.next_seq();
            self.persist(s);
        }
    }

    /// Retitle a session (sidebar rename).
    pub fn set_title(&self, id: &str, title: &str) -> bool {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.title = title.to_string();
            s.updated = now_secs();
            s.seq = self.next_seq();
            self.persist(s);
            true
        } else {
            false
        }
    }

    pub fn set_fields(&self, id: &str, provider: Option<String>, model: Option<String>) {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.provider = provider.or(s.provider.take());
            s.model = model.or(s.model.take());
            s.updated = now_secs();
            s.seq = self.next_seq();
            self.persist(s);
        }
    }

    /// Replace the whole message list in place — used by auto-compaction,
    /// which swaps the older span for a single summary message.
    pub fn replace_messages(&self, id: &str, messages: Vec<ChatMessage>) -> bool {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.messages = messages;
            s.updated = now_secs();
            s.seq = self.next_seq();
            self.persist(s);
            true
        } else {
            false
        }
    }

    /// Record a path created by a tool call. Dedupes by path — a re-touch
    /// just refreshes tool/ts. Returns true when a new path was appended.
    pub fn add_file(&self, id: &str, path: &str, tool: &str) -> bool {
        let mut map = self.map.write().unwrap();
        let Some(s) = map.get_mut(id) else { return false };
        if let Some(f) = s.files.iter_mut().find(|f| f.path == path) {
            f.tool = tool.to_string();
            f.ts = now_secs();
            self.persist(s);
            return false;
        }
        if s.files.len() >= MAX_SESSION_FILES {
            return false;
        }
        s.files.push(SessionFile {
            path: path.to_string(),
            tool: tool.to_string(),
            ts: now_secs(),
        });
        self.persist(s);
        true
    }

    /// Fold one provider call's usage into the session total and persist it.
    pub fn add_usage(&self, id: &str, u: &crate::types::Usage) -> bool {
        if u.is_zero() {
            return false;
        }
        let mut map = self.map.write().unwrap();
        let Some(s) = map.get_mut(id) else { return false };
        s.usage.add(u);
        self.persist(s);
        true
    }

    /// Rewind: keep only the first `keep` messages of the session.
    pub fn truncate(&self, id: &str, keep: usize) -> bool {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.messages.truncate(keep);
            s.updated = now_secs();
            s.seq = self.next_seq();
            self.persist(s);
            true
        } else {
            false
        }
    }

    /// Branch: new session containing the first `keep` messages of `id`,
    /// inheriting its provider/model overrides. Returns the new session id.
    pub fn fork(&self, id: &str, keep: usize) -> Option<String> {
        let src = self.get(id)?;
        let new_id = self.create(src.provider.clone(), src.model.clone());
        {
            let mut map = self.map.write().unwrap();
            if let Some(s) = map.get_mut(&new_id) {
                s.title = format!("{} (fork)", src.title);
                s.messages = src.messages[..keep.min(src.messages.len())].to_vec();
                s.updated = now_secs();
                s.seq = self.next_seq();
                self.persist(s);
            }
        }
        Some(new_id)
    }

    pub fn delete(&self, id: &str) {
        self.map.write().unwrap().remove(id);
        let _ = std::fs::remove_file(self.dir.join(format!("{id}.jsonl")));
    }

    /// Cross-session full-text search over titles, message text and tool
    /// inputs. Sessions are all in memory, so this is a plain scan by
    /// reference under the read lock — cloning every session's message
    /// history first (the old behaviour) doubled RSS for the scan's
    /// duration. Results are newest-first and capped at `limit` overall and
    /// `MAX_HITS_PER_SESSION` per session.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return vec![];
        }
        let map = self.map.read().unwrap();
        let mut sessions: Vec<&Session> = map.values().collect();
        sessions.sort_by(|a, b| (b.updated, b.seq).cmp(&(a.updated, a.seq)));
        let mut hits = Vec::new();
        for s in sessions {
            search_session(s, &needle, &mut hits, limit);
            if hits.len() >= limit {
                hits.truncate(limit);
                break;
            }
        }
        hits
    }

    /// Search inside a single session (id, unique prefix, or empty = newest).
    pub fn search_in(&self, id: &str, query: &str, limit: usize) -> Vec<SearchHit> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return vec![];
        }
        let s = self.resolve(id).and_then(|sid| self.get(&sid));
        let Some(s) = s else { return vec![] };
        let mut hits = Vec::new();
        search_session(&s, &needle, &mut hits, limit);
        hits.truncate(limit);
        hits
    }
}

/// Collect up to `MAX_HITS_PER_SESSION` hits for one session. `needle` must
/// already be lowercased and trimmed.
fn search_session(s: &Session, needle: &str, hits: &mut Vec<SearchHit>, limit: usize) {
    let mut per_session = 0usize;
    // push is a plain fn (not a closure) so the loop can still read
    // `per_session` for the cap checks
    let push = |role: &str, msg_index: Option<usize>, snippet: String, per: &mut usize, hits: &mut Vec<SearchHit>| {
        if *per >= MAX_HITS_PER_SESSION || hits.len() >= limit {
            return;
        }
        *per += 1;
        hits.push(SearchHit {
            id: s.id.clone(),
            title: s.title.clone(),
            updated: s.updated,
            role: role.to_string(),
            msg_index,
            snippet,
        });
    };

    if let Some(sn) = snippet_around(&s.title, needle) {
        push("title", None, sn, &mut per_session, hits);
    }
    for (i, m) in s.messages.iter().enumerate() {
        if hits.len() >= limit || per_session >= MAX_HITS_PER_SESSION {
            return;
        }
        for b in &m.blocks {
            match b {
                Block::Text { text } => {
                    if let Some(sn) = snippet_around(text, needle) {
                        push(msg_role(&m.role), Some(i), sn, &mut per_session, hits);
                    }
                }
                Block::ToolUse { name, input, .. } => {
                    // match the tool name and its arguments on one line, so
                    // `amty search grep` finds the shell call that ran it
                    let rendered = match input {
                        serde_json::Value::String(v) => v.clone(),
                        _ => input.to_string(),
                    };
                    let label = format!("{name} {rendered}");
                    if let Some(sn) = snippet_around(&label, needle) {
                        push("tool", Some(i), sn, &mut per_session, hits);
                    }
                }
                Block::ToolResult { content, .. } => {
                    if let Some(sn) = snippet_around(content, needle) {
                        push("tool", Some(i), sn, &mut per_session, hits);
                    }
                }
                Block::Image { .. } => {}
            }
        }
    }
}

fn msg_role(r: &Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// The meta line stores everything except messages.
#[derive(Serialize, Deserialize)]
struct MetaOnly {
    id: String,
    title: String,
    provider: Option<String>,
    model: Option<String>,
    created: u64,
    updated: u64,
    seq: u64,
    /// Session-lifetime token totals (see `Session::usage`).
    #[serde(default)]
    usage: crate::types::Usage,
    /// Paths created by tool calls (see `Session::files`).
    #[serde(default)]
    files: Vec<SessionFile>,
}

impl From<&Session> for MetaOnly {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id.clone(),
            title: s.title.clone(),
            provider: s.provider.clone(),
            model: s.model.clone(),
            created: s.created,
            updated: s.updated,
            seq: s.seq,
            usage: s.usage,
            files: s.files.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Block;

    fn temp_store() -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("amty-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (SessionStore::load(dir.clone()), dir)
    }

    #[test]
    fn roundtrip() {
        let (store, dir) = temp_store();
        let id = store.create(Some("openai".into()), Some("m1".into()));
        store.push_message(&id, ChatMessage::user("hello world this is a title"));
        store.push_message(&id, ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Text { text: "hi".into() },
                Block::ToolUse { id: "t1".into(), name: "fs_read".into(), input: serde_json::json!({"path":"/tmp"}) },
            ],
        });
        drop(store);
        let store2 = SessionStore::load(dir.clone());
        let s = store2.get(&id).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.model.as_deref(), Some("m1"));
        assert!(s.title.contains("hello"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn usage_accumulates_and_survives_reload() {
        let (store, dir) = temp_store();
        let id = store.create(Some("commandcode".into()), Some("m".into()));
        let a = crate::types::Usage {
            input: 3000, output: 20, cache_read: 2900, cache_write: 10, reasoning: 5,
        };
        let b = crate::types::Usage { input: 1000, output: 40, cache_read: 900, ..Default::default() };
        assert!(store.add_usage(&id, &a));
        assert!(store.add_usage(&id, &b));
        // a zero-usage call must not rewrite the session
        assert!(!store.add_usage(&id, &crate::types::Usage::default()));

        let total = store.get(&id).unwrap().usage;
        assert_eq!(total.input, 4000);
        assert_eq!(total.output, 60);
        assert_eq!(total.cache_read, 3800);
        assert_eq!(total.uncached_input(), 200);
        // and the listing exposes it
        let meta = store.list().into_iter().find(|m| m.id == id).unwrap();
        assert_eq!(meta.usage, total);

        // totals must come back from disk (they live in the meta line)
        drop(store);
        let store2 = SessionStore::load(dir.clone());
        assert_eq!(store2.get(&id).unwrap().usage, total);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn files_are_tracked_deduped_and_persisted() {
        let (store, dir) = temp_store();
        let id = store.create(None, None);
        assert!(store.add_file(&id, "/tmp/made.txt", "fs_write"));
        assert!(store.add_file(&id, "/tmp/outdir", "shell"));
        // same path again: deduped (list stays), tool/ts refreshed
        assert!(!store.add_file(&id, "/tmp/made.txt", "fs_edit"));
        assert!(!store.add_file("no-such-session", "/x", "fs_write"));
        let s = store.get(&id).unwrap();
        assert_eq!(s.files.len(), 2);
        assert_eq!(s.files[0].path, "/tmp/made.txt");
        assert_eq!(s.files[0].tool, "fs_edit");
        let meta = store.list().into_iter().find(|m| m.id == id).unwrap();
        assert_eq!(meta.n_files, 2);
        // the list lives in the meta line, so it survives a reload
        drop(store);
        let store2 = SessionStore::load(dir.clone());
        assert_eq!(store2.get(&id).unwrap().files.len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Old session files have no `usage` in the meta line and must still load.
    #[test]
    fn sessions_without_usage_field_still_load() {
        let dir = std::env::temp_dir().join(format!("amty-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("legacy1.jsonl"),
            serde_json::json!({"meta": {
                "id": "legacy1", "title": "old", "provider": null, "model": null,
                "created": 1, "updated": 2, "seq": 1,
            }})
            .to_string()
                + "\n"
                + &serde_json::to_string(&ChatMessage::user("hi")).unwrap()
                + "\n",
        )
        .unwrap();
        let store = SessionStore::load(dir.clone());
        let s = store.get("legacy1").expect("legacy session loads");
        assert_eq!(s.messages.len(), 1);
        assert!(s.usage.is_zero());
        assert!(s.files.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn truncate_and_fork() {
        let (store, dir) = temp_store();
        let id = store.create(Some("p".into()), Some("m".into()));
        store.push_message(&id, ChatMessage::user("one"));
        store.push_message(&id, ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::Text { text: "a".into() }],
        });
        store.push_message(&id, ChatMessage::user("two"));
        let fork = store.fork(&id, 2).unwrap();
        let f = store.get(&fork).unwrap();
        assert_eq!(f.messages.len(), 2);
        assert_eq!(f.provider.as_deref(), Some("p"));
        assert!(f.title.contains("fork"));
        assert!(store.truncate(&id, 1));
        assert_eq!(store.get(&id).unwrap().messages.len(), 1);
        drop(store);
        let store2 = SessionStore::load(dir.clone());
        assert_eq!(store2.get(&fork).unwrap().messages.len(), 2);
        assert_eq!(store2.get(&id).unwrap().messages.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resolve_prefix_and_latest() {
        let (store, dir) = temp_store();
        let a = store.create(None, None);
        let b = store.create(None, None);
        assert_eq!(store.resolve(""), Some(b.clone())); // latest
        assert_eq!(store.resolve(&a), Some(a.clone()));
        assert_eq!(store.resolve(&a[..4]), Some(a));
        std::fs::remove_dir_all(dir).ok();
    }

    fn seed_search_store() -> (SessionStore, PathBuf, String, String) {
        let (store, dir) = temp_store();
        let a = store.create(None, None);
        store.push_message(&a, ChatMessage::user("deploy the WSL build please"));
        store.push_message(&a, ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Text { text: "running cargo build --release --target x86_64-pc-windows-gnu".into() },
                Block::ToolUse {
                    id: "t1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "cargo test --all-targets"}),
                },
            ],
        });
        let b = store.create(None, None);
        store.set_title(&b, "release checklist");
        store.push_message(&b, ChatMessage::user("nothing relevant here"));
        (store, dir, a, b)
    }

    #[test]
    fn search_matches_text_and_tool_input() {
        let (store, dir, a, _b) = seed_search_store();
        let hits = store.search("cargo", 20);
        assert!(hits.iter().any(|h| h.id == a && h.role == "assistant"));
        // tool calls are searchable: name + arguments
        let tool_hits = store.search("--all-targets", 20);
        assert!(tool_hits.iter().any(|h| h.id == a && h.role == "tool"));
        // and tool names
        assert!(store.search("shell", 20).iter().any(|h| h.id == a));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn search_matches_titles_and_is_case_insensitive() {
        let (store, dir, _a, b) = seed_search_store();
        let hits = store.search("RELEASE CHECKLIST", 20);
        assert!(hits.iter().any(|h| h.id == b && h.role == "title"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn search_returns_empty_for_blank_or_absent_queries() {
        let (store, dir, _a, _b) = seed_search_store();
        assert!(store.search("", 20).is_empty());
        assert!(store.search("   ", 20).is_empty());
        assert!(store.search("zzz-not-present-zzz", 20).is_empty());
        assert!(store.search("cargo", 0).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn search_respects_limit_and_per_session_cap() {
        let (store, dir) = temp_store();
        let id = store.create(None, None);
        for i in 0..10 {
            store.push_message(&id, ChatMessage::user(format!("needle message {i}")));
        }
        // one session cannot flood: at most MAX_HITS_PER_SESSION hits
        let hits = store.search("needle", 50);
        assert_eq!(hits.len(), MAX_HITS_PER_SESSION);
        // an explicit limit is honoured across sessions
        let hits = store.search("needle", 2);
        assert_eq!(hits.len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn search_in_limits_to_one_session() {
        let (store, dir, a, _b) = seed_search_store();
        let all = store.search("cargo", 20);
        let only = store.search_in(&a, "cargo", 20);
        assert!(!only.is_empty());
        assert_eq!(all.len(), only.len());
        assert!(only.iter().all(|h| h.id == a));
        // an unknown session yields nothing rather than an error
        assert!(store.search_in("nope", "cargo", 20).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn snippet_is_single_line_and_cjk_safe() {
        let long = format!("{}{}{}", "あ".repeat(200), "キーワード", "い".repeat(200));
        // note: Unicode lowercasing leaves kana alone, so CJK matching is a
        // plain substring test — only Latin case varies
        let sn = snippet_around(&long, "キーワード").expect("CJK substring match");
        assert!(!sn.contains('\n'));
        assert!(sn.contains("キーワード"));
        assert!(sn.len() < long.len());
        assert!(sn.starts_with('…') && sn.ends_with('…'));
        // whitespace (newlines included) collapses to single spaces
        let sn = snippet_around("line one\n\n  line two hit here", "hit").unwrap();
        assert_eq!(sn, "line one line two hit here");
        // no match -> no snippet, and offsets never split a character
        assert!(snippet_around("abc", "zzz").is_none());
        let wide = snippet_around(&"漢".repeat(400), "漢").unwrap();
        assert_eq!(wide.chars().count(), SNIPPET_AFTER + 1); // + leading ellipsis
        // the caller lowercases the needle, so Latin case folds
        assert!(snippet_around("Cargo Test", "cargo test").is_some());
    }

    #[test]
    fn hits_come_back_newest_first() {
        let (store, dir) = temp_store();
        let old = store.create(None, None);
        store.push_message(&old, ChatMessage::user("shared term"));
        let new = store.create(None, None);
        store.push_message(&new, ChatMessage::user("shared term again"));
        let hits = store.search("shared term", 10);
        assert_eq!(hits[0].id, new, "newest session first");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Memory probe, not a real test — run manually with:
    /// `cargo test --release mem_report -- --ignored --nocapture`
    /// Prints VmRSS/VmHWM around the store's heavy paths on synthetic data
    /// (~100 sessions × 40 × ~16 KiB messages ≈ 64 MiB retained).
    #[test]
    #[ignore]
    fn mem_report() {
        fn rss() -> String {
            std::fs::read_to_string("/proc/self/status")
                .unwrap_or_default()
                .lines()
                .filter(|l| l.starts_with("VmRSS") || l.starts_with("VmHWM"))
                .map(|l| l.trim().to_string())
                .collect::<Vec<_>>()
                .join("  ")
        }
        let dir = std::env::temp_dir().join(format!("amty-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = "the quick brown fox jumps over the lazy dog 0123456789abcdef\n".repeat(256);
        let msg = serde_json::to_string(&ChatMessage::user(&body)).unwrap();
        let ids: Vec<String> = (0..100)
            .map(|i| {
                let id = format!("mem{i:06}");
                let meta = serde_json::json!({"meta": {
                    "id": id, "title": format!("session {i}"), "provider": null,
                    "model": null, "created": i, "updated": i, "seq": i,
                }});
                let mut text = meta.to_string() + "\n";
                for _ in 0..40 {
                    text.push_str(&msg);
                    text.push('\n');
                }
                std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
                id
            })
            .collect();
        let store = SessionStore::load(dir.clone());
        println!("after load          {}", rss());
        store.push_message(&ids[0], ChatMessage::user("one more"));
        println!("after push_message  {}", rss());
        store.set_title(&ids[1], "renamed");
        println!("after set_title     {}", rss());
        let hits = store.search("brown fox", 50);
        println!("after search ({} hits) {}", hits.len(), rss());
        let s = store.get(&ids[0]).unwrap();
        println!("after get ({} msgs)  {}", s.messages.len(), rss());
        drop(store);
        std::fs::remove_dir_all(dir).ok();
    }
}
