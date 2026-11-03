//! Line diff for approval prompts: enough to show what an `fs_write` /
//! `fs_edit` is about to change, not a general-purpose differ.

/// Output caps: a prompt can't carry a whole file rewrite.
const MAX_BYTES: usize = 8192;
const MAX_LINES: usize = 200;
/// Context lines kept around the changed span.
const CTX: usize = 3;

/// Unified diff of `old` (`None` = the file does not exist) against `new`.
/// One hunk covering everything between the common prefix and suffix: a valid
/// diff, but not a minimal one — unchanged lines between two scattered edits
/// show up as removed and re-added. `shortcut: single hunk, upgrade to an LCS
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
    let removed = &old[pre..old.len() - suf];
    let added = &new[pre..new.len() - suf];
    let tail_ctx = new_end - (new.len() - suf);
    let room = MAX_LINES.saturating_sub(out.len() + tail_ctx);
    if removed.len() + added.len() <= room {
        for l in removed {
            out.push(format!("-{l}"));
        }
        for l in added {
            out.push(format!("+{l}"));
        }
    } else {
        // split the budget: a rewrite must not show only its removals
        // (2 lines go to the two count markers)
        let each = room.saturating_sub(2) / 2;
        for l in &removed[..each.min(removed.len())] {
            out.push(format!("-{l}"));
        }
        if removed.len() > each {
            out.push(format!("… ({} more lines removed)", removed.len() - each));
        }
        for l in &added[..each.min(added.len())] {
            out.push(format!("+{l}"));
        }
        if added.len() > each {
            out.push(format!("… ({} more lines added)", added.len() - each));
        }
    }
    for l in &new[new.len() - suf..new_end] {
        out.push(format!(" {l}"));
    }

    const MARKER: &str = "… (truncated)";
    let mut text = String::new();
    for (i, line) in out.iter().enumerate() {
        if i == MAX_LINES || text.len() + line.len() + 1 + MARKER.len() > MAX_BYTES {
            text.push_str(MARKER);
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
        // exactly 3 lines of context on both sides
        assert_eq!(
            unified("/x", Some(old), new),
            "--- /x\n+++ /x\n@@ -2,7 +2,7 @@\n b\n c\n d\n-e\n+E\n f\n g\n h\n"
        );
    }

    #[test]
    fn identical_content_yields_empty_diff() {
        assert!(unified("/x", Some("same\n"), "same\n").is_empty());
    }

    #[test]
    fn huge_change_shows_both_sides() {
        let old: String = (0..2000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..2000).map(|i| format!("new {i}\n")).collect();
        let d = unified("/x", Some(&old), &new);
        // a capped rewrite must not look like a deletion-only diff
        assert!(d.contains("-old 0"), "{d}");
        assert!(d.contains("+new 0"), "{d}");
        assert!(d.contains("more lines removed") && d.contains("more lines added"), "{d}");
        assert!(d.lines().count() <= 202, "{} lines", d.lines().count());
    }

    #[test]
    fn a_single_huge_line_is_cut_by_bytes() {
        let d = unified("/x", None, &"y".repeat(20_000));
        assert!(d.starts_with("--- /x\n+++ /x\n"), "headers kept");
        assert!(d.len() <= 8192, "{} bytes", d.len());
        assert!(d.ends_with("… (truncated)"), "{}", d.len());
    }
}
