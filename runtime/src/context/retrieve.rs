//! Ranks indexed files for a request and selects small, deduplicated code ranges within a token budget.

use super::index::{FileEntry, Index};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const DEFAULT_BUDGET_TOKENS: usize = 8_000;
const CHARS_PER_TOKEN: usize = 4;
const WINDOW_LINES: usize = 12;
const MAX_SNIPPET_LINES: u32 = 80;
const MAX_CANDIDATE_FILES: usize = 40;
const MAX_RELATED: usize = 6;

const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "this",
    "that",
    "from",
    "into",
    "what",
    "where",
    "when",
    "how",
    "why",
    "does",
    "should",
    "can",
    "could",
    "would",
    "please",
    "add",
    "fix",
    "make",
    "use",
    "using",
    "code",
    "file",
    "files",
    "function",
    "method",
    "class",
    "all",
    "are",
    "not",
    "but",
    "you",
    "our",
    "its",
    "it's",
    "has",
    "have",
    "get",
    "set",
    "new",
    "out",
    "let",
    "var",
    "via",
    "then",
    "than",
    "also",
    "some",
    "any",
    "need",
    "want",
    "work",
    "works",
    "change",
    "update",
    "implement",
    "create",
    "test",
    "tests",
];

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    pub score: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Retrieval {
    pub terms: Vec<String>,
    pub snippets: Vec<Snippet>,
    pub files_considered: usize,
    pub budget_tokens: usize,
    pub used_tokens: usize,
}

/// Lowercased query terms, including camelCase / snake_case parts of identifiers.
pub fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |t: String| {
        if t.len() >= 3 && !STOPWORDS.contains(&t.as_str()) && !out.contains(&t) {
            out.push(t);
        }
    };
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.is_empty() {
            continue;
        }
        add(word.to_lowercase());
        let mut cur = String::new();
        let mut prev_lower = false;
        for c in word.chars() {
            if (c == '_' || (c.is_uppercase() && prev_lower)) && !cur.is_empty() {
                add(std::mem::take(&mut cur).to_lowercase());
            }
            if c != '_' {
                cur.push(c);
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
        }
        if !cur.is_empty() && cur.len() < word.len() {
            add(cur.to_lowercase());
        }
    }
    out
}

struct Candidate<'a> {
    file: &'a FileEntry,
    score: f32,
    ranges: Vec<(u32, u32, f32, String)>,
}

fn line_slice(content: &str, start: u32, end: u32) -> String {
    content.lines().skip(start as usize - 1).take((end - start + 1) as usize).collect::<Vec<_>>().join("\n")
}

pub struct Options<'a> {
    pub budget_tokens: usize,
    pub focus_paths: &'a [String],
}

pub fn retrieve(index: &Index, query: &str, opts: &Options) -> Retrieval {
    let terms = terms(query);
    let focus: HashSet<&str> = opts.focus_paths.iter().map(String::as_str).collect();
    let texts: Vec<(&FileEntry, String)> = index.files().filter_map(|f| f.content.as_deref().map(|c| (f, c.to_lowercase()))).collect();
    let n = texts.len().max(1) as f32;
    let idf: HashMap<&str, f32> = terms
        .iter()
        .map(|t| {
            let df = texts.iter().filter(|(_, c)| c.contains(t.as_str())).count() as f32;
            (t.as_str(), (n / (df + 1.0)).ln().max(0.0) + 1.0)
        })
        .collect();

    let mut cands: Vec<Candidate> = Vec::new();
    for (f, lower) in &texts {
        let mut score = 0.0;
        let mut ranges = Vec::new();
        let path_lower = f.rel.to_lowercase();
        for t in &terms {
            if path_lower.split(['/', '.', '-', '_']).any(|seg| seg == t) {
                score += 4.0 * idf[t.as_str()];
            } else if path_lower.contains(t.as_str()) {
                score += 1.5 * idf[t.as_str()];
            }
        }
        for s in &f.symbols.definitions {
            let name = s.name.to_lowercase();
            let exact = terms.contains(&name);
            let partial = !exact && name.len() >= 4 && terms.iter().any(|t| name.contains(t.as_str()));
            if exact || partial {
                let w = if exact { 10.0 } else { 3.0 };
                score += w;
                let end = s.end_line.min(s.start_line + MAX_SNIPPET_LINES - 1);
                ranges.push((s.start_line, end, w, format!("defines {} `{}`", s.kind, s.name)));
            }
        }
        let mut best_line = None;
        let mut best_hits = 0.0;
        for t in &terms {
            let c = lower.matches(t.as_str()).count();
            if c > 0 {
                score += (1.0 + c as f32).ln() * idf[t.as_str()];
            }
        }
        if ranges.is_empty() && score > 0.0 {
            for (i, line) in lower.lines().enumerate() {
                let hits: f32 = terms.iter().filter(|t| line.contains(t.as_str())).map(|t| idf[t.as_str()]).sum();
                if hits > best_hits {
                    best_hits = hits;
                    best_line = Some(i as u32 + 1);
                }
            }
        }
        if let Some(l) = best_line {
            let start = l.saturating_sub(WINDOW_LINES as u32).max(1);
            ranges.push((start, l + WINDOW_LINES as u32, best_hits, "matches query terms".into()));
        }
        if focus.contains(f.rel.as_str()) {
            score += 8.0;
            if ranges.is_empty() {
                ranges.push((1, MAX_SNIPPET_LINES, 1.0, "open in editor".into()));
            }
        }
        if score > 0.0 && !ranges.is_empty() {
            cands.push(Candidate { file: f, score, ranges });
        }
    }
    cands.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.file.rel.cmp(&b.file.rel)));
    cands.truncate(MAX_CANDIDATE_FILES);
    let files_considered = cands.len();

    // Ranges per file: (start, end, score, reason). Related definitions are added for top files.
    let mut picked: BTreeMap<String, Vec<(u32, u32, f32, String)>> = BTreeMap::new();
    let mut seen_defs: HashSet<(String, u32)> = HashSet::new();
    let mut related = 0;
    for (rank, c) in cands.iter().enumerate() {
        let base = c.score / (rank as f32 + 1.0);
        let entry = picked.entry(c.file.rel.clone()).or_default();
        for (s, e, w, why) in &c.ranges {
            entry.push((*s, *e, base + w, why.clone()));
            seen_defs.insert((c.file.rel.clone(), *s));
        }
        if rank >= 5 || related >= MAX_RELATED {
            continue;
        }
        for name in &c.file.symbols.references {
            if related >= MAX_RELATED {
                break;
            }
            let defs = index.definitions(name);
            if defs.len() != 1 {
                continue; // Skip unknown and ambiguous names.
            }
            let (path, s) = &defs[0];
            if !seen_defs.insert((path.clone(), s.start_line)) {
                continue;
            }
            related += 1;
            let end = s.end_line.min(s.start_line + MAX_SNIPPET_LINES - 1);
            picked.entry(path.clone()).or_default().push((
                s.start_line,
                end,
                base * 0.4,
                format!("definition of `{name}` used by {}", c.file.rel),
            ));
        }
    }

    let mut snippets: Vec<Snippet> = Vec::new();
    for (path, mut ranges) in picked {
        let Some(content) = index.get(&path).and_then(|f| f.content.as_deref()) else { continue };
        let total = content.lines().count() as u32;
        ranges.sort_by_key(|r| r.0);
        let mut merged: Vec<(u32, u32, f32, String)> = Vec::new();
        for (s, e, sc, why) in ranges {
            let e = e.min(total.max(1));
            match merged.last_mut() {
                Some(last) if s <= last.1 + 1 => {
                    last.1 = last.1.max(e);
                    last.2 = last.2.max(sc);
                    if !last.3.contains(&why) {
                        last.3 = format!("{}; {why}", last.3);
                    }
                }
                _ => merged.push((s, e, sc, why)),
            }
        }
        for (s, e, sc, why) in merged {
            if s <= e && s <= total {
                snippets.push(Snippet {
                    text: line_slice(content, s, e),
                    path: path.clone(),
                    start_line: s,
                    end_line: e,
                    score: sc,
                    reason: why,
                });
            }
        }
    }
    snippets.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| (&a.path, a.start_line).cmp(&(&b.path, b.start_line))));
    let budget_chars = opts.budget_tokens * CHARS_PER_TOKEN;
    let mut used = 0;
    snippets.retain(|s| {
        let cost = s.text.len() + s.path.len() + 16;
        if used + cost > budget_chars {
            return false;
        }
        used += cost;
        true
    });
    Retrieval { terms, snippets, files_considered, budget_tokens: opts.budget_tokens, used_tokens: used.div_ceil(CHARS_PER_TOKEN) }
}

#[cfg(test)]
mod tests {
    use super::super::index::tests::temp_repo;
    use super::*;

    #[test]
    fn splits_identifiers_and_drops_stopwords() {
        assert_eq!(terms("Fix the parseConfig in load_settings"), ["parseconfig", "parse", "config", "load_settings", "load", "settings"]);
    }

    #[test]
    fn ranks_symbol_definitions_and_adds_related_definitions_within_budget() {
        let root = temp_repo(&[
            (
                "src/config.rs",
                "use crate::util::normalize_path;\n\npub fn parse_config(s: &str) -> Config {\n    normalize_path(s);\n    Config\n}\n",
            ),
            ("src/util.rs", "pub fn normalize_path(s: &str) -> String {\n    s.to_string()\n}\n"),
            ("src/other.rs", "pub fn unrelated() {}\n"),
            ("README.md", "nothing here\n"),
        ]);
        let mut idx = Index::new(root.clone(), &[]).unwrap();
        idx.refresh();
        let r = retrieve(&idx, "why does parse_config fail?", &Options { budget_tokens: 1000, focus_paths: &[] });
        assert_eq!(r.snippets[0].path, "src/config.rs");
        assert_eq!((r.snippets[0].start_line, r.snippets[0].end_line), (3, 6));
        assert!(r.snippets[0].reason.contains("parse_config"));
        let rel = r.snippets.iter().find(|s| s.path == "src/util.rs").expect("related definition");
        assert!(rel.reason.contains("normalize_path"));
        assert!(!r.snippets.iter().any(|s| s.path == "src/other.rs"));
        let tiny = retrieve(&idx, "parse_config", &Options { budget_tokens: 10, focus_paths: &[] });
        assert!(tiny.used_tokens <= 10);
        std::fs::remove_dir_all(root).ok();
    }
}
