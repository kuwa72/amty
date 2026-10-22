//! Plain-markdown knowledge store. Notes live under `notes.dir` (default
//! `<data_dir>/notes/`); a note's `name` is its `/`-separated path relative
//! to that root, without the `.md` suffix (`projects/amty/decisions` →
//! `<root>/projects/amty/decisions.md`). The root is deliberately a normal
//! directory of markdown files so it doubles as an Obsidian vault — humans
//! edit in their editor of choice, agents via the note_* tools / API / CLI.
use crate::config::Config;
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct NoteMeta {
    /// Relative path without `.md`, `/`-separated.
    pub name: String,
    /// First `# heading`, else first non-empty line, else the file stem.
    pub title: String,
    pub bytes: u64,
    /// File mtime (unix secs).
    pub updated: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct NoteHit {
    pub name: String,
    pub title: String,
    pub updated: u64,
    /// Single-line excerpt around the first match.
    pub snippet: String,
}

/// Where notes live: `notes.dir` (with `~` expanded) or `<data_dir>/notes`.
pub fn dir(cfg: &Config) -> PathBuf {
    if let Some(d) = cfg.notes.dir.as_deref().filter(|s| !s.trim().is_empty()) {
        let d = d.trim();
        if let Some(rest) = d.strip_prefix("~/").or_else(|| d.strip_prefix("~\\")) {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        return PathBuf::from(d);
    }
    crate::config::data_dir().join("notes")
}

/// One line appended to the system tail each turn so the agent knows the
/// store exists and where it is.
pub fn prompt(cfg: &Config) -> String {
    format!(
        "\n\nNotes — a durable markdown knowledge store at {} (a normal dir; \
         also usable as an Obsidian vault). Use note_list/note_read/\
         note_write/note_delete/note_search to keep and recall stable facts, \
         decisions and references. Prefer it over MEMORY.md for topic-level \
         notes; MEMORY.md is for how-to-work lessons.",
        dir(cfg).display()
    )
}

/// Validate and normalize a note name into its file path under `root`.
/// Accepts `a/b` or `a/b.md`; rejects traversal, absolutes and characters a
/// synced vault can't carry (`<>"|?*` and `:` are illegal on Windows/macOS
/// filesystems that Obsidian vaults sync through).
fn path_for(root: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim().replace('\\', "/");
    let name = name.strip_suffix(".md").unwrap_or(&name);
    if name.is_empty() {
        return Err("note name required".into());
    }
    if name.len() > 200 {
        return Err("note name too long (max 200 chars)".into());
    }
    let mut rel = PathBuf::new();
    for part in name.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.starts_with('.')
            || part.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c))
            || part.ends_with([' ', '.'])
        {
            return Err(format!("bad note name '{name}'"));
        }
        rel.push(part);
    }
    if rel.as_os_str().is_empty() {
        return Err("note name required".into());
    }
    // paranoia: the cleaned path must still be strictly relative
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("bad note name '{name}'"));
    }
    Ok(root.join(rel).with_extension("md"))
}

/// The `.md` file a note name maps to (for "reveal in file manager").
pub(crate) fn file_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    path_for(root, name)
}

/// Note title: first `# ` heading, else the first non-empty line.
fn title_of(text: &str, fallback: &str) -> String {
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(h) = l.strip_prefix('#') {
            let h = h.trim_start_matches('#').trim();
            if !h.is_empty() {
                return h.chars().take(120).collect();
            }
        }
        return l.trim_start_matches(['#', '>', '-', '*']).trim().chars().take(120).collect();
    }
    fallback.to_string()
}

fn meta(root: &Path, path: &Path) -> Option<NoteMeta> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let rel = path.strip_prefix(root).ok()?;
    let name = rel
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let title = title_of(&text, name.rsplit('/').next().unwrap_or(&name));
    let updated = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(NoteMeta {
        name,
        title,
        bytes: md.len(),
        updated,
    })
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<NoteMeta>, depth: usize) {
    if depth > 12 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        // file_type does not follow symlinks: symlinked dirs are skipped,
        // so a vault with link loops can't hang the walk
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(root, &p, out, depth + 1),
            Ok(t) if t.is_file() && p.extension().and_then(|s| s.to_str()) == Some("md") => {
                if let Some(m) = meta(root, &p) {
                    out.push(m);
                }
            }
            _ => {}
        }
    }
}

/// All notes, newest first.
pub fn list(root: &Path) -> Vec<NoteMeta> {
    let mut out = vec![];
    walk(root, root, &mut out, 0);
    out.sort_by(|a, b| (b.updated, &b.name).cmp(&(a.updated, &a.name)));
    out
}

pub fn read(root: &Path, name: &str) -> Result<String, String> {
    let p = path_for(root, name)?;
    std::fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()))
}

/// Create or overwrite a note. Atomic (tmp file + rename) like the MEMORY.md
/// writer, so a crash mid-save can't leave a truncated note.
pub fn write(root: &Path, name: &str, content: &str) -> Result<u64, String> {
    let p = path_for(root, name)?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    let tmp = p.with_extension("md.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename to {}: {e}", p.display()))?;
    Ok(content.len() as u64)
}

pub fn delete(root: &Path, name: &str) -> Result<(), String> {
    let p = path_for(root, name)?;
    if !p.exists() {
        return Err(format!("no such note '{name}'"));
    }
    std::fs::remove_file(&p).map_err(|e| format!("delete {}: {e}", p.display()))?;
    // drop directories emptied by the delete, up to (not incl.) the root
    let mut d = p.parent();
    while let Some(dir) = d {
        if dir == root || std::fs::remove_dir(dir).is_err() {
            break;
        }
        d = dir.parent();
    }
    Ok(())
}

/// Case-insensitive substring search over note names and contents.
pub fn search(root: &Path, query: &str, limit: usize) -> Vec<NoteHit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() || limit == 0 {
        return vec![];
    }
    let mut hits = vec![];
    for m in list(root) {
        if hits.len() >= limit {
            break;
        }
        let p = match path_for(root, &m.name) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        let haystack = format!("{}\n{}", m.name, text);
        if let Some(sn) = crate::session::snippet_around(&haystack, &needle) {
            hits.push(NoteHit {
                name: m.name,
                title: m.title,
                updated: m.updated,
                snippet: sn,
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let d = std::env::temp_dir().join(format!("amty-notes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn path_for_rejects_traversal_and_bad_chars() {
        let root = Path::new("/root");
        assert!(path_for(root, "a/b").is_ok());
        assert!(path_for(root, "a/b.md").is_ok());
        for bad in ["", "..", "a/../b", "/abs", "a//b", ".hidden", "a/bad:name", "a/.", "x".repeat(300).as_str()] {
            assert!(path_for(root, bad).is_err(), "{bad} should be rejected");
        }
        let p = path_for(root, "proj/decisions").unwrap();
        assert_eq!(p, Path::new("/root/proj/decisions.md"));
    }

    #[test]
    fn write_read_list_search_delete_roundtrip() {
        let root = tmp_root();
        write(&root, "proj/amty", "# amty\n\nUses egui, no webview.\n").unwrap();
        write(&root, "misc", "plain note about cargo\n").unwrap();

        assert_eq!(read(&root, "proj/amty").unwrap(), "# amty\n\nUses egui, no webview.\n");

        let metas = list(&root);
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().any(|m| m.name == "proj/amty" && m.title == "amty"));

        let hits = search(&root, "webview", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "proj/amty");
        // name itself is searchable
        assert_eq!(search(&root, "misc", 10).len(), 1);

        delete(&root, "proj/amty").unwrap();
        assert!(read(&root, "proj/amty").is_err());
        // emptied parent dir went away too
        assert!(!root.join("proj").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
