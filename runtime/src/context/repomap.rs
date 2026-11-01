//! Repository map: the files the most of the project depends on (PageRank over the project graph,
//! leaning towards the files of the request), each with its main definitions and their lines, cut
//! to a token budget. The agent gets one at the start of a task, and the `repo_map` tool on demand.

use super::Workspace;

const CHARS_PER_TOKEN: usize = 4;

/// Definitions listed per file.
const MAX_SYMBOLS: usize = 8;
const MIN_BUDGET: usize = 100;
pub const MAX_BUDGET: usize = 8_000;

/// The map as text, at most `budget_tokens`; `focus` are root-relative paths to lean towards.
pub fn render(w: &Workspace, focus: &[String], budget_tokens: usize) -> String {
    let budget = budget_tokens.clamp(MIN_BUDGET, MAX_BUDGET) * CHARS_PER_TOKEN;
    let g = w.graph();
    let focus_ids: Vec<usize> = focus.iter().filter_map(|p| g.id(p)).collect();
    let rank = g.page_rank(&focus_ids);
    let mut order: Vec<usize> = (0..g.nodes.len()).filter(|&v| g.nodes[v].symbols > 0).collect();
    order.sort_by(|a, b| rank[*b].total_cmp(&rank[*a]).then_with(|| g.nodes[*a].path.cmp(&g.nodes[*b].path)));
    let mut out = String::new();
    let mut listed = 0;
    for v in order {
        let path = &g.nodes[v].path;
        let Some(f) = w.index.get(path) else { continue };
        // Top-level definitions first (types and functions), methods after.
        let mut defs: Vec<&super::symbols::Symbol> = f.symbols.definitions.iter().collect();
        defs.sort_by_key(|s| (s.kind == "method", s.start_line));
        let names: Vec<String> = defs.iter().take(MAX_SYMBOLS).map(|s| format!("{} {}:{}", s.kind, s.name, s.start_line)).collect();
        let more = defs.len().saturating_sub(MAX_SYMBOLS);
        let line = if more > 0 { format!("{path}: {} (+{more} more)\n", names.join(", ")) } else { format!("{path}: {}\n", names.join(", ")) };
        if out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
        listed += 1;
    }
    if listed == 0 {
        return String::new();
    }
    format!(
        "<repo_map>\nThe {listed} files the most of the project depends on{}, with their main definitions (kind name:line). Use it to choose what to read; read the code before you change it.\n{out}</repo_map>",
        if focus_ids.is_empty() { "" } else { " (leaning towards the files of this request)" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::index::tests::temp_repo;

    #[test]
    fn lists_the_most_used_files_first_within_the_budget() {
        let root = temp_repo(&[
            ("src/a.rs", "pub fn alpha() { core(); }\n"),
            ("src/b.rs", "pub fn beta() { core(); }\n"),
            ("src/core.rs", "pub fn core() {}\npub struct Engine;\n"),
            ("README.md", "# hi\n"),
        ]);
        let w = Workspace::open(&root, &[]).unwrap();
        let map = render(&w, &[], 1_000);
        assert!(map.starts_with("<repo_map>") && map.ends_with("</repo_map>"), "{map}");
        let first = map.lines().nth(2).unwrap();
        assert!(first.starts_with("src/core.rs: function core:1") || first.starts_with("src/core.rs:"), "{map}");
        assert!(!map.contains("README.md"), "files without definitions are left out");
        let small = render(&w, &["src/b.rs".into()], MIN_BUDGET);
        assert!(small.len() <= MIN_BUDGET * CHARS_PER_TOKEN + 300, "the budget holds: {}", small.len());
        assert!(small.contains("leaning towards"));
        std::fs::remove_dir_all(root).ok();
    }
}
