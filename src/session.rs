use crate::types::{ChatMessage, Role};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

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
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    fn persist(&self, s: &Session) {
        let _ = std::fs::create_dir_all(&self.dir);
        let mut out = String::new();
        if let Ok(m) = serde_json::to_string(&serde_json::json!({"meta": MetaOnly::from(s)})) {
            out.push_str(&m);
            out.push('\n');
        }
        for msg in &s.messages {
            if let Ok(l) = serde_json::to_string(msg) {
                out.push_str(&l);
                out.push('\n');
            }
        }
        let _ = std::fs::write(self.dir.join(format!("{}.jsonl", s.id)), out);
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
            let s = s.clone();
            drop(map);
            self.persist(&s);
        }
    }

    /// Retitle a session (sidebar rename).
    pub fn set_title(&self, id: &str, title: &str) -> bool {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.title = title.to_string();
            s.updated = now_secs();
            s.seq = self.next_seq();
            let s = s.clone();
            drop(map);
            self.persist(&s);
            true
        } else {
            false
        }
    }

    pub fn set_fields(&self, id: &str, provider: Option<String>, model: Option<String>) {        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.provider = provider.or(s.provider.take());
            s.model = model.or(s.model.take());
            s.updated = now_secs();
            s.seq = self.next_seq();
            let s = s.clone();
            drop(map);
            self.persist(&s);
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
            let s = s.clone();
            drop(map);
            self.persist(&s);
            true
        } else {
            false
        }
    }

    /// Rewind: keep only the first `keep` messages of the session.
    pub fn truncate(&self, id: &str, keep: usize) -> bool {
        let mut map = self.map.write().unwrap();
        if let Some(s) = map.get_mut(id) {
            s.messages.truncate(keep);
            s.updated = now_secs();
            s.seq = self.next_seq();
            let s = s.clone();
            drop(map);
            self.persist(&s);
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
                let s = s.clone();
                drop(map);
                self.persist(&s);
            }
        }
        Some(new_id)
    }

    pub fn delete(&self, id: &str) {
        self.map.write().unwrap().remove(id);
        let _ = std::fs::remove_file(self.dir.join(format!("{id}.jsonl")));
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
}
