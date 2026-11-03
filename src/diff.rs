//! Line diff for approval prompts: enough to show what an `fs_write` /
//! `fs_edit` is about to change, not a general-purpose differ.

/// Output caps: a prompt can't carry a whole file rewrite.
const MAX_BYTES: usize = 8192;
const MAX_LINES: usize = 200;
/// Context lines kept around the changed span.
const CTX: usize = 3;

/// Unified diff of `old` (`None` = the file does not exist) against `new`.
/// One hunk: everything between the common prefix and suffix. That is a
/// truthful diff (never a superset of the change), just not the minimal one
/// for several scattered edits — `shortcut: single hunk, upgrade to an LCS
/// diff if scattered edits start to matter`.
pub fn unified(path: &str, old: Option<&str>, new: &str) -> String {
    let old: Vec<&str> = old.map(|s| s.lines().collect()).unwrap_or_default();
    let new: Vec<&str> = new.lines().collect();
    let pre = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    if pre == old.len() && pre == new.len() {
        return String::new();
    }
    let suf = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(old.len() - pre)
        .min(new.len() - pre);

    let start = pre.saturating_sub(CTX);
    let old_end = (old.len() - suf + CTX).min(old.len());
    let new_end = (new.len() - suf + CTX).min(new.len());
    // `-0,0` / `+0,0` on the side that has no lines, matching diff(1)
    let old_start = if old.is_empty() { 0 } else { start + 1 };
    let new_start = if new.is_empty() { 0 } else { start + 1 };

    let mut out = vec![
        format!("--- {path}"),
        format!("+++ {path}"),
        format!(
            "@@ -{old_start},{} +{new_start},{} @@",
            old_end - start,
            new_end - start
        ),
    ];
    for l in &old[start..pre] {
        out.push(format!(" {l}"));
    }
    for l in &old[pre..old.len() - suf] {
        out.push(format!("-{l}"));
    }
    for l in &new[pre..new.len() - suf] {
        out.push(format!("+{l}"));
    }
    for l in &new[new.len() - suf..new_end] {
        out.push(format!(" {l}"));
    }

    let mut text = String::new();
    for (i, line) in out.iter().enumerate() {
        if i == MAX_LINES || text.len() + line.len() + 1 > MAX_BYTES {
            text.push_str("\n… (truncated)");
            break;
        }
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::unified;

    #[test]
    fn new_file_is_all_added() {
        let d = unified("/tmp/a.txt", None, "one\ntwo\n");
        assert!(d.starts_with("--- /tmp/a.txt\n+++ /tmp/a.txt\n"), "{d}");
        assert!(d.contains("@@ -0,0 +1,2 @@"), "{d}");
        assert!(d.contains("+one"), "{d}");
        assert!(d.contains("+two"), "{d}");
    }

    #[test]
    fn overwrite_shows_removed_and_added_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let new = "a\nb\nc\nd\nE\nf\ng\nh\ni\n";
        let d = unified("/x", Some(old), new);
        assert!(d.contains("-e"), "{d}");
        assert!(d.contains("+E"), "{d}");
        // 3 lines of context around the change, not the whole file
        assert!(d.contains(" c"), "{d}");
        assert!(!d.contains(" a"), "{d}");
    }

    #[test]
    fn identical_content_yields_empty_diff() {
        assert!(unified("/x", Some("same\n"), "same\n").is_empty());
    }

    #[test]
    fn huge_change_is_truncated() {
        let old: String = (0..2000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..2000).map(|i| format!("new {i}\n")).collect();
        let d = unified("/x", Some(&old), &new);
        assert!(d.len() < 9000, "{} bytes", d.len());
        assert!(d.contains("truncated"), "{d}");
    }
}
