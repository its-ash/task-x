//! File discovery and reading tools.

use super::schema::{int, object, path};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use crate::context::index::{MAX_FILE_BYTES, globset};
use crate::context::symbols;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

const DEFAULT_LIST_LIMIT: usize = 500;
const MAX_READ_LINES: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_MULTI_FILES: usize = 20;

pub fn tools() -> Vec<ToolDef> {
    use Permission::Read;
    vec![
        tool(
            "list_files",
            "List indexed files under a directory (respects .gitignore and exclusions).",
            Read,
            || {
                object(
                    json!({ "path": path("Directory relative to the workspace root; default '.'"), "recursive": { "type": "boolean" }, "limit": int("Max entries (default 500)", 1) }),
                    &[],
                )
            },
            list_files,
        ),
        tool(
            "find_files",
            "Find indexed files whose path matches a glob, e.g. 'src/**/*.ts'.",
            Read,
            || object(json!({ "glob": { "type": "string" }, "limit": int("Max results (default 500)", 1) }), &["glob"]),
            find_files,
        ),
        tool(
            "get_file_tree",
            "Show the directory tree to a given depth.",
            Read,
            || object(json!({ "path": path("Directory; default '.'"), "depth": int("Depth (default 3, max 8)", 1) }), &[]),
            get_file_tree,
        ),
        tool(
            "get_file_metadata",
            "Size, line count, language, and modification time of a file.",
            Read,
            || object(json!({ "path": path("File path") }), &["path"]),
            get_file_metadata,
        ),
        tool(
            "detect_language",
            "Detect the programming language of a file.",
            Read,
            || object(json!({ "path": path("File path") }), &["path"]),
            detect_language,
        ),
        tool(
            "get_project_structure",
            "Detected languages, manifests, frameworks, commands, and top-level layout.",
            Read,
            || object(json!({}), &[]),
            get_project_structure,
        ),
        tool(
            "read_file",
            "Read a text file with line numbers. Use offset/limit for large files.",
            Read,
            || {
                object(
                    json!({ "path": path("File path"), "offset": int("First line, 1-based (default 1)", 1), "limit": int("Max lines (default 2000)", 1) }),
                    &["path"],
                )
            },
            read_file,
        ),
        tool(
            "read_range",
            "Read an inclusive 1-based line range of a file.",
            Read,
            || {
                object(
                    json!({ "path": path("File path"), "start": int("First line", 1), "end": int("Last line", 1) }),
                    &["path", "start", "end"],
                )
            },
            read_range,
        ),
        tool(
            "read_multiple_files",
            "Read up to 20 files at once.",
            Read,
            || object(json!({ "paths": { "type": "array", "items": { "type": "string" }, "maxItems": MAX_MULTI_FILES } }), &["paths"]),
            read_multiple_files,
        ),
        tool(
            "read_symbol",
            "Read the source of a symbol definition by name (optionally in one file).",
            Read,
            || object(json!({ "name": { "type": "string" }, "path": path("Limit to this file") }), &["name"]),
            read_symbol,
        ),
        tool(
            "get_file_outline",
            "List the symbols defined in a file with line ranges.",
            Read,
            || object(json!({ "path": path("File path") }), &["path"]),
            get_file_outline,
        ),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListIn {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    recursive: Option<bool>,
    #[serde(default)]
    limit: Option<usize>,
}

fn dir_prefix(c: &ToolCtx, p: Option<&str>) -> Result<String, ToolError> {
    let rel = match p {
        Some(p) => c.resolve(p)?.1,
        None => ".".into(),
    };
    Ok(if rel == "." { String::new() } else { format!("{rel}/") })
}

fn list_files(c: &ToolCtx, input: Value) -> ToolResult {
    let i: ListIn = parse(input)?;
    let prefix = dir_prefix(c, i.path.as_deref())?;
    let limit = i.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(5_000);
    let recursive = i.recursive.unwrap_or(false);
    c.with_workspace(|w| {
        let mut dirs = BTreeSet::new();
        let mut files = Vec::new();
        for f in w.index.files().filter(|f| f.rel.starts_with(&prefix)) {
            let rest = &f.rel[prefix.len()..];
            match rest.split_once('/') {
                Some((d, _)) if !recursive => {
                    dirs.insert(format!("{prefix}{d}/"));
                }
                _ => files.push(f.rel.clone()),
            }
        }
        let total = dirs.len() + files.len();
        let entries: Vec<String> = dirs.into_iter().chain(files).take(limit).collect();
        json!({ "entries": entries, "total": total, "truncated": total > limit })
    })
}

fn find_files(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        #[serde(alias = "pattern")]
        glob: String,
        #[serde(default)]
        limit: Option<usize>,
    }
    let i: In = parse(input)?;
    let set = globset([i.glob.as_str()]).map_err(ToolError::invalid)?;
    let limit = i.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(5_000);
    c.with_workspace(|w| {
        let m: Vec<&str> = w.index.files().map(|f| f.rel.as_str()).filter(|r| set.is_match(r)).collect();
        json!({ "files": m.iter().take(limit).collect::<Vec<_>>(), "total": m.len(), "truncated": m.len() > limit })
    })
}

fn get_file_tree(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        depth: Option<usize>,
    }
    let i: In = parse(input)?;
    let prefix = dir_prefix(c, i.path.as_deref())?;
    let depth = i.depth.unwrap_or(3).clamp(1, 8);
    c.with_workspace(|w| {
        let mut lines = BTreeSet::new();
        for f in w.index.files().filter(|f| f.rel.starts_with(&prefix)) {
            let parts: Vec<&str> = f.rel[prefix.len()..].split('/').collect();
            for d in 1..=parts.len().min(depth) {
                let is_dir = d < parts.len();
                lines.insert(format!("{}{}", parts[..d].join("/"), if is_dir { "/" } else { "" }));
            }
            if lines.len() > 5_000 {
                break;
            }
        }
        let tree: Vec<String> = lines
            .iter()
            .map(|l| {
                let depth = l.trim_end_matches('/').matches('/').count();
                let name = l.trim_end_matches('/').rsplit('/').next().unwrap_or(l);
                format!("{}{}{}", "  ".repeat(depth), name, if l.ends_with('/') { "/" } else { "" })
            })
            .collect();
        json!({ "root": if prefix.is_empty() { "." } else { prefix.trim_end_matches('/') }, "tree": tree.join("\n") })
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathIn {
    path: String,
}

/// Reads a UTF-8 text file within limits and records its fingerprint for stale-edit detection.
pub fn read_text(c: &ToolCtx, raw: &str) -> Result<(String, String), ToolError> {
    let (abs, rel) = c.resolve_existing(raw)?;
    let meta = std::fs::metadata(&abs).map_err(|e| ToolError::io(e, &rel))?;
    if !meta.is_file() {
        return Err(ToolError::invalid(format!("{rel} is not a file")));
    }
    if meta.len() > MAX_FILE_BYTES * 4 {
        return Err(ToolError::invalid(format!("{rel} is {} bytes; too large to read", meta.len())));
    }
    let bytes = std::fs::read(&abs).map_err(|e| ToolError::io(e, &rel))?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(ToolError::invalid(format!("{rel} is a binary file")));
    }
    let text = String::from_utf8(bytes).map_err(|_| ToolError::invalid(format!("{rel} is not valid UTF-8")))?;
    c.mark_seen(&rel, &text);
    Ok((rel, text))
}

fn numbered(text: &str, start: usize, end: usize) -> String {
    text.lines()
        .enumerate()
        .skip(start - 1)
        .take(end + 1 - start)
        .map(|(n, l)| {
            if l.len() > MAX_LINE_CHARS {
                let mut cut = MAX_LINE_CHARS;
                while !l.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{:>6}\t{}… [line truncated]", n + 1, &l[..cut])
            } else {
                format!("{:>6}\t{l}", n + 1)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn range_result(rel: &str, text: &str, start: usize, end: usize) -> ToolResult {
    let total = text.lines().count();
    if total == 0 {
        return Ok(json!({ "path": rel, "totalLines": 0, "content": "" }));
    }
    if start > total {
        return Err(ToolError::invalid(format!(
            "{} has {total} lines; start {start} is past the end (valid 1..={total}; use read_file without offset to read the whole file)",
            rel
        )));
    }
    let end = end.min(total).min(start + MAX_READ_LINES - 1);
    Ok(
        json!({ "path": rel, "startLine": start, "endLine": end, "totalLines": total, "content": numbered(text, start, end), "more": end < total }),
    )
}

fn read_file(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    }
    let i: In = parse(input)?;
    let (rel, text) = read_text(c, &i.path)?;
    let start = i.offset.unwrap_or(1).max(1);
    range_result(&rel, &text, start, start + i.limit.unwrap_or(MAX_READ_LINES).clamp(1, MAX_READ_LINES) - 1)
}

fn read_range(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
        // The result names its range startLine/endLine, so a model often sends those names back.
        #[serde(alias = "startLine", alias = "start_line")]
        start: usize,
        #[serde(alias = "endLine", alias = "end_line")]
        end: usize,
    }
    let i: In = parse(input)?;
    let (rel, text) = read_text(c, &i.path)?;
    let total = text.lines().count();
    if i.start == 0 {
        return Err(ToolError::invalid(format!(
            "read_range: start must be 1-based and at least 1, got {} ({} has {total} lines; valid 1..={total})",
            i.start, rel
        )));
    }
    if i.end < i.start {
        return Err(ToolError::invalid(format!(
            "read_range: end {} is before start {} ({} has {total} lines; valid 1..={total})",
            i.end, i.start, rel
        )));
    }
    if i.start > total {
        return Err(ToolError::invalid(format!(
            "read_range: start {} is past the end of {} ({} has {total} lines; valid 1..={total})",
            i.start, rel, rel
        )));
    }
    range_result(&rel, &text, i.start, i.end)
}

fn read_multiple_files(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        paths: Vec<String>,
    }
    let i: In = parse(input)?;
    if i.paths.len() > MAX_MULTI_FILES {
        return Err(ToolError::invalid(format!("at most {MAX_MULTI_FILES} paths")));
    }
    let files: Vec<Value> = i
        .paths
        .iter()
        .map(|p| match read_text(c, p).and_then(|(rel, t)| range_result(&rel, &t, 1, MAX_READ_LINES / 4)) {
            Ok(v) => v,
            Err(e) => json!({ "path": p, "error": e }),
        })
        .collect();
    Ok(json!({ "files": files }))
}

fn read_symbol(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        name: String,
        #[serde(default)]
        path: Option<String>,
    }
    let i: In = parse(input)?;
    let only = i.path.as_deref().map(|p| c.resolve(p).map(|r| r.1)).transpose()?;
    let defs: Vec<(String, u32, u32, String)> = c.with_workspace(|w| {
        w.index
            .definitions(&i.name)
            .iter()
            .filter(|(p, _)| only.as_ref().is_none_or(|o| o == p))
            .map(|(p, s)| (p.clone(), s.start_line, s.end_line, s.kind.clone()))
            .collect()
    })?;
    if defs.is_empty() {
        return Err(ToolError::new(ToolErrorKind::NotFound, format!("no definition named '{}' in the index", i.name)));
    }
    let out: Vec<Value> = defs
        .iter()
        .take(10)
        .map(|(p, s, e, k)| match read_text(c, p) {
            Ok((rel, t)) => json!({ "path": rel, "kind": k, "startLine": s, "endLine": e, "content": numbered(&t, *s as usize, (*e as usize).min(*s as usize + 300)) }),
            Err(err) => json!({ "path": p, "error": err }),
        })
        .collect();
    Ok(json!({ "definitions": out, "total": defs.len() }))
}

fn get_file_outline(c: &ToolCtx, input: Value) -> ToolResult {
    let i: PathIn = parse(input)?;
    let (abs, rel) = c.resolve_existing(&i.path)?;
    let lang = symbols::parser_lang(&abs).ok_or_else(|| ToolError::invalid(format!("no outline support for {rel}")))?;
    let (_, text) = read_text(c, &rel)?;
    let syms = symbols::extract(lang, &text);
    Ok(json!({ "path": rel, "language": lang.name(), "symbols": syms.definitions }))
}

fn get_file_metadata(c: &ToolCtx, input: Value) -> ToolResult {
    let i: PathIn = parse(input)?;
    let (abs, rel) = c.resolve_existing(&i.path)?;
    let m = std::fs::metadata(&abs).map_err(|e| ToolError::io(e, &rel))?;
    let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    let lines = (m.is_file() && m.len() <= MAX_FILE_BYTES).then(|| std::fs::read_to_string(&abs).ok().map(|t| t.lines().count())).flatten();
    Ok(json!({
        "path": rel,
        "isDir": m.is_dir(),
        "size": m.len(),
        "lines": lines,
        "modifiedUnix": modified,
        "language": symbols::language_name(&abs),
        "readonly": m.permissions().readonly(),
    }))
}

fn detect_language(c: &ToolCtx, input: Value) -> ToolResult {
    let i: PathIn = parse(input)?;
    let (abs, rel) = c.resolve_existing(&i.path)?;
    Ok(json!({ "path": rel, "language": symbols::language_name(&abs), "symbolSupport": symbols::parser_lang(&abs).is_some() }))
}

fn get_project_structure(c: &ToolCtx, _input: Value) -> ToolResult {
    c.with_workspace(|w| {
        let mut top = BTreeSet::new();
        for f in w.index.files() {
            top.insert(match f.rel.split_once('/') {
                Some((d, _)) => format!("{d}/"),
                None => f.rel.clone(),
            });
        }
        json!({ "project": w.project, "topLevel": top, "files": w.index.stats().files })
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn lists_finds_and_trees() {
        let c = ctx(&[("src/a.rs", "fn a() {}\n"), ("src/sub/b.rs", "fn b() {}\n"), ("README.md", "hi\n")]);
        let l = run(&c, "list_files", json!({})).unwrap();
        assert_eq!(l["entries"], json!(["src/", "README.md"]));
        let l = run(&c, "list_files", json!({ "path": "src", "recursive": true })).unwrap();
        assert_eq!(l["entries"], json!(["src/a.rs", "src/sub/b.rs"]));
        assert_eq!(run(&c, "find_files", json!({ "glob": "**/*.rs" })).unwrap()["total"], 2);
        assert_eq!(run(&c, "find_files", json!({ "pattern": "**/*.rs" })).unwrap()["total"], 2);
        let t = run(&c, "get_file_tree", json!({ "depth": 2 })).unwrap();
        assert!(t["tree"].as_str().unwrap().contains("  sub/"));
        assert!(run(&c, "list_files", json!({ "path": "../" })).is_err());
    }

    #[test]
    fn reads_ranges_symbols_and_outlines() {
        let c = ctx(&[("lib.rs", "fn one() {}\n\nfn two() {\n    one();\n}\n")]);
        let r = run(&c, "read_range", json!({ "path": "lib.rs", "start": 3, "end": 4 })).unwrap();
        assert_eq!(r["content"], "     3\tfn two() {\n     4\t    one();");
        let named = run(&c, "read_range", json!({ "path": "lib.rs", "startLine": 3, "endLine": 4 })).unwrap();
        assert_eq!(named["content"], r["content"], "startLine/endLine are accepted as start/end");
        assert_eq!(run(&c, "read_range", json!({ "path": "lib.rs", "start_line": 3, "end_line": 4 })).unwrap()["content"], r["content"]);
        let r = run(&c, "read_file", json!({ "path": "lib.rs", "offset": 5, "limit": 10 })).unwrap();
        assert_eq!((r["endLine"].as_u64(), r["more"].as_bool()), (Some(5), Some(false)));
        let s = run(&c, "read_symbol", json!({ "name": "two" })).unwrap();
        assert_eq!(s["definitions"][0]["startLine"], 3);
        let o = run(&c, "get_file_outline", json!({ "path": "lib.rs" })).unwrap();
        assert_eq!(o["symbols"].as_array().unwrap().len(), 2);
        assert!(c.seen.lock().unwrap().contains_key("lib.rs"));
        let m = run(&c, "read_multiple_files", json!({ "paths": ["lib.rs", "missing.rs"] })).unwrap();
        assert_eq!(m["files"][1]["error"]["kind"], "notFound");
        assert_eq!(run(&c, "detect_language", json!({ "path": "lib.rs" })).unwrap()["language"], "rust");
    }

    #[test]
    fn read_range_errors_say_what_is_valid() {
        let c = ctx(&[("notes.txt", "one\ntwo\nthree\n")]);
        let err = |v: Value| run(&c, "read_range", v).unwrap_err();
        let start0 = err(json!({ "path": "notes.txt", "start": 0, "end": 2 })).message;
        assert!(start0.contains("start must be 1-based") && start0.contains("3 lines") && start0.contains("1..=3"), "{start0}");
        let rev = err(json!({ "path": "notes.txt", "start": 4, "end": 2 })).message;
        assert!(rev.contains("end 2 is before start 4") && rev.contains("1..=3"), "{rev}");
        let past = err(json!({ "path": "notes.txt", "start": 9, "end": 10 })).message;
        assert!(past.contains("start 9 is past the end") && past.contains("3 lines") && past.contains("1..=3"), "{past}");

        // read_file's offset past the end also names the valid range.
        let past_file = run(&c, "read_file", json!({ "path": "notes.txt", "offset": 9, "limit": 5 })).unwrap_err().message;
        assert!(past_file.contains("start 9 is past the end") && past_file.contains("1..=3"), "{past_file}");
    }
}
