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
            "Create a file, or replace a whole file whose current text you know (you read or wrote it in this task). An unread existing file needs overwrite true; the error shows its text, so the same call works next time.",
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
        tool(
            "copy_file",
            "Copy a file into the workspace. `from` is a path inside the workspace, or anywhere outside it (an absolute path such as ~/Downloads/logo.png); `to` is always inside the workspace. Binary files are copied byte for byte. Reading a file from outside the workspace needs the user's approval.",
            Write,
            || {
                object(
                    json!({
                        "from": path("Source file: inside the workspace, or an absolute path outside it"),
                        "to": path("Destination inside the workspace"),
                        "overwrite": { "type": "boolean", "description": "Replace the destination if it exists" },
                    }),
                    &["from", "to"],
                )
            },
            copy_file,
        ),
        tool(
            "move_file",
            "Move a file into the workspace, keeping nothing at the source. `from` is a path inside the workspace, or anywhere outside it; `to` is always inside the workspace. Reading or deleting a file outside the workspace needs the user's approval.",
            Write,
            || {
                object(
                    json!({
                        "from": path("Source file: inside the workspace, or an absolute path outside it"),
                        "to": path("Destination inside the workspace"),
                        "overwrite": { "type": "boolean", "description": "Replace the destination if it exists" },
                    }),
                    &["from", "to"],
                )
            },
            move_file,
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
    let (abs, rel) = c.resolve_existing(raw)?;
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
#[serde(rename_all = "camelCase")]
struct ReplaceIn {
    #[serde(alias = "file_path", alias = "filePath", alias = "file")]
    path: String,
    #[serde(alias = "old_text", alias = "old_string", alias = "oldString", alias = "search", alias = "old")]
    old_text: String,
    #[serde(alias = "new_text", alias = "new_string", alias = "newString", alias = "replace", alias = "new")]
    new_text: String,
    #[serde(default, alias = "expected_count")]
    expected_count: Option<usize>,
}

/// Fields a model may add to an edit; anything else is reported back as ignored.
const EDIT_FIELDS: &[&str] = &[
    "path",
    "file_path",
    "filePath",
    "file",
    "oldText",
    "old_text",
    "old_string",
    "oldString",
    "search",
    "old",
    "newText",
    "new_text",
    "new_string",
    "newString",
    "replace",
    "new",
    "expectedCount",
    "expected_count",
    "all",
];
/// The names a model uses for an edit's file.
const PATH_FIELDS: &[&str] = &["path", "file_path", "filePath", "file"];
/// The names a model uses for the text an edit replaces.
const OLD_FIELDS: &[&str] = &["oldText", "old_text", "old_string", "oldString", "search", "old"];

fn looks_like_edit(v: &Value) -> bool {
    v.as_object().is_some_and(|o| PATH_FIELDS.iter().chain(OLD_FIELDS).any(|k| o.contains_key(*k)))
}

/// The file path a call gives beside `edits`, used for each edit that names no file.
fn take_path(map: &mut serde_json::Map<String, Value>) -> Option<String> {
    PATH_FIELDS.iter().find_map(|k| match map.get(*k) {
        Some(Value::String(s)) if !s.trim().is_empty() => map.remove(*k).and_then(|v| v.as_str().map(str::to_owned)),
        _ => None,
    })
}

/// The batch of edits a model sent, in the shapes models actually produce: an array (the schema),
/// an object keyed by step or line (`{"1": {...}, "5": {...}}`), an edit object wrapped in a
/// one-key map (`{"5": {...}}`), or a single edit on its own. A strict array-only reader turned each
/// of those into a failed call.
fn normalize_edits(raw: Value) -> Result<Vec<Value>, ToolError> {
    fn unwrap_one(v: Value) -> Value {
        match v {
            Value::Object(o) if !looks_like_edit(&Value::Object(o.clone())) && o.len() == 1 => {
                let only = o.into_iter().next().map(|(_, v)| v).unwrap_or(Value::Null);
                if only.is_object() { unwrap_one(only) } else { Value::Object(Default::default()) }
            }
            other => other,
        }
    }

    let list: Vec<Value> = match raw {
        Value::Array(items) => items,
        // A single edit, without the array around it.
        Value::Object(o) if looks_like_edit(&Value::Object(o.clone())) => vec![Value::Object(o)],
        // A map of key → edit. Numeric keys are read in numeric order so the edits keep the order
        // the model meant (a plain JSON object sorts them as text: "10" before "2").
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            let numeric = entries.iter().all(|(k, _)| k.trim().parse::<u64>().is_ok());
            if numeric {
                entries.sort_by_key(|(k, _)| k.trim().parse::<u64>().unwrap_or(0));
            }
            entries.into_iter().map(|(_, v)| v).collect()
        }
        other => {
            return Err(ToolError::invalid(format!(
                "edits must be a list of {{path, oldText, newText}} objects, not {}",
                match other {
                    Value::Null => "null".to_owned(),
                    Value::Bool(_) => "a boolean".to_owned(),
                    Value::Number(_) => "a number".to_owned(),
                    Value::String(_) => "a string".to_owned(),
                    _ => "another type".to_owned(),
                }
            )));
        }
    };
    Ok(list.into_iter().map(unwrap_one).filter(|v| !v.as_object().is_some_and(|o| o.is_empty() && !looks_like_edit(v))).collect())
}

/// The edits of one `edit_files` call, read tolerantly. The schema asks for `{"edits": [...]}`, but a
/// model may write a single edit's fields directly (`{path, oldText, newText}`) or add a stray key
/// such as a step number. A strict `{edits}`-only reader answered all of those with
/// "unknown field `path`, expected `edits`", which never says what to send instead.
/// Returns the raw edits value, the file path given beside `edits` (for edits that name none), and
/// the top-level keys nothing reads.
fn take_edits(input: Value) -> Result<(Value, Option<String>, Vec<String>), ToolError> {
    let Value::Object(mut map) = input else {
        return Err(ToolError::invalid("edit_files takes an object: {\"edits\": [{\"path\": ..., \"oldText\": ..., \"newText\": ...}]}"));
    };
    if let Some(edits) = map.remove("edits") {
        // `{"path": "a.js", "edits": [{oldText, newText}, ...]}`: one file, several edits.
        let path = take_path(&mut map);
        return Ok((edits, path, map.keys().cloned().collect()));
    }
    let object = Value::Object(map.clone());
    // One edit written without the wrapper: the fields of an edit, directly.
    if looks_like_edit(&object) {
        return Ok((object, None, Vec::new()));
    }
    // A map of key → edit (`{"1": {...}}`), which `normalize_edits` reads in numeric order.
    if !map.is_empty() && map.values().all(Value::is_object) {
        return Ok((Value::Object(map), None, Vec::new()));
    }
    Err(ToolError::invalid(
        "edit_files needs edits: {\"edits\": [{\"path\": ..., \"oldText\": ..., \"newText\": ...}]}; a single edit may also be sent directly",
    ))
}

fn edit_files(c: &ToolCtx, input: Value) -> ToolResult {
    let (raw, path, mut ignored) = take_edits(input)?;
    let edits = normalize_edits(raw)?;
    if edits.is_empty() || edits.len() > MAX_BATCH_EDITS {
        return Err(ToolError::invalid(format!("edits must have 1..={MAX_BATCH_EDITS} items")));
    }
    let mut changes: Vec<Change> = Vec::new();
    let mut total = 0;
    for (k, raw) in edits.into_iter().enumerate() {
        let mut v = raw;
        let all = v.as_object_mut().and_then(|o| o.remove("all")).is_some_and(|a| a.as_bool() == Some(true));
        if let (Some(o), Some(p)) = (v.as_object_mut(), &path) {
            if !PATH_FIELDS.iter().any(|k| o.contains_key(*k)) {
                o.insert("path".into(), json!(p));
            }
        }
        if v.as_object().is_some_and(|o| !PATH_FIELDS.iter().any(|k| o.contains_key(*k))) {
            return Err(ToolError::invalid(format!(
                "edits[{k}] names no file: give each edit a \"path\", or put one \"path\" beside \"edits\" for edits to a single file"
            )));
        }
        if let Some(o) = v.as_object() {
            for key in o.keys().filter(|key| !EDIT_FIELDS.contains(&key.as_str())) {
                let key = key.clone();
                if !ignored.contains(&key) {
                    ignored.push(key);
                }
            }
        }
        let e: ReplaceIn = parse(v).map_err(|err| ToolError::invalid(format!("edits[{k}]: {}", err.message)))?;
        let (abs, rel) = c.resolve_existing(&e.path)?;
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
    if !ignored.is_empty() {
        // Tell the model which keys nothing reads, without failing work that is otherwise correct.
        r["ignoredFields"] = json!(ignored);
    }
    Ok(r)
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
            format!(
                "oldText matches {n} times in {rel} but expectedCount is {expected}; add surrounding lines to make it unique or set expectedCount"
            ),
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

/// Text of an existing file shown in a create_file conflict.
const MAX_CONFLICT_CHARS: usize = 20_000;

fn clip_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more bytes]", &s[..cut], s.len() - cut)
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
    let (abs, rel) = c.resolve(&i.path)?;
    if abs.is_dir() {
        return Err(ToolError::invalid(format!("{rel} is a directory")));
    }
    let before = if abs.exists() {
        let current = read_existing(&abs, &rel)?;
        if current == i.content {
            return Ok(json!({ "changed": [], "operation": null, "unchanged": rel }));
        }
        // Replacing a file is safe once the agent knows its current text: it read or wrote it, so
        // nothing it has not seen is lost. The change is undoable through the edit log either way.
        let known = c.seen.lock().expect("seen lock").get(&rel) == Some(&fingerprint(&current));
        if !i.overwrite && !known {
            // The error shows the file and counts as a read, so the same call succeeds next time.
            c.mark_seen(&rel, &current);
            return Err(ToolError::new(
                ToolErrorKind::Conflict,
                format!(
                    "{rel} already exists and was not read in this task. Its current text is below. To replace it, call create_file again (it now succeeds); for a small change, use replace_text or edit_files.\n<current path=\"{rel}\">\n{}\n</current>",
                    clip_text(&current, MAX_CONFLICT_CHARS)
                ),
            ));
        }
        Some(current)
    } else {
        None
    };
    commit(c, "create_file", vec![Change { abs, rel, before, after: Some(i.content) }])
}

fn rename_file(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        #[serde(alias = "old_path", alias = "old_name", alias = "source")]
        from: String,
        #[serde(alias = "new_path", alias = "new_name", alias = "destination")]
        to: String,
    }
    let i: In = parse(input)?;
    let (from_abs, from_rel, text) = load(c, &i.from)?;
    let (to_abs, to_rel) = c.resolve(&i.to)?;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferIn {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

/// The file as text, when it is small enough and valid UTF-8; `None` for a binary or huge file.
fn text_of(abs: &Path) -> Option<String> {
    let meta = std::fs::metadata(abs).ok()?;
    if meta.len() > MAX_WRITE_BYTES as u64 {
        return None;
    }
    read_existing(abs, &abs.display().to_string()).ok()
}

/// Copies or moves one file into the workspace. A text file goes through `commit`, so it gets a
/// diff, the stale check, review and undo like any edit; a binary file is copied byte for byte and
/// recorded without text (the review list holds text, and undo can only remove a new file).
fn transfer(c: &ToolCtx, input: Value, moving: bool) -> ToolResult {
    let op = if moving { "move_file" } else { "copy_file" };
    let i: TransferIn = parse(input)?;
    c.check_cancel()?;
    let (from_abs, from_label, outside) = c.resolve_source(&i.from)?;
    let (to_abs, to_rel) = c.resolve(&i.to)?;
    if to_abs == from_abs {
        return Err(ToolError::invalid(format!("{to_rel} is the same file as {from_label}; nothing to do")));
    }
    if to_abs.is_dir() || from_abs.is_dir() {
        return Err(ToolError::invalid("copy_file and move_file work on one file; name the file itself, not a folder"));
    }
    let existing = std::fs::symlink_metadata(&to_abs).ok();
    if existing.is_some() && !i.overwrite {
        return Err(ToolError::new(ToolErrorKind::Conflict, format!("{to_rel} already exists; pass overwrite: true to replace it")));
    }
    let result = match text_of(&from_abs) {
        Some(text) => {
            // Text: the diff, the stale check, review and undo all work as for any other edit.
            let before = if existing.is_some() {
                let current = read_existing(&to_abs, &to_rel)?;
                if current == text {
                    return Ok(json!({ "changed": [], "operation": null, "unchanged": to_rel }));
                }
                Some(current)
            } else {
                None
            };
            let mut changes = vec![Change { abs: to_abs.clone(), rel: to_rel.clone(), before, after: Some(text.clone()) }];
            if moving && !outside {
                // One operation, so undo puts the source back as well.
                changes.push(Change { abs: from_abs.clone(), rel: from_label.clone(), before: Some(text), after: None });
            }
            commit(c, op, changes)?
        }
        None => {
            std::fs::copy(&from_abs, &to_abs).map_err(|e| ToolError::io(e, &format!("copying {from_label} to {to_rel}")))?;
            if moving {
                std::fs::remove_file(&from_abs).map_err(|e| ToolError::io(e, &format!("removing {from_label}")))?;
            }
            // Binary content has no diff to show; the operation still records the file.
            record(c, op, &[Change { abs: to_abs.clone(), rel: to_rel.clone(), before: None, after: None }])
        }
    };
    let mut out = result;
    out["from"] = json!(from_label);
    out["to"] = json!(to_rel);
    if outside {
        out["outsideWorkspace"] = json!(true);
    }
    Ok(out)
}

fn copy_file(c: &ToolCtx, input: Value) -> ToolResult {
    transfer(c, input, false)
}

fn move_file(c: &ToolCtx, input: Value) -> ToolResult {
    transfer(c, input, true)
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
        let (abs, rel) = c.resolve(target)?;
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
                let (old_abs, old_rel) = c.resolve(old)?;
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
    use super::super::{outside_source, undo};
    use super::*;

    #[test]
    fn edit_files_takes_the_shapes_models_send() {
        let c = ctx(&[("a.txt", "one\ntwo\n"), ("b.txt", "three\n"), ("c.txt", "four\n")]);
        // The schema's array, with a stray key (the model added a step number) and a nested one-key
        // map — the two shapes that failed with "unknown field `5`".
        let r = run(
            &c,
            "edit_files",
            json!({ "edits": [
                { "path": "a.txt", "oldText": "one", "newText": "1", "5": "stray" },
                { "5": { "path": "b.txt", "oldText": "three", "newText": "3" } },
            ] }),
        )
        .unwrap();
        assert_eq!(r["replacements"], 2, "{r}");
        assert_eq!(r["ignoredFields"], json!(["5"]), "the model is told which keys nothing read");
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\ntwo\n");
        assert_eq!(std::fs::read_to_string(c.root.join("b.txt")).unwrap(), "3\n");

        // An object keyed by step, read in numeric order (not "10" before "2").
        let c2 = ctx(&[("c.txt", "a b c\n")]);
        run(
            &c2,
            "edit_files",
            json!({ "edits": {
                "2": { "path": "c.txt", "oldText": " b", "newText": " B" },
                "10": { "path": "c.txt", "oldText": " c", "newText": " C" },
            } }),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(c2.root.join("c.txt")).unwrap(), "a B C\n");

        // A single edit without the array around it.
        let c3 = ctx(&[("d.txt", "x\n")]);
        let r = run(&c3, "edit_files", json!({ "edits": { "path": "d.txt", "oldText": "x", "newText": "y" } })).unwrap();
        assert_eq!(r["replacements"], 1);

        // Unusable values still fail with a message that shows the shape.
        let bad = run(&c, "edit_files", json!({ "edits": "a.txt one->1" })).unwrap_err();
        assert_eq!(bad.kind, ToolErrorKind::InvalidInput);
        assert!(bad.message.contains("edits must be a list of {path, oldText, newText} objects"), "{}", bad.message);
        assert_eq!(
            run(&c, "edit_files", json!({ "edits": [{ "path": "a.txt", "oldText": "1" }] })).unwrap_err().kind,
            ToolErrorKind::InvalidInput,
            "a missing newText is still an error"
        );
    }

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

    /// A call whose top level is one edit, or has a stray key beside `edits`, must still work: this is
    /// the shape that answered "unknown field `path`, expected `edits`".
    #[test]
    fn edit_files_accepts_a_loose_top_level() {
        let c = ctx(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
        // The edit's own fields at the top level, with no `edits` wrapper.
        let r = run(&c, "edit_files", json!({ "path": "a.txt", "oldText": "one", "newText": "1" })).unwrap();
        assert_eq!((r["replacements"].as_u64(), r["changed"].as_array().unwrap().len()), (Some(1), 1), "{r}");
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n");

        // `edits` beside a stray key: the edit runs and the key is reported back.
        let r =
            run(&c, "edit_files", json!({ "edits": [{ "path": "b.txt", "oldText": "two", "newText": "2" }], "plan": "step 2" })).unwrap();
        assert_eq!(r["replacements"], 1, "{r}");
        assert_eq!(r["ignoredFields"], json!(["plan"]), "the stray top-level key is named");
        assert_eq!(std::fs::read_to_string(c.root.join("b.txt")).unwrap(), "2\n");

        // One path beside `edits` applies to the edits that name no file; an edit's own path wins.
        let c2 = ctx(&[("s.js", "let a = 1;\nlet b = 2;\n"), ("t.js", "t\n")]);
        let r = run(
            &c2,
            "edit_files",
            json!({ "path": "s.js", "edits": [
                { "oldText": "a = 1", "newText": "a = 10" },
                { "old_string": "b = 2", "new_string": "b = 20" },
                { "file_path": "t.js", "search": "t", "replace": "T" },
            ] }),
        )
        .unwrap();
        assert_eq!(r["replacements"], 3, "{r}");
        assert!(r.get("ignoredFields").is_none(), "the top-level path is read: {r}");
        assert_eq!(std::fs::read_to_string(c2.root.join("s.js")).unwrap(), "let a = 10;\nlet b = 20;\n");
        assert_eq!(std::fs::read_to_string(c2.root.join("t.js")).unwrap(), "T\n");
        let bad = run(&c2, "edit_files", json!({ "edits": [{ "oldText": "x", "newText": "y" }] })).unwrap_err();
        assert!(bad.message.contains("edits[0] names no file") && !bad.message.contains("missing field"), "{}", bad.message);

        // No edits at all: the message says what to send, instead of naming a serde field.
        let bad = run(&c, "edit_files", json!({ "5": "nope" })).unwrap_err();
        assert_eq!(bad.kind, ToolErrorKind::InvalidInput);
        assert!(bad.message.contains("edit_files needs edits"), "{}", bad.message);
        assert!(!bad.message.contains("unknown field"), "{}", bad.message);
    }

    #[test]
    fn copy_and_move_bring_a_file_in_from_outside() {
        let c = ctx(&[("src/a.txt", "one\n"), ("src/b.txt", "two\n")]);
        // Inside the workspace: an ordinary edit, with a diff and an undo entry.
        let r = run(&c, "copy_file", json!({ "from": "src/a.txt", "to": "src/copied.txt" })).unwrap();
        assert!(r["changed"][0]["diff"].as_str().unwrap().contains("+one"), "{r}");
        assert_eq!(std::fs::read_to_string(c.root.join("src/copied.txt")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(c.root.join("src/a.txt")).unwrap(), "one\n", "a copy keeps the source");

        // A duplicate destination needs `overwrite`, and the message says so.
        let err = run(&c, "copy_file", json!({ "from": "src/b.txt", "to": "src/copied.txt" })).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Conflict);
        assert!(err.message.contains("overwrite: true"), "{}", err.message);
        run(&c, "copy_file", json!({ "from": "src/b.txt", "to": "src/copied.txt", "overwrite": true })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("src/copied.txt")).unwrap(), "two\n");

        // A move inside the workspace is one operation, so one undo puts the source back.
        let r = run(&c, "move_file", json!({ "from": "src/a.txt", "to": "src/moved.txt" })).unwrap();
        assert!(!c.root.join("src/a.txt").exists() && c.root.join("src/moved.txt").exists());
        let op = r["operation"].as_u64().unwrap() as usize;
        undo(&c, op).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("src/a.txt")).unwrap(), "one\n", "undo restores the source");
        assert!(!c.root.join("src/moved.txt").exists(), "and removes the destination");
    }

    #[test]
    fn copy_file_takes_an_absolute_source_outside_the_workspace() {
        let c = ctx(&[("a.txt", "x\n")]);
        let outside = std::env::temp_dir().join(format!("taskx-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, "from outside\n").unwrap();
        let raw = outside.to_string_lossy().to_string();
        assert!(outside_source(&c.root, &raw), "an absolute path elsewhere is outside");
        assert!(!outside_source(&c.root, "a.txt") && !outside_source(&c.root, "src/../a.txt"), "an inside path is not");

        let r = run(&c, "copy_file", json!({ "from": raw, "to": "vendor/out.txt" })).unwrap();
        assert_eq!(r["outsideWorkspace"], true, "{r}");
        assert_eq!(std::fs::read_to_string(c.root.join("vendor/out.txt")).unwrap(), "from outside\n");
        assert!(outside.exists(), "a copy leaves the source alone");

        // The destination is still held to the workspace rules.
        let bad = run(&c, "copy_file", json!({ "from": raw, "to": "../escape.txt" })).unwrap_err();
        assert_eq!(bad.kind, ToolErrorKind::Forbidden, "{}", bad.message);
        std::fs::remove_file(&outside).ok();
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
        let r = run(&c, "replace_text", json!({ "file_path": "a.txt", "old_string": "1\n", "new_string": "one\n" })).unwrap();
        assert_eq!(r["replacements"], 1);
        assert!(
            run(&c, "replace_text", json!({ "file_path": "a.txt", "old_string": "one\n", "new_string": "1\n", "expected_count": 1 }))
                .is_ok()
        );
        let r = run(&c, "replace_all", json!({ "path": "a.txt", "oldText": "two", "newText": "2" })).unwrap();
        assert_eq!(r["replacements"], 2);
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n2\n2\n");
        assert_eq!(c.edits.lock().unwrap().operations.len(), 4);
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
    fn create_file_on_an_existing_file() {
        let c = ctx(&[("old.ts", "export const a = 1;\n")]);
        let same = run(&c, "create_file", json!({ "path": "old.ts", "content": "export const a = 1;\n" })).unwrap();
        assert_eq!(same["unchanged"], "old.ts", "the same content is not an error");
        let err = run(&c, "create_file", json!({ "path": "old.ts", "content": "new\n" })).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Conflict, "an unread file is not replaced blindly");
        assert!(err.message.contains("export const a = 1;"), "the error shows the current text");
        run(&c, "create_file", json!({ "path": "old.ts", "content": "new\n" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("old.ts")).unwrap(), "new\n", "the same call works after the conflict");
        std::fs::write(c.root.join("old.ts"), "user edit\n").unwrap();
        assert_eq!(
            run(&c, "create_file", json!({ "path": "old.ts", "content": "x\n" })).unwrap_err().kind,
            ToolErrorKind::Conflict,
            "a change by someone else needs a new look"
        );
        run(&c, "create_file", json!({ "path": "old.ts", "content": "x\n" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("old.ts")).unwrap(), "x\n");
    }

    #[test]
    fn line_edits_create_rename_delete() {
        let c = ctx(&[("a.txt", "1\n2\n3")]);
        run(&c, "insert_at_line", json!({ "path": "a.txt", "line": 2, "text": "1.5" })).unwrap();
        run(&c, "insert_at_line", json!({ "path": "a.txt", "line": 5, "text": "4" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n1.5\n2\n3\n4\n");
        run(&c, "delete_range", json!({ "path": "a.txt", "start": 2, "end": 3 })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n3\n4\n");
        run(&c, "create_file", json!({ "path": "a.txt", "content": "whole\n" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "whole\n", "a file the agent wrote can be replaced");
        run(&c, "create_file", json!({ "path": "dir/new.txt", "content": "n" })).unwrap();
        run(&c, "rename_file", json!({ "from": "dir/new.txt", "to": "moved.txt" })).unwrap();
        assert!(!c.root.join("dir/new.txt").exists() && c.root.join("moved.txt").exists());
        run(&c, "rename_file", json!({ "old_path": "moved.txt", "new_path": "dir/new.txt" })).unwrap();
        assert!(c.root.join("dir/new.txt").exists() && !c.root.join("moved.txt").exists());
        run(&c, "rename_file", json!({ "from": "dir/new.txt", "to": "moved.txt" })).unwrap();
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
