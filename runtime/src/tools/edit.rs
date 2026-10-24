//! Editing tools. Every edit validates its target, refuses stale or ambiguous changes, writes
//! atomically, rolls back multi-file changes on failure, and records a diff for review and undo.

use super::schema::{int, object, path};
use super::{FileChange, Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, fingerprint, parse, patch, tool};
use crate::context::index::{MAX_FILE_BYTES, is_secret};
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_WRITE_BYTES: usize = 4 * MAX_FILE_BYTES as usize;
const MAX_BATCH_EDITS: usize = 200;

pub fn tools() -> Vec<ToolDef> {
    use Permission::{Delete, Write};
    vec![
        tool(
            "replace_text",
            "Replace an exact text span in a file. Fails if the text is missing or matches more times than expectedCount (default 1).",
            Write,
            || {
                object(
                    json!({ "path": path("File path"), "oldText": { "type": "string", "description": "Exact existing text, including whitespace" }, "newText": { "type": "string" }, "expectedCount": int("Number of matches expected (default 1)", 1) }),
                    &["path", "oldText", "newText"],
                )
            },
            replace_text,
        ),
        tool(
            "replace_all",
            "Replace every occurrence of an exact text in a file.",
            Write,
            || {
                object(
                    json!({ "path": path("File path"), "oldText": { "type": "string" }, "newText": { "type": "string" } }),
                    &["path", "oldText", "newText"],
                )
            },
            replace_all,
        ),
        tool(
            "edit_files",
            "Apply many exact text replacements across one or more files in one atomic step. Edits to the same file apply in order. All files change or none.",
            Write,
            || {
                object(
                    json!({ "edits": { "type": "array", "minItems": 1, "maxItems": MAX_BATCH_EDITS, "items": object(
                        json!({ "path": path("File path"), "oldText": { "type": "string", "description": "Exact existing text, including whitespace" }, "newText": { "type": "string" }, "expectedCount": int("Number of matches expected (default 1)", 1), "all": { "type": "boolean", "description": "Replace every occurrence" } }),
                        &["path", "oldText", "newText"],
                    ) } }),
                    &["edits"],
                )
            },
            edit_files,
        ),
        tool(
            "insert_at_line",
            "Insert text before a 1-based line (use totalLines+1 to append).",
            Write,
            || {
                object(
                    json!({ "path": path("File path"), "line": int("Line to insert before", 1), "text": { "type": "string", "description": "Text to insert; a trailing newline is added if missing" } }),
                    &["path", "line", "text"],
                )
            },
            insert_at_line,
        ),
        tool(
            "delete_range",
            "Delete an inclusive 1-based line range.",
            Write,
            || {
                object(
                    json!({ "path": path("File path"), "start": int("First line", 1), "end": int("Last line", 1) }),
                    &["path", "start", "end"],
                )
            },
            delete_range,
        ),
        tool(
            "apply_patch",
            "Apply a unified diff (one or more files, '--- a/x' / '+++ b/x' headers). Context lines must match exactly. All files change or none.",
            Write,
            || object(json!({ "patch": { "type": "string", "description": "Unified diff text" } }), &["patch"]),
            apply_patch,
        ),
        tool(
            "create_file",
            "Create a new file. Fails if it exists unless overwrite is true.",
            Write,
            || {
                object(
                    json!({ "path": path("File path"), "content": { "type": "string" }, "overwrite": { "type": "boolean" } }),
                    &["path", "content"],
                )
            },
            create_file,
        ),
        tool(
            "rename_file",
            "Rename or move a file inside the workspace.",
            Write,
            || object(json!({ "from": path("Existing path"), "to": path("New path") }), &["from", "to"]),
            rename_file,
        ),
        tool("delete_file", "Delete a file.", Delete, || object(json!({ "path": path("File path") }), &["path"]), delete_file),
    ]
}

pub struct Change {
    pub abs: PathBuf,
    pub rel: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

pub fn diff(rel: &str, before: Option<&str>, after: Option<&str>) -> String {
    // Secret files show no content: the diff goes to the model, the UI, and the saved transcript.
    if is_secret(rel) {
        return format!("{rel}: secret file, contents not shown\n");
    }
    TextDiff::from_lines(before.unwrap_or(""), after.unwrap_or(""))
        .unified_diff()
        .context_radius(3)
        .header(&before.map_or("/dev/null".into(), |_| format!("a/{rel}")), &after.map_or("/dev/null".into(), |_| format!("b/{rel}")))
        .to_string()
}

fn load(c: &ToolCtx, raw: &str) -> Result<(PathBuf, String, String), ToolError> {
    let (abs, rel) = c.resolve_existing_write(raw)?;
    let text = read_existing(&abs, &rel)?;
    Ok((abs, rel, text))
}

fn read_existing(abs: &Path, rel: &str) -> Result<String, ToolError> {
    let bytes = std::fs::read(abs).map_err(|e| ToolError::io(e, rel))?;
    if bytes.len() > MAX_WRITE_BYTES || bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(ToolError::invalid(format!("{rel} is binary or too large to edit")));
    }
    String::from_utf8(bytes).map_err(|_| ToolError::invalid(format!("{rel} is not valid UTF-8")))
}

fn write_atomic(abs: &Path, content: &str) -> std::io::Result<()> {
    let dir = abs.parent().ok_or_else(|| std::io::Error::other("no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.task-x-{}.tmp", std::process::id()));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        if let Ok(m) = std::fs::metadata(abs) {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, abs)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn apply_one(ch: &Change) -> std::io::Result<()> {
    match &ch.after {
        Some(a) => write_atomic(&ch.abs, a),
        None => std::fs::remove_file(&ch.abs),
    }
}

fn revert_one(ch: &Change) {
    let _ = match &ch.before {
        Some(b) => write_atomic(&ch.abs, b),
        None => std::fs::remove_file(&ch.abs),
    };
}

/// Validates freshness, writes all changes (rolling back on failure), records them, and reindexes.
pub fn commit(c: &ToolCtx, op: &str, changes: Vec<Change>) -> ToolResult {
    c.check_cancel()?;
    {
        let seen = c.seen.lock().expect("seen lock");
        for ch in &changes {
            if let (Some(b), Some(fp)) = (&ch.before, seen.get(&ch.rel)) {
                if fingerprint(b) != *fp {
                    return Err(ToolError::new(
                        ToolErrorKind::Stale,
                        format!("{} changed on disk since it was last read; read it again before editing", ch.rel),
                    ));
                }
            }
        }
    }
    for ch in &changes {
        if ch.after.as_ref().is_some_and(|a| a.len() > MAX_WRITE_BYTES) {
            return Err(ToolError::invalid(format!("new content for {} is too large", ch.rel)));
        }
    }
    for (i, ch) in changes.iter().enumerate() {
        if let Err(e) = apply_one(ch) {
            changes[..i].iter().rev().for_each(revert_one);
            return Err(ToolError::io(e, &format!("writing {} (all changes rolled back)", ch.rel)));
        }
    }
    Ok(record(c, op, &changes))
}

/// Logs changes that are already on disk (for example, a worktree patch applied by git): diffs,
/// seen fingerprints, reindex, and one undoable operation.
pub fn record(c: &ToolCtx, op: &str, changes: &[Change]) -> Value {
    let records: Vec<FileChange> = changes
        .iter()
        .map(|ch| FileChange {
            path: ch.rel.clone(),
            diff: diff(&ch.rel, ch.before.as_deref(), ch.after.as_deref()),
            before: ch.before.clone(),
            after: ch.after.clone(),
            settled: false,
        })
        .collect();
    {
        let mut seen = c.seen.lock().expect("seen lock");
        for ch in changes {
            match &ch.after {
                Some(a) => seen.insert(ch.rel.clone(), fingerprint(a)),
                None => seen.remove(&ch.rel),
            };
        }
    }
    c.reindex(changes.iter().map(|ch| ch.abs.clone()).collect());
    let summary: Vec<Value> = records.iter().map(|r| json!({ "path": r.path, "diff": r.diff })).collect();
    let mut log = c.edits.lock().expect("edit log lock");
    let id = log.push(op, c.task_id.clone(), records);
    log.save();
    json!({ "changed": summary, "operation": id })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplaceIn {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    expected_count: Option<usize>,
}

/// Replaces `old` in `text`; `all` skips the expected-count check. Returns the new text and match count.
fn replace_in(text: &str, rel: &str, i: &ReplaceIn, all: bool) -> Result<(String, usize), ToolError> {
    if i.old_text.is_empty() {
        return Err(ToolError::invalid("oldText must not be empty"));
    }
    if i.old_text == i.new_text {
        return Err(ToolError::invalid("oldText and newText are identical"));
    }
    let n = text.matches(&i.old_text).count();
    if n == 0 {
        let hint = if text.contains(i.old_text.trim()) { " (it matches after trimming whitespace; copy the exact text)" } else { "" };
        return Err(ToolError::new(ToolErrorKind::NoMatch, format!("oldText not found in {rel}{hint}")));
    }
    let expected = i.expected_count.unwrap_or(1);
    if !all && n != expected {
        return Err(ToolError::new(
            ToolErrorKind::Ambiguous,
            format!("oldText matches {n} times in {rel} but expectedCount is {expected}; add surrounding lines to make it unique or set expectedCount"),
        ));
    }
    Ok((text.replace(&i.old_text, &i.new_text), n))
}

fn replace(c: &ToolCtx, input: Value, all: bool) -> ToolResult {
    let i: ReplaceIn = parse(input)?;
    let (abs, rel, text) = load(c, &i.path)?;
    let (after, n) = replace_in(&text, &rel, &i, all)?;
    let mut r =
        commit(c, if all { "replace_all" } else { "replace_text" }, vec![Change { abs, rel, before: Some(text), after: Some(after) }])?;
    r["replacements"] = json!(n);
    Ok(r)
}

fn edit_files(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        edits: Vec<Value>,
    }
    let i: In = parse(input)?;
    if i.edits.is_empty() || i.edits.len() > MAX_BATCH_EDITS {
        return Err(ToolError::invalid(format!("edits must have 1..={MAX_BATCH_EDITS} items")));
    }
    let mut changes: Vec<Change> = Vec::new();
    let mut total = 0;
    for (k, raw) in i.edits.into_iter().enumerate() {
        let mut v = raw;
        let all = v.as_object_mut().and_then(|o| o.remove("all")).is_some_and(|a| a.as_bool() == Some(true));
        let e: ReplaceIn = parse(v).map_err(|err| ToolError::invalid(format!("edits[{k}]: {}", err.message)))?;
        let (abs, rel) = c.resolve_existing_write(&e.path)?;
        let idx = match changes.iter().position(|ch| ch.rel == rel) {
            Some(x) => x,
            None => {
                let text = read_existing(&abs, &rel)?;
                changes.push(Change { abs, rel, before: Some(text.clone()), after: Some(text) });
                changes.len() - 1
            }
        };
        let ch = &mut changes[idx];
        let (after, n) = replace_in(ch.after.as_deref().unwrap_or_default(), &ch.rel, &e, all)
            .map_err(|err| ToolError::new(err.kind, format!("edits[{k}]: {}", err.message)))?;
        ch.after = Some(after);
        total += n;
    }
    changes.retain(|ch| ch.before != ch.after);
    let mut r = commit(c, "edit_files", changes)?;
    r["replacements"] = json!(total);
    Ok(r)
}

fn replace_text(c: &ToolCtx, input: Value) -> ToolResult {
    replace(c, input, false)
}

fn replace_all(c: &ToolCtx, input: Value) -> ToolResult {
    replace(c, input, true)
}

fn split_keep(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn insert_at_line(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
        line: usize,
        text: String,
    }
    let i: In = parse(input)?;
    let (abs, rel, text) = load(c, &i.path)?;
    let mut lines: Vec<String> = split_keep(&text).into_iter().map(String::from).collect();
    let n = lines.len();
    if i.line == 0 || i.line > n + 1 {
        return Err(ToolError::invalid(format!("line must be 1..={} for {rel}", n + 1)));
    }
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    if let Some(last) = lines.last_mut().filter(|l| i.line == n + 1 && !l.ends_with('\n')) {
        last.push_str(nl);
    }
    let mut ins = i.text;
    if !ins.ends_with('\n') {
        ins.push_str(nl);
    }
    lines.insert(i.line - 1, ins);
    let out = lines.concat();
    commit(c, "insert_at_line", vec![Change { abs, rel, before: Some(text), after: Some(out) }])
}

fn delete_range(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
        start: usize,
        end: usize,
    }
    let i: In = parse(input)?;
    let (abs, rel, text) = load(c, &i.path)?;
    let lines = split_keep(&text);
    if i.start == 0 || i.end < i.start || i.end > lines.len() {
        return Err(ToolError::invalid(format!("need 1 <= start <= end <= {} for {rel}", lines.len())));
    }
    let out = [&lines[..i.start - 1], &lines[i.end..]].concat().concat();
    commit(c, "delete_range", vec![Change { abs, rel, before: Some(text), after: Some(out) }])
}

fn create_file(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
        content: String,
        #[serde(default)]
        overwrite: bool,
    }
    let i: In = parse(input)?;
    let (abs, rel) = c.resolve_write(&i.path)?;
    if abs.is_dir() {
        return Err(ToolError::invalid(format!("{rel} is a directory")));
    }
    let before = if abs.exists() {
        if !i.overwrite {
            return Err(ToolError::new(
                ToolErrorKind::Conflict,
                format!("{rel} already exists; set overwrite to replace it or use an edit tool"),
            ));
        }
        Some(read_existing(&abs, &rel)?)
    } else {
        None
    };
    commit(c, "create_file", vec![Change { abs, rel, before, after: Some(i.content) }])
}

fn rename_file(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        from: String,
        to: String,
    }
    let i: In = parse(input)?;
    let (from_abs, from_rel, text) = load(c, &i.from)?;
    let (to_abs, to_rel) = c.resolve_write(&i.to)?;
    if to_abs.exists() {
        return Err(ToolError::new(ToolErrorKind::Conflict, format!("{to_rel} already exists")));
    }
    commit(
        c,
        "rename_file",
        vec![
            Change { abs: to_abs, rel: to_rel, before: None, after: Some(text.clone()) },
            Change { abs: from_abs, rel: from_rel, before: Some(text), after: None },
        ],
    )
}

fn delete_file(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        path: String,
    }
    let i: In = parse(input)?;
    let (abs, rel, text) = load(c, &i.path)?;
    commit(c, "delete_file", vec![Change { abs, rel, before: Some(text), after: None }])
}

fn apply_patch(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        patch: String,
    }
    let i: In = parse(input)?;
    let files = patch::parse(&i.patch).map_err(|e| ToolError::invalid(format!("invalid patch: {e}")))?;
    let mut changes = Vec::new();
    for fp in &files {
        let target = fp.new_path.as_ref().or(fp.old_path.as_ref()).ok_or_else(|| ToolError::invalid("patch has no file path"))?;
        let (abs, rel) = c.resolve_write(target)?;
        if changes.iter().any(|ch: &Change| ch.rel == rel) {
            return Err(ToolError::invalid(format!("{rel} appears twice in the patch")));
        }
        let before = match &fp.old_path {
            Some(_) => Some(read_existing(&abs, &rel)?),
            None if abs.exists() => return Err(ToolError::new(ToolErrorKind::Conflict, format!("{rel} already exists"))),
            None => None,
        };
        let after = match &fp.new_path {
            None => None,
            Some(_) => Some(
                patch::apply(before.as_deref().unwrap_or(""), fp)
                    .map_err(|e| ToolError::new(ToolErrorKind::Conflict, format!("{rel}: {e}")))?,
            ),
        };
        if let (Some(old), Some(new)) = (&fp.old_path, &fp.new_path) {
            if old != new {
                let (old_abs, old_rel) = c.resolve_write(old)?;
                changes.push(Change { abs: old_abs, rel: old_rel, before: before.clone(), after: None });
                changes.push(Change { abs, rel, before: None, after });
                continue;
            }
        }
        changes.push(Change { abs, rel, before, after });
    }
    commit(c, "apply_patch", changes)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn edit_files_is_ordered_and_atomic() {
        let c = ctx(&[("a.txt", "x y x\n"), ("b.txt", "b\n")]);
        let bad = json!({ "edits": [{ "path": "a.txt", "oldText": "y", "newText": "Y" }, { "path": "b.txt", "oldText": "zzz", "newText": "q" }] });
        assert_eq!(run(&c, "edit_files", bad).unwrap_err().kind, ToolErrorKind::NoMatch);
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "x y x\n");
        let ok = json!({ "edits": [
            { "path": "a.txt", "oldText": "x", "newText": "z", "all": true },
            { "path": "a.txt", "oldText": "z y", "newText": "Q" },
            { "path": "b.txt", "oldText": "b", "newText": "B" },
        ] });
        let r = run(&c, "edit_files", ok).unwrap();
        assert_eq!((r["replacements"].as_u64(), r["changed"].as_array().unwrap().len()), (Some(4), 2));
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "Q z\n");
        assert_eq!(std::fs::read_to_string(c.root.join("b.txt")).unwrap(), "B\n");
        assert_eq!(c.edits.lock().unwrap().operations.len(), 1);
    }

    #[test]
    fn replace_validates_matches_and_records_diff() {
        let c = ctx(&[("a.txt", "one\ntwo\ntwo\n")]);
        assert_eq!(
            run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "two", "newText": "2" })).unwrap_err().kind,
            ToolErrorKind::Ambiguous
        );
        assert_eq!(
            run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "zzz", "newText": "2" })).unwrap_err().kind,
            ToolErrorKind::NoMatch
        );
        let r = run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "one\n", "newText": "1\n" })).unwrap();
        assert!(r["changed"][0]["diff"].as_str().unwrap().contains("-one\n+1\n"));
        let r = run(&c, "replace_all", json!({ "path": "a.txt", "oldText": "two", "newText": "2" })).unwrap();
        assert_eq!(r["replacements"], 2);
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n2\n2\n");
        assert_eq!(c.edits.lock().unwrap().operations.len(), 2);
    }

    #[test]
    fn detects_stale_files() {
        let c = ctx(&[("a.txt", "x\n")]);
        run(&c, "read_file", json!({ "path": "a.txt" })).unwrap();
        std::fs::write(c.root.join("a.txt"), "x\nuser edit\n").unwrap();
        assert_eq!(
            run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "x", "newText": "y" })).unwrap_err().kind,
            ToolErrorKind::Stale
        );
        run(&c, "read_file", json!({ "path": "a.txt" })).unwrap();
        run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "x\n", "newText": "y\n" })).unwrap();
    }

    #[test]
    fn line_edits_create_rename_delete() {
        let c = ctx(&[("a.txt", "1\n2\n3")]);
        run(&c, "insert_at_line", json!({ "path": "a.txt", "line": 2, "text": "1.5" })).unwrap();
        run(&c, "insert_at_line", json!({ "path": "a.txt", "line": 5, "text": "4" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n1.5\n2\n3\n4\n");
        run(&c, "delete_range", json!({ "path": "a.txt", "start": 2, "end": 3 })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n3\n4\n");
        assert_eq!(run(&c, "create_file", json!({ "path": "a.txt", "content": "" })).unwrap_err().kind, ToolErrorKind::Conflict);
        run(&c, "create_file", json!({ "path": "dir/new.txt", "content": "n" })).unwrap();
        run(&c, "rename_file", json!({ "from": "dir/new.txt", "to": "moved.txt" })).unwrap();
        assert!(!c.root.join("dir/new.txt").exists() && c.root.join("moved.txt").exists());
        run(&c, "delete_file", json!({ "path": "moved.txt" })).unwrap();
        assert!(!c.root.join("moved.txt").exists());
        assert!(run(&c, "create_file", json!({ "path": "../escape.txt", "content": "" })).is_err());
        assert!(c.with_workspace(|w| w.index.get("moved.txt").is_none()).unwrap());
    }

    #[test]
    fn apply_patch_is_all_or_nothing() {
        let c = ctx(&[("a.txt", "a\nb\n"), ("b.txt", "x\n")]);
        let bad = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+A\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-nope\n+y\n";
        assert_eq!(run(&c, "apply_patch", json!({ "patch": bad })).unwrap_err().kind, ToolErrorKind::Conflict);
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "a\nb\n");
        let good = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+A\n--- /dev/null\n+++ b/c.txt\n@@ -0,0 +1 @@\n+new\n";
        run(&c, "apply_patch", json!({ "patch": good })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "A\nb\n");
        assert_eq!(std::fs::read_to_string(c.root.join("c.txt")).unwrap(), "new\n");
    }
}
