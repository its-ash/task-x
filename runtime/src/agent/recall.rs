//! Cross-session memory: a searchable log of what earlier tasks in a workspace did. Each finished
//! task adds an entry (its request, its answer, and the files it changed) to
//! `<dataDir>/memory/<workspace hash>.jsonl`; the notes in `.task-x/memory.md` are searched too.
//! `recall` ranks entries with BM25. The log keeps the newest `MAX_ENTRIES`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 1_000;
const MAX_TEXT_CHARS: usize = 2_000;
const MAX_FILES: usize = 40;
const K1: f64 = 1.2;
const B: f64 = 0.75;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    /// `task` (a finished task) or `note` (a line of `.task-x/memory.md`).
    pub kind: String,
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub files: Vec<String>,
    pub created_ms: u64,
}

/// The log file of a workspace.
pub fn file(data_dir: &Path, root: &Path) -> PathBuf {
    data_dir.join("memory").join(format!("{:016x}.jsonl", crate::tools::fingerprint(&root.to_string_lossy())))
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Adds one finished task. Rewrites the file without the oldest entries when it grows past the limit.
pub fn add(file: &Path, task_id: &str, title: &str, text: &str, files: &[String]) -> std::io::Result<()> {
    if text.trim().is_empty() && files.is_empty() {
        return Ok(());
    }
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    let e = Entry {
        id: task_id.to_owned(),
        kind: "task".into(),
        title: clip(title, 200),
        text: clip(text.trim(), MAX_TEXT_CHARS),
        files: files.iter().take(MAX_FILES).cloned().collect(),
        created_ms: super::store::now_ms(),
    };
    let mut all = load(file);
    all.retain(|o| o.id != e.id);
    all.push(e);
    if all.len() > MAX_ENTRIES {
        all.drain(..all.len() - MAX_ENTRIES);
    }
    let tmp = file.with_extension("jsonl.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    for e in &all {
        writeln!(f, "{}", serde_json::to_string(e).map_err(std::io::Error::other)?)?;
    }
    drop(f);
    std::fs::rename(tmp, file)
}

pub fn load(file: &Path) -> Vec<Entry> {
    std::fs::read_to_string(file).map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()).unwrap_or_default()
}

/// Removes one entry by id. Returns whether it was there.
pub fn forget(file: &Path, id: &str) -> std::io::Result<bool> {
    let mut all = load(file);
    let before = all.len();
    all.retain(|e| e.id != id);
    if all.len() == before {
        return Ok(false);
    }
    let body: String = all.iter().filter_map(|e| serde_json::to_string(e).ok()).map(|l| l + "\n").collect();
    std::fs::write(file, body)?;
    Ok(true)
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| w.len() > 1).map(str::to_lowercase).collect()
}

/// The entries that match `query` best (BM25 over title, text, and file names), newest first on ties.
pub fn search<'a>(entries: &'a [Entry], query: &str, limit: usize) -> Vec<(f64, &'a Entry)> {
    let q: Vec<String> = words(query);
    if q.is_empty() || entries.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = entries.iter().map(|e| words(&format!("{} {} {}", e.title, e.text, e.files.join(" ")))).collect();
    let avg = docs.iter().map(Vec::len).sum::<usize>() as f64 / docs.len() as f64;
    let n = docs.len() as f64;
    let mut df: HashMap<&str, f64> = HashMap::new();
    for d in &docs {
        let mut seen: Vec<&str> = d.iter().map(String::as_str).collect();
        seen.sort_unstable();
        seen.dedup();
        for w in seen {
            *df.entry(w).or_default() += 1.0;
        }
    }
    let mut scored: Vec<(f64, &Entry)> = docs
        .iter()
        .zip(entries)
        .filter_map(|(d, e)| {
            let len = d.len() as f64;
            let s: f64 = q
                .iter()
                .map(|w| {
                    let tf = d.iter().filter(|x| *x == w).count() as f64;
                    if tf == 0.0 {
                        return 0.0;
                    }
                    let df = df.get(w.as_str()).copied().unwrap_or(0.0);
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg.max(1.0)))
                })
                .sum();
            (s > 0.0).then_some((s, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.created_ms.cmp(&a.1.created_ms)));
    scored.truncate(limit);
    scored
}

/// The notes of `.task-x/memory.md` as entries, so `recall` finds them too.
pub fn notes(root: &Path) -> Vec<Entry> {
    crate::tools::memory::load(root)
        .map(|m| {
            m.lines()
                .map(|l| l.trim().trim_start_matches("- ").trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .enumerate()
                .map(|(i, l)| Entry { id: format!("note{}", i + 1), kind: "note".into(), title: l.into(), text: String::new(), files: Vec::new(), created_ms: 0 })
                .collect()
        })
        .unwrap_or_default()
}

/// The `recall` tool's answer.
pub fn recall(data_dir: Option<&Path>, root: &Path, query: &str, limit: usize) -> Value {
    let mut all = data_dir.map(|d| load(&file(d, root))).unwrap_or_default();
    all.extend(notes(root));
    let hits: Vec<Value> = search(&all, query, limit)
        .into_iter()
        .map(|(score, e)| json!({ "id": e.id, "kind": e.kind, "title": e.title, "text": e.text, "files": e.files, "createdMs": e.created_ms, "score": (score * 100.0).round() / 100.0 }))
        .collect();
    json!({ "query": query, "entries": all.len(), "results": hits, "note": if hits.is_empty() { "Nothing in the memory of earlier tasks matches; search the code instead." } else { "Earlier work can be out of date: check the code before you rely on it." } })
}

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "recall".into(),
        description: "Search the memory of earlier tasks in this project (their requests, answers, and changed files) and the project notes. Use it before exploring when the request builds on earlier work (\"like last time\", \"the bug we fixed\"). Returns the best matches.".into(),
        input_schema: json!({ "type": "object", "properties": {
            "query": { "type": "string" },
            "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
        }, "required": ["query"], "additionalProperties": false }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_tasks_and_finds_them_by_words_and_files() {
        let dir = std::env::temp_dir().join(format!("task-x-recall-{}", std::process::id()));
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join(".task-x")).unwrap();
        std::fs::write(root.join(".task-x/memory.md"), "# Project memory\n\n- Tests: cargo test in runtime\n").unwrap();
        let f = file(&dir, &root);
        add(&f, "t1", "Fix the login redirect", "The redirect loop came from the session cookie path.", &["src/auth/session.rs".into()]).unwrap();
        add(&f, "t2", "Add dark mode", "Added a theme toggle.", &["web/theme.css".into()]).unwrap();
        add(&f, "t3", "", "", &[]).unwrap();
        assert_eq!(load(&f).len(), 2, "an empty task is not kept");
        let r = recall(Some(&dir), &root, "session cookie bug", 5);
        assert_eq!(r["results"][0]["id"], "t1");
        assert_eq!(recall(Some(&dir), &root, "theme.css", 5)["results"][0]["id"], "t2", "file names are searched");
        assert_eq!(recall(Some(&dir), &root, "cargo test", 5)["results"][0]["kind"], "note", "project notes are searched");
        assert!(recall(Some(&dir), &root, "kubernetes", 5)["results"].as_array().unwrap().is_empty());
        add(&f, "t1", "Fix the login redirect again", "Second fix.", &[]).unwrap();
        assert_eq!(load(&f).iter().filter(|e| e.id == "t1").count(), 1, "a continued task replaces its entry");
        assert!(forget(&f, "t2").unwrap() && !forget(&f, "t2").unwrap());
        std::fs::remove_dir_all(dir).ok();
    }
}
