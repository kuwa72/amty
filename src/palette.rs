//! Ctrl/Cmd+K palette: one filter box over sessions, CLI subcommands,
//! skills and notes.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Session,
    Note,
    Skill,
    Command,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub kind: Kind,
    /// Session id / note name / skill path / subcommand.
    pub target: String,
}

/// Indices of `items` that match `query`, best first. Prefix beats
/// word-start beats substring; case-insensitive; an empty query keeps the
/// input order (the caller feeds it newest-first).
pub fn rank(query: &str, items: &[Item]) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..items.len()).collect();
    }
    let mut scored: Vec<(u8, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let label = it.label.to_lowercase();
            let score = if label.starts_with(&q) {
                3
            } else if label.split([' ', '/', '-', '_']).any(|w| w.starts_with(&q)) {
                2
            } else if label.contains(&q) {
                1
            } else {
                return None;
            };
            Some((score, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Read-only `amty` subcommands worth running from the palette (`search`
/// needs an argument, so it is not one of them).
pub const COMMANDS: &[&str] = &["status", "sessions", "usage", "te", "evolve", "notes", "catalog"];

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str) -> Item {
        Item { label: label.into(), kind: Kind::Session, target: label.into() }
    }

    #[test]
    fn prefix_beats_substring_and_case_is_ignored() {
        let items = vec![item("refactor provider"), item("Provider docs"), item("other")];
        assert_eq!(rank("prov", &items), vec![1, 0]);
    }

    #[test]
    fn word_start_beats_plain_substring() {
        let items = vec![item("xprov"), item("x prov")];
        assert_eq!(rank("prov", &items), vec![1, 0]);
    }

    #[test]
    fn empty_query_lists_everything_in_order() {
        let items = vec![item("a"), item("b")];
        assert_eq!(rank("  ", &items), vec![0, 1]);
    }

    #[test]
    fn no_match_is_empty() {
        assert!(rank("zzz", &[item("abc")]).is_empty());
    }
}
