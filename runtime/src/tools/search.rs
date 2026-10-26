//! Search tools over the in-memory index (fast, and they respect index exclusions).

use super::fs::read_text;
use super::schema::{int, object, path};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use crate::context::index::globset;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;
const MAX_EXCERPT: usize = 300;
const MAX_PATTERN: usize = 1_000;

fn search_schema(field: &str, desc: &str) -> Value {
    object(
        json!({
            field: { "type": "string", "description": desc },
            "caseSensitive": { "type": "boolean" },
            "glob": { "type": "string", "description": "Only search paths matching this glob" },
            "limit": int("Max matches (default 100)", 1),
            "offset": int("Skip this many matches (pagination)", 0),
        }),
        &[field],
    )
}

pub fn tools() -> Vec<ToolDef> {
    use Permission::Read;
    vec![
        tool(
            "search_text",
            "Search indexed files for a literal string. Returns path, line, and excerpt.",
            Read,
            || search_schema("query", "Literal text"),
            search_text,
        ),
        tool(
            "search_regex",
            "Search indexed files with a Rust-syntax regular expression.",
            Read,
            || search_schema("pattern", "Regular expression"),
            search_regex,
        ),
        tool(
            "find_all",
            "Find every occurrence of a literal string in one file, with line and column.",
            Read,
            || {
                object(
                    json!({ "path": path("File path"), "query": { "type": "string" }, "caseSensitive": { "type": "boolean" } }),
                    &["path", "query"],
                )
            },
            find_all,
        ),
        tool(
            "find_symbol",
            "Find symbol definitions whose name contains the text (case-insensitive).",
            Read,
            || {
                object(
                    json!({ "name": { "type": "string" }, "kind": { "type": "string", "description": "e.g. function, class, method" }, "limit": int("Max results (default 100)", 1) }),
                    &["name"],
                )
            },
            find_symbol,
        ),
        tool(
            "find_definition",
            "Find the exact definition(s) of a symbol with a short excerpt.",
            Read,
            || object(json!({ "name": { "type": "string" } }), &["name"]),
            find_definition,
        ),
        tool(
            "find_references",
            "Find whole-word references to a name across indexed files.",
            Read,
            || {
                object(
                    json!({ "name": { "type": "string" }, "limit": int("Max results (default 100)", 1), "offset": int("Pagination offset", 0) }),
                    &["name"],
                )
            },
            find_references,
        ),
        tool(
            "search_files_by_content",
            "List files containing a literal string, with match counts.",
            Read,
            || {
                object(
                    json!({ "query": { "type": "string" }, "glob": { "type": "string" }, "caseSensitive": { "type": "boolean" }, "limit": int("Max files (default 100)", 1) }),
                    &["query"],
                )
            },
            search_files_by_content,
        ),
        tool(
            "get_dependencies",
            "Project dependency graph from the index. With a path: the files it depends on and the files that use it (import or symbol reference), and any import cycle it is in. Without a path: the most used files (hubs), import cycles, and graph size. Use it to find what a change affects.",
            Read,
            || object(json!({ "path": path("File path (optional)") }), &[]),
            get_dependencies,
        ),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DepsIn {
    #[serde(default)]
    path: Option<String>,
}

fn get_dependencies(c: &ToolCtx, input: Value) -> ToolResult {
    let p: DepsIn = parse(input)?;
    let g = c.with_workspace(|w| w.graph())?;
    match p.path {
        Some(raw) => {
            let (_, rel) = c.resolve(&raw)?;
            g.neighbors(&rel).ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, format!("{rel} is not in the index")))
        }
        None => Ok(g.stats()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchIn {
    #[serde(alias = "pattern")]
    query: String,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

fn excerpt(line: &str) -> String {
    let t = line.trim();
    if t.len() <= MAX_EXCERPT {
        return t.to_owned();
    }
    let mut cut = MAX_EXCERPT;
    while !t.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &t[..cut])
}

fn build_regex(pattern: &str, literal: bool, case_sensitive: bool) -> Result<Regex, ToolError> {
    if pattern.is_empty() || pattern.len() > MAX_PATTERN {
        return Err(ToolError::invalid(format!("pattern must be 1..={MAX_PATTERN} characters")));
    }
    let p = if literal { regex::escape(pattern) } else { pattern.to_owned() };
    RegexBuilder::new(&p)
        .case_insensitive(!case_sensitive)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| ToolError::invalid(format!("invalid regex: {e}")))
}

fn grep(c: &ToolCtx, re: &Regex, glob: Option<&str>, limit: usize, offset: usize) -> ToolResult {
    let filter = glob.map(|g| globset([g]).map_err(ToolError::invalid)).transpose()?;
    c.with_workspace(|w| {
        let mut matches = Vec::new();
        let mut total = 0usize;
        for f in w.index.files() {
            if filter.as_ref().is_some_and(|g| !g.is_match(&f.rel)) {
                continue;
            }
            let Some(text) = f.content.as_deref() else { continue };
            if !re.is_match(text) {
                continue;
            }
            for (n, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    if total >= offset && matches.len() < limit {
                        matches.push(json!({ "path": f.rel, "line": n + 1, "excerpt": excerpt(line) }));
                    }
                    total += 1;
                }
            }
        }
        json!({ "matches": matches, "total": total, "offset": offset, "more": total > offset + matches.len() })
    })
}

fn run_search(c: &ToolCtx, input: Value, literal: bool) -> ToolResult {
    let i: SearchIn = parse(input)?;
    let re = build_regex(&i.query, literal, i.case_sensitive)?;
    grep(c, &re, i.glob.as_deref(), i.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), i.offset.unwrap_or(0))
}

fn search_text(c: &ToolCtx, input: Value) -> ToolResult {
    run_search(c, input, true)
}

fn search_regex(c: &ToolCtx, input: Value) -> ToolResult {
    run_search(c, input, false)
}

fn find_all(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct In {
        path: String,
        query: String,
        #[serde(default)]
        case_sensitive: bool,
    }
    let i: In = parse(input)?;
    let re = build_regex(&i.query, true, i.case_sensitive)?;
    let (rel, text) = read_text(c, &i.path)?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for m in re.find_iter(line) {
            out.push(json!({ "line": n + 1, "column": line[..m.start()].chars().count() + 1, "excerpt": excerpt(line) }));
            if out.len() >= MAX_LIMIT {
                break;
            }
        }
    }
    Ok(json!({ "path": rel, "count": out.len(), "matches": out }))
}

fn find_symbol(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        name: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    }
    let i: In = parse(input)?;
    let needle = i.name.to_lowercase();
    if needle.is_empty() {
        return Err(ToolError::invalid("name must not be empty"));
    }
    let limit = i.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    c.with_workspace(|w| {
        let mut hits: Vec<(u8, Value)> = Vec::new();
        for f in w.index.files() {
            for s in &f.symbols.definitions {
                let lname = s.name.to_lowercase();
                if !lname.contains(&needle) || i.kind.as_ref().is_some_and(|k| !s.kind.eq_ignore_ascii_case(k)) {
                    continue;
                }
                let rank = if s.name == i.name {
                    0
                } else if lname == needle {
                    1
                } else {
                    2
                };
                hits.push((
                    rank,
                    json!({ "name": s.name, "kind": s.kind, "path": f.rel, "startLine": s.start_line, "endLine": s.end_line }),
                ));
            }
        }
        hits.sort_by_key(|h| h.0);
        let total = hits.len();
        json!({ "symbols": hits.into_iter().take(limit).map(|h| h.1).collect::<Vec<_>>(), "total": total })
    })
}

fn find_definition(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        name: String,
    }
    let i: In = parse(input)?;
    let defs = c.with_workspace(|w| {
        w.index
            .definitions(&i.name)
            .iter()
            .map(|(p, s)| {
                let first =
                    w.index.get(p).and_then(|f| f.content.as_deref()).and_then(|t| t.lines().nth(s.start_line as usize - 1)).map(excerpt);
                json!({ "path": p, "kind": s.kind, "startLine": s.start_line, "endLine": s.end_line, "signature": first })
            })
            .collect::<Vec<_>>()
    })?;
    if defs.is_empty() {
        return Err(ToolError::new(ToolErrorKind::NotFound, format!("no definition named '{}'", i.name)));
    }
    Ok(json!({ "definitions": defs }))
}

fn find_references(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        name: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
    }
    let i: In = parse(input)?;
    if i.name.is_empty() || i.name.len() > 200 {
        return Err(ToolError::invalid("name must be 1..=200 characters"));
    }
    let re = Regex::new(&format!(r"\b{}\b", regex::escape(&i.name))).map_err(|e| ToolError::invalid(e.to_string()))?;
    let mut r = grep(c, &re, None, i.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), i.offset.unwrap_or(0))?;
    let defs: Vec<(String, u32)> =
        c.with_workspace(|w| w.index.definitions(&i.name).iter().map(|(p, s)| (p.clone(), s.start_line)).collect())?;
    for m in r["matches"].as_array_mut().into_iter().flatten() {
        let is_def = defs.iter().any(|(p, l)| m["path"] == p.as_str() && m["line"] == *l);
        m["isDefinition"] = json!(is_def);
    }
    Ok(r)
}

fn search_files_by_content(c: &ToolCtx, input: Value) -> ToolResult {
    let i: SearchIn = parse(input)?;
    let re = build_regex(&i.query, true, i.case_sensitive)?;
    let filter = i.glob.as_deref().map(|g| globset([g]).map_err(ToolError::invalid)).transpose()?;
    let limit = i.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    c.with_workspace(|w| {
        let mut files: Vec<(usize, &str)> = w
            .index
            .files()
            .filter(|f| filter.as_ref().is_none_or(|g| g.is_match(&f.rel)))
            .filter_map(|f| f.content.as_deref().map(|t| (re.find_iter(t).count(), f.rel.as_str())))
            .filter(|(n, _)| *n > 0)
            .collect();
        files.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let total = files.len();
        json!({ "files": files.iter().take(limit).map(|(n, p)| json!({ "path": p, "matches": n })).collect::<Vec<_>>(), "total": total })
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn text_regex_and_pagination() {
        let c = ctx(&[("a.rs", "let Foo = 1;\nfoo();\n"), ("b.ts", "foo bar foo\n"), (".env", "foo=secret\n")]);
        let r = run(&c, "search_text", json!({ "query": "foo" })).unwrap();
        assert_eq!(r["total"], 4);
        let r = run(&c, "search_text", json!({ "query": "foo", "caseSensitive": true, "glob": "*.rs" })).unwrap();
        assert_eq!(r["matches"], json!([{ "path": "a.rs", "line": 2, "excerpt": "foo();" }]));
        let r = run(&c, "search_regex", json!({ "pattern": "^let \\w+", "limit": 1, "offset": 0 })).unwrap();
        assert_eq!((r["total"].as_u64(), r["more"].as_bool()), (Some(1), Some(false)));
        let r = run(&c, "search_text", json!({ "query": "foo", "limit": 1, "offset": 1 })).unwrap();
        assert_eq!(r["matches"][0]["line"], 1);
        assert!(run(&c, "search_regex", json!({ "pattern": "(" })).is_err());
        assert_eq!(run(&c, "find_all", json!({ "path": "b.ts", "query": "foo" })).unwrap()["count"], 2);
        let f = run(&c, "search_files_by_content", json!({ "query": "foo", "caseSensitive": true })).unwrap();
        assert_eq!(f["files"][0], json!({ "path": "b.ts", "matches": 2 }));
    }

    #[test]
    fn symbols_definitions_and_references() {
        let c =
            ctx(&[("lib.rs", "pub fn parse_input() {}\nfn main() { parse_input(); }\n"), ("x.py", "def parse_input_py():\n    pass\n")]);
        let s = run(&c, "find_symbol", json!({ "name": "parse_input" })).unwrap();
        assert_eq!(s["symbols"][0]["name"], "parse_input");
        assert_eq!(s["total"], 2);
        let d = run(&c, "find_definition", json!({ "name": "parse_input" })).unwrap();
        assert_eq!(d["definitions"][0]["signature"], "pub fn parse_input() {}");
        let r = run(&c, "find_references", json!({ "name": "parse_input" })).unwrap();
        assert_eq!(r["total"], 2, "whole-word match must skip parse_input_py");
        assert_eq!(r["matches"][0]["isDefinition"], true);
        assert_eq!(r["matches"][1]["isDefinition"], false);
        assert_eq!(run(&c, "find_definition", json!({ "name": "nope" })).unwrap_err().kind, ToolErrorKind::NotFound);
    }
}
