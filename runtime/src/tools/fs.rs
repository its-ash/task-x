//! File discovery and reading tools.

use super::schema::{int, object, path};
use super::stream;
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use crate::context::index::{MAX_FILE_BYTES, globset};
use crate::context::symbols;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const DEFAULT_LIST_LIMIT: usize = 500;
const MAX_READ_LINES: usize = 2_000;
/// Lines a read with no range returns. A longer file also gets its outline, so the model reads only
/// the part it needs instead of the whole file.
const DEFAULT_READ_LINES: usize = 300;
const MAX_OUTLINE: usize = 80;
const MAX_SYMBOLS: usize = 10;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_MULTI_FILES: usize = 20;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
/// As base64 this fits the limit for images in a prompt (`provider::types`).
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

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
            "Read a text file with line numbers, or see a PNG, JPEG, GIF, or WebP image. Any path on the machine works: in the workspace, absolute, or ~/. list_files also lists folders outside the workspace. Without offset/limit it returns the first 300 lines, and a longer file also lists its symbols with line numbers: then read only the range you need.",
            Read,
            || {
                object(
                    json!({ "path": path("File path"), "offset": int("First line, 1-based (default 1)", 1), "limit": int("Max lines (default 300, max 2000)", 1) }),
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
            "Read the source of symbol definitions by name (optionally in one file). Pass names to read up to 10 symbols in one call.",
            Read,
            || {
                object(
                    json!({
                        "name": { "type": "string" },
                        "names": { "type": "array", "items": { "type": "string" }, "maxItems": MAX_SYMBOLS, "description": "Several symbols at once" },
                        "path": path("Limit to this file")
                    }),
                    &[],
                )
            },
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

fn list_outside(c: &ToolCtx, dir: &Path, recursive: bool, limit: usize) -> ToolResult {
    if !dir.is_dir() {
        return Err(ToolError::new(ToolErrorKind::NotFound, format!("{} is not a folder", dir.display())));
    }
    let walk = ignore::WalkBuilder::new(dir).standard_filters(false).max_depth(if recursive { None } else { Some(1) }).build();
    let mut entries = Vec::new();
    let mut truncated = false;
    for e in walk.flatten().filter(|e| e.depth() > 0) {
        c.check_cancel()?;
        if entries.len() == limit {
            truncated = true;
            break;
        }
        let dir = e.file_type().is_some_and(|t| t.is_dir());
        entries.push(format!("{}{}", e.path().display(), if dir { "/" } else { "" }));
    }
    Ok(json!({ "total": entries.len(), "entries": entries, "truncated": truncated }))
}

fn list_files(c: &ToolCtx, input: Value) -> ToolResult {
    let i: ListIn = parse(input)?;
    let limit = i.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(5_000);
    let recursive = i.recursive.unwrap_or(false);
    // The index covers only the workspace, so a folder outside it (or `.git`) is read from disk.
    if let Some(p) = i.path.as_deref().filter(|p| c.resolve(p).is_err()) {
        return list_outside(c, &c.resolve_read(p)?.0, recursive, limit);
    }
    let prefix = dir_prefix(c, i.path.as_deref())?;
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

/// The image type of file bytes, from their first bytes. A file's name can be wrong, and a model
/// provider refuses the whole request when an image is not valid.
fn image_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() > 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = u32::from(c[0]) << 16 | u32::from(*c.get(1).unwrap_or(&0)) << 8 | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// An image file as a tool result. `take_image` moves its data out, so the model gets a real image
/// and the chat and logs do not get megabytes of base64.
fn read_image(abs: &Path, rel: &str) -> Result<Option<Value>, ToolError> {
    let named = abs.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if !named {
        return Ok(None);
    }
    let meta = std::fs::metadata(abs).map_err(|e| ToolError::io(e, rel))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(ToolError::invalid(format!("{rel} is {} bytes; images can be up to {MAX_IMAGE_BYTES} bytes", meta.len())));
    }
    let bytes = std::fs::read(abs).map_err(|e| ToolError::io(e, rel))?;
    let media = image_type(&bytes).ok_or_else(|| ToolError::invalid(format!("{rel} has an image name, but it is not a PNG, JPEG, GIF, or WebP image")))?;
    Ok(Some(json!({ "path": rel, "image": true, "mediaType": media, "bytes": bytes.len(), "base64": base64(&bytes) })))
}

/// Takes the image out of a `read_file` result: (media type, base64 data).
pub fn take_image(v: &mut Value) -> Option<(String, String)> {
    if v["image"] != true {
        return None;
    }
    let data = v.as_object_mut()?.remove("base64")?;
    Some((v["mediaType"].as_str()?.to_owned(), data.as_str()?.to_owned()))
}

/// Reads a UTF-8 text file within limits and records its fingerprint for stale-edit detection.
pub fn read_text(c: &ToolCtx, raw: &str) -> Result<(String, String), ToolError> {
    let (abs, rel) = c.resolve_read(raw)?;
    let meta = std::fs::metadata(&abs).map_err(|e| ToolError::io(e, &rel))?;
    if !meta.is_file() {
        return Err(ToolError::invalid(format!("{rel} is not a file")));
    }
    if meta.len() > MAX_FILE_BYTES * 4 {
        return Err(ToolError::invalid(format!(
            "{rel} is {} bytes; too large to load whole. read_file, read_range, find_all, and search_text stream it",
            meta.len()
        )));
    }
    let bytes = std::fs::read(&abs).map_err(|e| ToolError::io(e, &rel))?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(ToolError::invalid(format!("{rel} is a binary file")));
    }
    let text = String::from_utf8(bytes).map_err(|_| ToolError::invalid(format!("{rel} is not valid UTF-8")))?;
    c.mark_seen(&rel, &text);
    Ok((rel, text))
}

/// A workspace text file above the whole-load limit, as (absolute, relative) path: reads and
/// searches stream it line by line. `None` for every other file.
pub fn large_file(c: &ToolCtx, raw: &str) -> Result<Option<(PathBuf, String)>, ToolError> {
    let (abs, rel) = c.resolve_read(raw)?;
    let meta = std::fs::metadata(&abs).map_err(|e| ToolError::io(e, &rel))?;
    if !meta.is_file() || meta.len() <= MAX_FILE_BYTES * 4 {
        return Ok(None);
    }
    if meta.len() > stream::MAX_BYTES {
        return Err(ToolError::invalid(format!("{rel} is {} bytes; above the {} byte stream limit", meta.len(), stream::MAX_BYTES)));
    }
    if !stream::is_text(&abs).map_err(|e| ToolError::io(e, &rel))? {
        return Err(ToolError::invalid(format!("{rel} is a binary file")));
    }
    Ok(Some((abs, rel)))
}

fn number_line(n: usize, l: &str) -> String {
    if l.len() <= MAX_LINE_CHARS {
        return format!("{n:>6}\t{l}");
    }
    let mut cut = MAX_LINE_CHARS;
    while !l.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{n:>6}\t{}… [line truncated]", &l[..cut])
}

fn numbered(text: &str, start: usize, end: usize) -> String {
    text.lines().enumerate().skip(start - 1).take(end + 1 - start).map(|(n, l)| number_line(n + 1, l)).collect::<Vec<_>>().join("\n")
}

/// `range_result` for a large file: one streamed pass keeps the range and counts all lines.
fn stream_range(c: &ToolCtx, abs: &Path, rel: &str, start: usize, end: usize) -> ToolResult {
    let end = end.min(start + MAX_READ_LINES - 1);
    let mut lines = Vec::new();
    let total = stream::each_line(abs, &c.cancel, |n, l| {
        if (start..=end).contains(&n) {
            lines.push(number_line(n, l));
        }
        true
    })
    .map_err(|e| ToolError::io(e, rel))?;
    if total == 0 {
        return Ok(json!({ "path": rel, "totalLines": 0, "content": "", "streamed": true }));
    }
    if start > total {
        return Err(ToolError::invalid(format!("{rel} has {total} lines; start {start} is past the end (valid 1..={total})")));
    }
    let end = end.min(total);
    Ok(json!({ "path": rel, "startLine": start, "endLine": end, "totalLines": total, "content": lines.join("\n"), "more": end < total, "streamed": true }))
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
        // Results name their range startLine/endLine, so a model often sends those, or read_range's start/end, here.
        #[serde(default, alias = "start", alias = "startLine", alias = "start_line")]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default, alias = "endLine", alias = "end_line")]
        end: Option<usize>,
    }
    let i: In = parse(input)?;
    let start = i.offset.unwrap_or(1).max(1);
    let whole = i.end.is_none() && i.limit.is_none();
    let end = match i.end {
        Some(end) if end < start => return Err(ToolError::invalid(format!("read_file: end {end} is before start {start}"))),
        Some(end) => end,
        None => start + i.limit.unwrap_or(DEFAULT_READ_LINES).clamp(1, MAX_READ_LINES) - 1,
    };
    let (abs, rel) = c.resolve_read(&i.path)?;
    if let Some(image) = read_image(&abs, &rel)? {
        return Ok(image);
    }
    if let Some((abs, rel)) = large_file(c, &i.path)? {
        let mut r = stream_range(c, &abs, &rel, start, end)?;
        if whole && r["more"] == true {
            r["note"] = json!(format!(
                "{rel} is too large to load whole; lines {start}-{} of {} were streamed. Read other lines with read_range, or find them with find_all or search_text.",
                r["endLine"], r["totalLines"]
            ));
        }
        return Ok(r);
    }
    let (rel, text) = read_text(c, &i.path)?;
    let mut r = range_result(&rel, &text, start, end)?;
    if whole && r["more"] == true {
        if let Some(outline) = outline(c, &rel, &text) {
            r["outline"] = json!(outline);
        }
        r["note"] = json!(format!(
            "Showing lines {start}-{} of {}. Read only what you need next: read_range for a range, read_symbol for a definition.",
            r["endLine"], r["totalLines"]
        ));
    }
    Ok(r)
}

/// The symbols of a file in one line each (`L12-40 fn name`), for parser-supported languages.
fn outline(c: &ToolCtx, rel: &str, text: &str) -> Option<Vec<String>> {
    let (abs, _) = c.resolve_read(rel).ok()?;
    let lang = symbols::parser_lang(&abs)?;
    let defs = symbols::extract(lang, text).definitions;
    let lines: Vec<String> =
        defs.iter().take(MAX_OUTLINE).map(|s| format!("L{}-{} {} {}", s.start_line, s.end_line, s.kind, s.name)).collect();
    (!lines.is_empty()).then_some(lines)
}

fn read_range(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
        // The result names its range startLine/endLine, so a model often sends those names back, or read_file's offset/limit.
        #[serde(alias = "startLine", alias = "start_line", alias = "offset")]
        start: usize,
        #[serde(default, alias = "endLine", alias = "end_line")]
        end: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    }
    let i: In = parse(input)?;
    let start = i.start;
    let end = i
        .end
        .or_else(|| i.limit.map(|n| start + n.max(1) - 1))
        .ok_or_else(|| ToolError::invalid("read_range: give end (the last line) or limit (the number of lines)"))?;
    if let Some((abs, rel)) = large_file(c, &i.path)? {
        if start == 0 || end < start {
            return Err(ToolError::invalid(format!("read_range: need 1 <= start <= end, got start {start}, end {end}")));
        }
        return stream_range(c, &abs, &rel, start, end);
    }
    let (rel, text) = read_text(c, &i.path)?;
    let total = text.lines().count();
    if start == 0 {
        return Err(ToolError::invalid(format!(
            "read_range: start must be 1-based and at least 1, got {start} ({rel} has {total} lines; valid 1..={total})"
        )));
    }
    if end < start {
        return Err(ToolError::invalid(format!(
            "read_range: end {end} is before start {start} ({rel} has {total} lines; valid 1..={total})"
        )));
    }
    if start > total {
        return Err(ToolError::invalid(format!(
            "read_range: start {start} is past the end of {rel} ({rel} has {total} lines; valid 1..={total})"
        )));
    }
    range_result(&rel, &text, start, end)
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
        .map(|p| match large_file(c, p).and_then(|big| match big {
            Some((abs, rel)) => stream_range(c, &abs, &rel, 1, MAX_READ_LINES / 4),
            None => read_text(c, p).and_then(|(rel, t)| range_result(&rel, &t, 1, MAX_READ_LINES / 4)),
        }) {
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
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        names: Vec<String>,
        #[serde(default)]
        path: Option<String>,
    }
    let i: In = parse(input)?;
    let names: Vec<String> = i.name.into_iter().chain(i.names).filter(|n| !n.trim().is_empty()).collect();
    if names.is_empty() || names.len() > MAX_SYMBOLS {
        return Err(ToolError::invalid(format!("give name, or names with 1 to {MAX_SYMBOLS} symbols")));
    }
    let only = i.path.as_deref().map(|p| c.resolve(p).map(|r| r.1)).transpose()?;
    let one = |name: &str| -> ToolResult {
        let defs: Vec<(String, u32, u32, String)> = c.with_workspace(|w| {
            w.index
                .definitions(name)
                .iter()
                .filter(|(p, _)| only.as_ref().is_none_or(|o| o == p))
                .map(|(p, s)| (p.clone(), s.start_line, s.end_line, s.kind.clone()))
                .collect()
        })?;
        if defs.is_empty() {
            return Err(ToolError::new(ToolErrorKind::NotFound, format!("no definition named '{name}' in the index")));
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
    };
    if let [name] = names.as_slice() {
        return one(name);
    }
    // Several symbols: one result each, and a missing one does not fail the others.
    let symbols: Vec<Value> = names
        .iter()
        .map(|n| match one(n) {
            Ok(mut v) => {
                v["name"] = json!(n);
                v
            }
            Err(e) => json!({ "name": n, "error": e }),
        })
        .collect();
    Ok(json!({ "symbols": symbols }))
}

fn get_file_outline(c: &ToolCtx, input: Value) -> ToolResult {
    let i: PathIn = parse(input)?;
    let (abs, rel) = c.resolve_read(&i.path)?;
    let lang = symbols::parser_lang(&abs).ok_or_else(|| ToolError::invalid(format!("no outline support for {rel}")))?;
    let (_, text) = read_text(c, &rel)?;
    let syms = symbols::extract(lang, &text);
    Ok(json!({ "path": rel, "language": lang.name(), "symbols": syms.definitions }))
}

fn get_file_metadata(c: &ToolCtx, input: Value) -> ToolResult {
    let i: PathIn = parse(input)?;
    let (abs, rel) = c.resolve_read(&i.path)?;
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
    let (abs, rel) = c.resolve_read(&i.path)?;
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
    fn reads_work_anywhere_and_writes_do_not() {
        let c = ctx(&[("a.txt", "in\n")]);
        let dir = std::env::temp_dir().join(format!("task-x-read-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "outside text\n").unwrap();
        let path = file.to_string_lossy().to_string();
        assert!(run(&c, "read_file", json!({ "path": path })).unwrap()["content"].as_str().unwrap().contains("outside text"));
        assert_eq!(run(&c, "read_range", json!({ "path": path, "start": 1, "end": 1 })).unwrap()["totalLines"], 1);
        assert_eq!(run(&c, "read_multiple_files", json!({ "paths": [path] })).unwrap()["files"][0]["totalLines"], 1);
        assert_eq!(run(&c, "get_file_metadata", json!({ "path": path })).unwrap()["size"], 13);
        let climbed = pathdiff_up(&c.root, &file);
        assert!(run(&c, "read_file", json!({ "path": climbed })).unwrap()["content"].as_str().unwrap().contains("outside text"), "a path that climbs out");
        let listed = run(&c, "list_files", json!({ "path": dir.to_string_lossy() })).unwrap();
        assert!(listed["entries"].to_string().contains("note.txt") && listed["entries"].to_string().contains("sub/"), "{listed}");
        std::fs::create_dir_all(c.root.join(".git")).unwrap();
        std::fs::write(c.root.join(".git/HEAD"), "ref: x\n").unwrap();
        assert!(run(&c, "read_file", json!({ "path": ".git/HEAD" })).unwrap()["content"].as_str().unwrap().contains("ref: x"));
        assert_eq!(run(&c, "read_file", json!({ "path": "a.txt" })).unwrap()["path"], "a.txt", "workspace paths stay relative");
        assert_eq!(run(&c, "read_file", json!({ "path": dir.join("missing").to_string_lossy() })).unwrap_err().kind, ToolErrorKind::NotFound);

        let edit = run(&c, "replace_text", json!({ "path": path, "oldText": "outside", "newText": "changed" })).unwrap_err();
        assert_eq!(edit.kind, ToolErrorKind::Forbidden, "a read does not open the file for edits");
        assert_eq!(run(&c, "create_file", json!({ "path": dir.join("new.txt").to_string_lossy(), "content": "x" })).unwrap_err().kind, ToolErrorKind::Forbidden);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "outside text\n");
        std::fs::remove_dir_all(dir).ok();
    }

    fn pathdiff_up(root: &Path, file: &Path) -> String {
        format!("{}{}", "../".repeat(root.components().count()), file.to_string_lossy().trim_start_matches('/'))
    }

    #[test]
    fn an_image_comes_back_as_an_image() {
        let c = ctx(&[("a.txt", "x\n")]);
        let dir = std::env::temp_dir().join(format!("task-x-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("panel.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let mut r = run(&c, "read_file", json!({ "path": png.to_string_lossy() })).unwrap();
        assert_eq!((r["mediaType"].as_str(), r["bytes"].as_u64()), (Some("image/png"), Some(12)));
        assert_eq!(take_image(&mut r), Some(("image/png".to_owned(), "iVBORw0KGgpyZXN0".to_owned())));
        assert!(r.get("base64").is_none(), "the data leaves the text result");
        let fake = dir.join("fake.png");
        std::fs::write(&fake, "not an image").unwrap();
        assert!(run(&c, "read_file", json!({ "path": fake.to_string_lossy() })).unwrap_err().message.contains("not a PNG"));
        assert_eq!(take_image(&mut json!({ "content": "text" })), None);
        assert_eq!((base64(b"M"), base64(b"Ma"), base64(b"Man")), ("TQ==".into(), "TWE=".into(), "TWFu".into()));
        std::fs::remove_dir_all(dir).ok();
    }

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
        assert!(run(&c, "list_files", json!({ "path": "../" })).is_ok(), "a folder outside the workspace can be listed");
        assert_eq!(run(&c, "list_files", json!({ "path": "../no-such-folder-here" })).unwrap_err().kind, ToolErrorKind::NotFound);
    }

    #[test]
    fn reads_ranges_symbols_and_outlines() {
        let c = ctx(&[("lib.rs", "fn one() {}\n\nfn two() {\n    one();\n}\n")]);
        let r = run(&c, "read_range", json!({ "path": "lib.rs", "start": 3, "end": 4 })).unwrap();
        assert_eq!(r["content"], "     3\tfn two() {\n     4\t    one();");
        let named = run(&c, "read_range", json!({ "path": "lib.rs", "startLine": 3, "endLine": 4 })).unwrap();
        assert_eq!(named["content"], r["content"], "startLine/endLine are accepted as start/end");
        assert_eq!(run(&c, "read_range", json!({ "path": "lib.rs", "start_line": 3, "end_line": 4 })).unwrap()["content"], r["content"]);
        assert_eq!(
            run(&c, "read_range", json!({ "path": "lib.rs", "offset": 3, "limit": 2 })).unwrap()["content"],
            r["content"],
            "offset/limit work too"
        );
        assert_eq!(
            run(&c, "read_file", json!({ "path": "lib.rs", "startLine": 3, "endLine": 4 })).unwrap()["content"],
            r["content"],
            "read_file takes a range"
        );
        assert_eq!(run(&c, "read_file", json!({ "path": "lib.rs", "start": 3, "end": 4 })).unwrap()["content"], r["content"]);
        assert_eq!(
            run(&c, "read_file", json!({ "path": "lib.rs", "offset": 4, "endLine": 3 })).unwrap_err().kind,
            ToolErrorKind::InvalidInput
        );
        assert_eq!(run(&c, "read_range", json!({ "path": "lib.rs", "start": 3 })).unwrap_err().kind, ToolErrorKind::InvalidInput);
        let r = run(&c, "read_file", json!({ "path": "lib.rs", "offset": 5, "limit": 10 })).unwrap();
        assert_eq!((r["endLine"].as_u64(), r["more"].as_bool()), (Some(5), Some(false)));
        let s = run(&c, "read_symbol", json!({ "name": "two" })).unwrap();
        assert_eq!(s["definitions"][0]["startLine"], 3);
        let both = run(&c, "read_symbol", json!({ "names": ["one", "two", "missing"] })).unwrap();
        assert_eq!(both["symbols"][1]["definitions"][0]["startLine"], 3, "{both}");
        assert_eq!(both["symbols"][2]["error"]["kind"], "notFound", "a missing symbol does not fail the others");
        let o = run(&c, "get_file_outline", json!({ "path": "lib.rs" })).unwrap();
        assert_eq!(o["symbols"].as_array().unwrap().len(), 2);
        assert!(c.seen.lock().unwrap().contains_key("lib.rs"));
        let m = run(&c, "read_multiple_files", json!({ "paths": ["lib.rs", "missing.rs"] })).unwrap();
        assert_eq!(m["files"][1]["error"]["kind"], "notFound");
        assert_eq!(run(&c, "detect_language", json!({ "path": "lib.rs" })).unwrap()["language"], "rust");
    }

    #[test]
    fn a_long_file_reads_its_start_and_lists_its_symbols() {
        let body: String = (0..400).map(|n| format!("fn f{n}() {{}}\n")).collect();
        let c = ctx(&[("big.rs", body.as_str())]);
        let r = run(&c, "read_file", json!({ "path": "big.rs" })).unwrap();
        assert_eq!((r["endLine"].as_u64(), r["more"].as_bool()), (Some(DEFAULT_READ_LINES as u64), Some(true)));
        assert!(
            r["outline"].as_array().is_some_and(|o| o.len() == MAX_OUTLINE && o[MAX_OUTLINE - 1].as_str().unwrap().contains("f79")),
            "{}",
            r["outline"]
        );
        assert!(r["note"].as_str().unwrap().contains("read_range"));
        let all = run(&c, "read_file", json!({ "path": "big.rs", "limit": 2000 })).unwrap();
        assert_eq!(all["endLine"], 400, "an explicit limit reads more");
        assert!(all.get("outline").is_none());
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
