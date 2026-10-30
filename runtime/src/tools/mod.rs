//! Provider-independent tool registry. Every tool declares a schema, permission class, timeout,
//! and output limit; adapters translate `ToolSpec`s into each provider's tool-calling format.

pub mod browser;
pub mod edit;
pub mod exec;
pub mod fs;
pub mod git;
pub mod logs;
pub mod memory;
pub mod todos;
pub mod patch;
pub mod search;
pub mod shell_search;
pub mod validate;
pub mod web;

use crate::context::Workspace;
use crate::provider::ToolSpec;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const MAX_OUTPUT_CHARS: usize = 60_000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum Permission {
    Read,
    Write,
    Delete,
    Execute,
    Network,
    GitWrite,
    Install,
    /// Reads a file from outside the workspace (copying one in). The workspace's own rules do not
    /// cover it, so it asks in every mode that can write, and is refused in a read-only one.
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolRisk {
    ReadOnly,
    Mutating,
    Destructive,
}

impl From<Permission> for ToolRisk {
    fn from(permission: Permission) -> Self {
        match permission {
            Permission::Read => ToolRisk::ReadOnly,
            Permission::Write | Permission::GitWrite | Permission::Execute | Permission::Network | Permission::External => {
                ToolRisk::Mutating
            }
            Permission::Delete | Permission::Install => ToolRisk::Destructive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolErrorKind {
    InvalidInput,
    NotFound,
    Forbidden,
    Ambiguous,
    NoMatch,
    Stale,
    Conflict,
    Timeout,
    /// A momentary OS error (interrupted, would block, timed out); the same call may succeed.
    Transient,
    Cancelled,
    Failed,
    Io,
}

/// How the agent should react to a failed tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorClass {
    /// Transient. The same call may succeed; read-only calls are retried automatically.
    Retryable,
    /// Will fail again as it is. Change the approach.
    Fatal,
    /// The input or the workspace state is wrong. Fix the request first.
    UserError,
    /// Blocked by the permission policy or the user. Do not retry.
    PolicyViolation,
}

impl ToolErrorKind {
    pub fn class(self) -> ErrorClass {
        use ToolErrorKind as K;
        match self {
            K::Transient | K::Timeout => ErrorClass::Retryable,
            K::Forbidden => ErrorClass::PolicyViolation,
            K::InvalidInput | K::NotFound | K::Ambiguous | K::NoMatch | K::Stale | K::Conflict => ErrorClass::UserError,
            K::Cancelled | K::Failed | K::Io => ErrorClass::Fatal,
        }
    }
}

/// Attempts per read-only call when its error is retryable (the first try included).
pub const TOOL_ATTEMPTS: u32 = 3;

/// Wait before retry `attempt` (1-based): 200 ms, 400 ms, 800 ms, … plus up to 100 ms of jitter.
pub fn retry_backoff(attempt: u32, jitter_ms: u64) -> Duration {
    Duration::from_millis((200u64 << attempt.saturating_sub(1).min(6)) + jitter_ms % 100)
}

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl ToolError {
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::InvalidInput, m)
    }
    pub fn io(e: std::io::Error, what: &str) -> Self {
        use std::io::ErrorKind as E;
        let kind = match e.kind() {
            E::NotFound => ToolErrorKind::NotFound,
            E::Interrupted | E::WouldBlock | E::TimedOut => ToolErrorKind::Transient,
            _ => ToolErrorKind::Io,
        };
        Self::new(kind, format!("{what}: {e}"))
    }
}

pub type ToolResult = Result<Value, ToolError>;

/// A file the agent changed, kept so changes can be shown as diffs and undone (see `checkpoint`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: String,
    /// Set when the user kept (accepted) or reverted this change in the review panel.
    #[serde(default)]
    pub settled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub id: usize,
    pub tool: String,
    pub task_id: Option<String>,
    pub changes: Vec<FileChange>,
    pub undone: bool,
    /// Old, settled operations lose their file contents when the log is saved; they can no longer be undone.
    #[serde(default)]
    pub expired: bool,
}

/// Operations whose contents stay on disk; older settled ones are expired when the log is saved.
const MAX_KEPT_OPERATIONS: usize = 500;
const CHECKPOINT_VERSION: u32 = 1;

#[derive(Default)]
pub struct EditLog {
    /// One entry per tool call that changed files, oldest first. Ids are indexes.
    pub operations: Vec<Operation>,
    /// Where the log is saved after every change, so checkpoints survive a restart.
    pub file: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Checkpoints {
    version: u32,
    operations: Vec<Operation>,
}

impl EditLog {
    pub fn push(&mut self, tool: &str, task_id: Option<String>, changes: Vec<FileChange>) -> usize {
        let id = self.operations.len();
        self.operations.push(Operation { id, tool: tool.into(), task_id, changes, undone: false, expired: false });
        id
    }

    /// Loads the saved log for one workspace. A missing or unreadable file gives an empty log.
    pub fn open(file: PathBuf) -> Self {
        let operations = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Checkpoints>(&b).ok())
            .filter(|c| c.version <= CHECKPOINT_VERSION && c.operations.iter().enumerate().all(|(i, o)| o.id == i))
            .map(|c| c.operations)
            .unwrap_or_default();
        Self { operations, file: Some(file) }
    }

    /// Writes the log atomically. Failures are not fatal: the log still works in memory.
    pub fn save(&mut self) {
        let Some(file) = self.file.clone() else { return };
        let cut = self.operations.len().saturating_sub(MAX_KEPT_OPERATIONS);
        for op in self.operations[..cut].iter_mut().filter(|o| !o.expired && (o.undone || o.changes.iter().all(|c| c.settled))) {
            op.expired = true;
            for c in &mut op.changes {
                (c.before, c.after) = (None, None);
            }
        }
        let body = serde_json::to_vec(&Checkpoints { version: CHECKPOINT_VERSION, operations: self.operations.clone() });
        let write = |b: Vec<u8>| -> std::io::Result<()> {
            std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
            let tmp = file.with_extension("json.tmp");
            std::fs::write(&tmp, b)?;
            std::fs::rename(tmp, &file)
        };
        if let Ok(b) = body {
            let _ = write(b);
        }
    }
}

/// Net unsettled change to one file across all agent operations, for the review panel.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFile {
    pub path: String,
    pub original: Option<String>,
    pub current: Option<String>,
    pub added: usize,
    pub removed: usize,
    pub operations: Vec<usize>,
}

pub fn pending(log: &EditLog) -> Vec<PendingFile> {
    let mut out: Vec<PendingFile> = Vec::new();
    for op in log.operations.iter().filter(|o| !o.undone && !o.expired) {
        for ch in op.changes.iter().filter(|c| !c.settled) {
            match out.iter_mut().find(|p| p.path == ch.path) {
                Some(p) => {
                    p.current = ch.after.clone();
                    p.operations.push(op.id);
                }
                None => out.push(PendingFile {
                    path: ch.path.clone(),
                    original: ch.before.clone(),
                    current: ch.after.clone(),
                    added: 0,
                    removed: 0,
                    operations: vec![op.id],
                }),
            }
        }
    }
    out.retain(|p| p.original != p.current);
    for p in &mut out {
        let diff = similar::TextDiff::from_lines(p.original.as_deref().unwrap_or(""), p.current.as_deref().unwrap_or(""));
        for c in diff.iter_all_changes() {
            match c.tag() {
                similar::ChangeTag::Insert => p.added += 1,
                similar::ChangeTag::Delete => p.removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }
    }
    out
}

fn settle(log: &mut EditLog, path: &str, ops: &[usize]) {
    for op in log.operations.iter_mut().filter(|o| ops.contains(&o.id)) {
        op.changes.iter_mut().filter(|c| c.path == path).for_each(|c| c.settled = true);
    }
}

/// Keeps the agent's changes to `paths` (all pending files when `None`): they leave the review list.
pub fn keep(ctx: &ToolCtx, paths: Option<&[String]>) -> Vec<String> {
    let mut log = ctx.edits.lock().expect("edit log lock");
    let mut kept = Vec::new();
    for p in pending(&log) {
        if paths.is_none_or(|ps| ps.contains(&p.path)) {
            settle(&mut log, &p.path, &p.operations);
            kept.push(p.path);
        }
    }
    log.save();
    kept
}

/// Restores pending files to their content before the agent touched them. A file that changed
/// on disk after the agent's last write is left alone and reported as a conflict.
pub fn revert(ctx: &ToolCtx, paths: Option<&[String]>) -> ToolResult {
    let targets: Vec<PendingFile> =
        pending(&ctx.edits.lock().expect("edit log lock")).into_iter().filter(|p| paths.is_none_or(|ps| ps.contains(&p.path))).collect();
    let mut changes = Vec::new();
    let mut reverted = Vec::new();
    let mut conflicts = Vec::new();
    for p in &targets {
        let (abs, rel) = ctx.resolve(&p.path)?;
        let disk = std::fs::read_to_string(&abs).ok();
        if disk == p.current {
            changes.push(edit::Change { abs, rel, before: disk, after: p.original.clone() });
            reverted.push(p);
        } else {
            conflicts.push(json!({ "path": p.path, "reason": "file changed after the agent's last edit; left as is" }));
        }
    }
    ctx.seen.lock().expect("seen lock").retain(|k, _| !reverted.iter().any(|p| &p.path == k));
    if !changes.is_empty() {
        let r = edit::commit(ctx, "revert", changes)?;
        let mut log = ctx.edits.lock().expect("edit log lock");
        let rev_op = r["operation"].as_u64().map(|o| o as usize);
        for p in &reverted {
            let mut ops = p.operations.clone();
            ops.extend(rev_op);
            settle(&mut log, &p.path, &ops);
        }
        log.save();
    }
    Ok(json!({ "reverted": reverted.iter().map(|p| &p.path).collect::<Vec<_>>(), "conflicts": conflicts }))
}

/// Reverts one operation without touching later user edits: a file is restored only if its
/// current content still equals what the operation wrote. Other files are reported as conflicts.
pub fn undo(ctx: &ToolCtx, id: usize) -> ToolResult {
    let op = {
        let log = ctx.edits.lock().expect("edit log lock");
        let op = log.operations.get(id).ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, format!("no operation {id}")))?;
        if op.undone {
            return Err(ToolError::new(ToolErrorKind::Conflict, format!("operation {id} was already undone")));
        }
        if op.expired {
            return Err(ToolError::new(ToolErrorKind::Conflict, format!("operation {id} is too old to undo")));
        }
        op.clone()
    };
    let mut restored = Vec::new();
    let mut conflicts = Vec::new();
    let mut changes = Vec::new();
    for ch in op.changes.iter().rev() {
        let (abs, rel) = ctx.resolve(&ch.path)?;
        let current = std::fs::read_to_string(&abs).ok();
        if current == ch.after {
            changes.push(edit::Change { abs, rel, before: current, after: ch.before.clone() });
            restored.push(ch.path.clone());
        } else {
            conflicts.push(json!({ "path": ch.path, "reason": "file changed after this operation; left as is" }));
        }
    }
    // The undo itself is not stale-checked: we just compared against the exact content written.
    ctx.seen.lock().expect("seen lock").retain(|k, _| !restored.contains(k));
    let res = if changes.is_empty() { json!({ "changed": [] }) } else { edit::commit(ctx, &format!("undo {id}"), changes)? };
    let mut log = ctx.edits.lock().expect("edit log lock");
    // Neither the undone change nor the undo itself is left for review.
    let undo_op = res["operation"].as_u64().map(|o| o as usize);
    for path in &restored {
        settle(&mut log, path, &[id]);
        if let Some(u) = undo_op {
            settle(&mut log, path, &[u]);
        }
    }
    if conflicts.is_empty() {
        log.operations[id].undone = true;
    }
    log.save();
    Ok(json!({ "operation": id, "restored": restored, "conflicts": conflicts, "changed": res["changed"] }))
}

pub fn fingerprint(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// The path with `.` and `..` resolved, without touching the file system. A symlink is not followed,
/// so a path that merely looks inside the workspace is still judged by where it points.
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `raw` names a file outside the workspace: an absolute path elsewhere, or one that climbs
/// out with `..`. Copying such a file in reads something the workspace rules do not cover.
pub fn outside_source(root: &Path, raw: &str) -> bool {
    let p = Path::new(raw.trim());
    let joined = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    !lexical(&joined).starts_with(root)
}

pub struct ToolCtx {
    pub root: PathBuf,
    pub workspace: Arc<RwLock<Option<Workspace>>>,
    pub cancel: CancellationToken,
    /// Content fingerprints of files as the agent last saw them (read or wrote).
    pub seen: Arc<Mutex<HashMap<String, u64>>>,
    pub edits: Arc<Mutex<EditLog>>,
    pub task_id: Option<String>,
}

impl ToolCtx {
    /// Resolves a model-supplied path inside the workspace. Rejects traversal, absolute paths outside
    /// the root, and symlink escapes. `.git` internals are never accessible.
    pub fn resolve(&self, raw: &str) -> Result<(PathBuf, String), ToolError> {
        let raw = raw.trim();
        if raw.is_empty() || raw.contains('\0') {
            return Err(ToolError::invalid("path must be a non-empty string"));
        }
        let p = Path::new(raw);
        let rel_input = if p.is_absolute() {
            p.strip_prefix(&self.root).map_err(|_| ToolError::new(ToolErrorKind::Forbidden, format!("{raw} is outside the workspace")))?
        } else {
            p
        };
        let mut parts: Vec<&str> = Vec::new();
        for c in rel_input.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_str().ok_or_else(|| ToolError::invalid("path is not valid UTF-8"))?),
                Component::CurDir => {}
                _ => return Err(ToolError::new(ToolErrorKind::Forbidden, format!("{raw}: '..' and root components are not allowed"))),
            }
        }
        let rel = parts.join("/");
        if parts.first() == Some(&".git") {
            return Err(ToolError::new(ToolErrorKind::Forbidden, "the .git directory is not accessible to tools"));
        }
        let abs = self.root.join(&rel);
        // Canonicalize the deepest existing ancestor so symlinks cannot point outside the root.
        let existing = abs.ancestors().find(|a| a.exists()).unwrap_or(&self.root);
        let canon = existing.canonicalize().map_err(|e| ToolError::io(e, raw))?;
        if !canon.starts_with(&self.root) {
            return Err(ToolError::new(ToolErrorKind::Forbidden, format!("{raw} resolves outside the workspace")));
        }
        Ok((abs, if rel.is_empty() { ".".into() } else { rel }))
    }

    /// Like `resolve`, but a missing path that names exactly one indexed file by its trailing
    /// components (e.g. `Grid.vue` for `src/components/Grid.vue`) resolves to that file.
    pub fn resolve_existing(&self, raw: &str) -> Result<(PathBuf, String), ToolError> {
        let (abs, rel) = self.resolve(raw)?;
        if abs.exists() {
            return Ok((abs, rel));
        }
        let suffix = format!("/{rel}");
        let hits: Vec<String> = self
            .with_workspace(|w| w.index.files().filter(|f| f.rel.ends_with(&suffix)).take(6).map(|f| f.rel.clone()).collect())
            .unwrap_or_default();
        match hits.as_slice() {
            [one] => self.resolve(one),
            [] => Ok((abs, rel)),
            many => Err(ToolError::new(
                ToolErrorKind::Ambiguous,
                format!("{rel} matches several files; use the full path: {}", many.join(", ")),
            )),
        }
    }

    /// Like `resolve`, but for the source of a copy or a move: the file may also live outside the
    /// workspace (an absolute path, or one that climbs out with `..`). Returns the real path, a
    /// label for messages, and whether it is outside. Writes always go through `resolve`, so the
    /// destination of every edit stays inside the workspace.
    pub fn resolve_source(&self, raw: &str) -> Result<(PathBuf, String, bool), ToolError> {
        let raw = raw.trim();
        if raw.is_empty() || raw.contains('\0') {
            return Err(ToolError::invalid("path must be a non-empty string"));
        }
        let given = Path::new(raw);
        let joined = if given.is_absolute() { given.to_path_buf() } else { self.root.join(given) };
        if lexical(&joined).starts_with(&self.root) {
            // Inside: the workspace rules apply (including the `.git` refusal), on the normal form so
            // `src/../a.txt` resolves while `..` out of the root stays forbidden.
            let rel = lexical(&joined).strip_prefix(&self.root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            let (abs, rel) = self.resolve(&rel)?;
            if !abs.is_file() {
                return Err(ToolError::new(ToolErrorKind::NotFound, format!("{rel} is not a file")));
            }
            return Ok((abs.canonicalize().unwrap_or(abs), rel, false));
        }
        // Outside: real path, existing file. The call needs the user's approval (`Permission::External`).
        let abs = lexical(&joined).canonicalize().map_err(|e| ToolError::io(e, raw))?;
        if !abs.is_file() {
            return Err(ToolError::new(ToolErrorKind::NotFound, format!("{raw} is not a file")));
        }
        Ok((abs.clone(), abs.display().to_string(), true))
    }

    pub fn check_cancel(&self) -> Result<(), ToolError> {
        if self.cancel.is_cancelled() { Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled")) } else { Ok(()) }
    }

    pub fn with_workspace<T>(&self, f: impl FnOnce(&Workspace) -> T) -> Result<T, ToolError> {
        let g = self.workspace.read().expect("workspace lock");
        g.as_ref().map(f).ok_or_else(|| ToolError::new(ToolErrorKind::Failed, "no workspace is open"))
    }

    pub fn mark_seen(&self, rel: &str, content: &str) {
        self.seen.lock().expect("seen lock").insert(rel.to_owned(), fingerprint(content));
    }

    pub fn reindex(&self, paths: Vec<PathBuf>) {
        if let Some(w) = self.workspace.write().expect("workspace lock").as_mut() {
            w.files_changed(&paths);
        }
    }
}

pub fn parse<T: DeserializeOwned>(mut input: Value) -> Result<T, ToolError> {
    loop {
        let e = match T::deserialize(&input) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        if !coerce(&mut input, &e.to_string()) {
            return Err(ToolError::invalid(format!("invalid input: {e}")));
        }
    }
}

/// Models often send numbers and booleans as strings ("396", "true"). Converts the string that the error names when it is a valid value of the expected type.
fn coerce(v: &mut Value, err: &str) -> bool {
    let Some((raw, want)) = err.strip_prefix("invalid type: string \"").and_then(|r| r.split_once("\", expected ")) else {
        return false;
    };
    let to = if want.starts_with("a boolean") {
        raw.trim().parse::<bool>().ok().map(Value::Bool)
    } else if want.starts_with(['u', 'i', 'f']) {
        serde_json::from_str::<serde_json::Number>(raw.trim()).ok().map(Value::Number)
    } else {
        None
    };
    to.is_some_and(|to| replace_str(v, raw, &to))
}

fn replace_str(v: &mut Value, raw: &str, to: &Value) -> bool {
    match v {
        Value::String(s) if s == raw => {
            *v = to.clone();
            true
        }
        Value::Array(a) => a.iter_mut().fold(false, |hit, x| replace_str(x, raw, to) | hit),
        Value::Object(o) => o.values_mut().fold(false, |hit, x| replace_str(x, raw, to) | hit),
        _ => false,
    }
}

pub type Handler = fn(&ToolCtx, Value) -> ToolResult;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    pub permission: Permission,
    pub timeout: Duration,
    pub handler: Handler,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub permission: Permission,
    pub timeout_ms: u64,
}

pub const fn tool(
    name: &'static str,
    description: &'static str,
    permission: Permission,
    schema: fn() -> Value,
    handler: Handler,
) -> ToolDef {
    ToolDef { name, description, schema, permission, timeout: DEFAULT_TIMEOUT, handler }
}

pub fn registry() -> &'static [ToolDef] {
    static R: std::sync::OnceLock<Vec<ToolDef>> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        [
            fs::tools(),
            search::tools(),
            edit::tools(),
            git::tools(),
            validate::tools(),
            logs::tools(),
            memory::tools(),
            todos::tools(),
            web::tools(),
            browser::tools(),
        ]
        .into_iter()
        .flatten()
        .collect()
    })
}

pub fn find(name: &str) -> Option<&'static ToolDef> {
    registry().iter().find(|t| t.name == name)
}

pub fn infos() -> Vec<ToolInfo> {
    registry()
        .iter()
        .map(|t| ToolInfo {
            name: t.name,
            description: t.description,
            input_schema: (t.schema)(),
            permission: t.permission,
            timeout_ms: t.timeout.as_millis() as u64,
        })
        .collect()
}

pub fn specs(filter: impl Fn(&ToolDef) -> bool) -> Vec<ToolSpec> {
    registry()
        .iter()
        .filter(|t| filter(t))
        .map(|t| ToolSpec { name: t.name.into(), description: t.description.into(), input_schema: (t.schema)() })
        .collect()
}

/// Truncates large results so one tool call cannot flood the model context.
pub fn limit_output(v: Value) -> Value {
    let s = v.to_string();
    if s.len() <= MAX_OUTPUT_CHARS {
        return v;
    }
    let mut cut = MAX_OUTPUT_CHARS;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    json!({ "truncated": true, "originalChars": s.len(), "partial": &s[..cut], "hint": "Narrow the request (smaller range, path filter, or limit)." })
}

/// Runs a tool on a blocking thread with its timeout and the caller's cancellation token.
pub async fn call(def: &'static ToolDef, ctx: Arc<ToolCtx>, input: Value) -> ToolResult {
    let cancel = ctx.cancel.clone();
    let task = tokio::task::spawn_blocking(move || (def.handler)(&ctx, input));
    tokio::select! {
        _ = cancel.cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled")),
        r = tokio::time::timeout(def.timeout, task) => match r {
            Err(_) => {
                cancel.cancel();
                Err(ToolError::new(ToolErrorKind::Timeout, format!("{} timed out after {}s", def.name, def.timeout.as_secs())))
            }
            Ok(Err(e)) => Err(ToolError::new(ToolErrorKind::Failed, format!("tool panicked: {e}"))),
            Ok(Ok(r)) => r.map(limit_output),
        },
    }
}

/// Shared JSON-schema helpers.
pub mod schema {
    use serde_json::{Value, json};

    pub fn object(props: Value, required: &[&str]) -> Value {
        json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
    }
    pub fn path(desc: &str) -> Value {
        json!({ "type": "string", "description": desc })
    }
    pub fn int(desc: &str, min: i64) -> Value {
        json!({ "type": "integer", "minimum": min, "description": desc })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::context::index::tests::temp_repo;

    pub fn ctx(files: &[(&str, &str)]) -> Arc<ToolCtx> {
        let root = temp_repo(files);
        let ws = Workspace::open(&root, &[]).unwrap();
        Arc::new(ToolCtx {
            root,
            workspace: Arc::new(RwLock::new(Some(ws))),
            cancel: CancellationToken::new(),
            seen: Arc::default(),
            edits: Arc::default(),
            task_id: None,
        })
    }

    pub fn run(c: &ToolCtx, name: &str, input: Value) -> ToolResult {
        (find(name).unwrap_or_else(|| panic!("no tool {name}")).handler)(c, input)
    }

    #[test]
    fn error_kinds_map_to_one_class() {
        use ErrorClass as C;
        use ToolErrorKind as K;
        assert_eq!(K::Transient.class(), C::Retryable);
        assert_eq!(K::Timeout.class(), C::Retryable);
        assert_eq!(K::Forbidden.class(), C::PolicyViolation);
        assert_eq!(K::InvalidInput.class(), C::UserError);
        assert_eq!(K::Stale.class(), C::UserError);
        assert_eq!(K::Io.class(), C::Fatal);
        assert_eq!(K::Cancelled.class(), C::Fatal);
        let transient = ToolError::io(std::io::Error::from(std::io::ErrorKind::Interrupted), "read");
        assert_eq!(transient.kind, K::Transient);
        assert_eq!(ToolError::io(std::io::Error::from(std::io::ErrorKind::PermissionDenied), "read").kind, K::Io);
        assert_eq!(ToolError::io(std::io::Error::from(std::io::ErrorKind::NotFound), "read").kind, K::NotFound);
    }

    #[test]
    fn retry_backoff_doubles_and_stays_bounded() {
        assert_eq!(retry_backoff(1, 0), Duration::from_millis(200));
        assert_eq!(retry_backoff(2, 0), Duration::from_millis(400));
        assert_eq!(retry_backoff(3, 99), Duration::from_millis(899));
        assert_eq!(retry_backoff(3, 100), Duration::from_millis(800), "jitter wraps below 100 ms");
        assert_eq!(retry_backoff(40, 0), Duration::from_millis(200 << 6), "the doubling is capped");
        assert_eq!(retry_backoff(0, 0), Duration::from_millis(200));
    }

    #[test]
    fn risk_follows_permission() {
        assert_eq!(ToolRisk::from(Permission::Read), ToolRisk::ReadOnly);
        assert_eq!(ToolRisk::from(Permission::Execute), ToolRisk::Mutating);
        assert_eq!(ToolRisk::from(Permission::Delete), ToolRisk::Destructive);
    }

    #[test]
    fn registry_names_are_unique_and_schemas_are_objects() {
        let mut names: Vec<_> = registry().iter().map(|t| t.name).collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n);
        for t in registry() {
            assert_eq!((t.schema)()["type"], "object", "{}", t.name);
        }
        for required in [
            "list_files",
            "find_files",
            "get_file_tree",
            "get_file_metadata",
            "detect_language",
            "get_project_structure",
            "search_text",
            "search_regex",
            "find_all",
            "find_symbol",
            "find_references",
            "find_definition",
            "search_files_by_content",
            "read_file",
            "read_range",
            "read_multiple_files",
            "read_symbol",
            "get_file_outline",
            "replace_text",
            "replace_all",
            "insert_at_line",
            "delete_range",
            "apply_patch",
            "create_file",
            "rename_file",
            "delete_file",
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "get_changed_files",
            "get_file_history",
            "format_code",
            "run_linter",
            "run_tests",
            "check_build",
            "get_diagnostics",
            "run_command",
        ] {
            assert!(find(required).is_some(), "missing tool {required}");
        }
    }

    #[test]
    fn resolve_blocks_traversal_secrets_git_and_symlink_escapes() {
        let c = ctx(&[("a.txt", "x"), (".env", "S=1")]);
        assert_eq!(c.resolve("./a.txt").unwrap().1, "a.txt");
        assert_eq!(c.resolve(&c.root.join("a.txt").to_string_lossy()).unwrap().1, "a.txt");
        assert_eq!(c.resolve(".env").unwrap().1, ".env");
        for bad in ["../x", "/etc/passwd", ".git/config", "a/../../x", ""] {
            assert!(c.resolve(bad).is_err(), "{bad} should be rejected");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", c.root.join("link")).unwrap();
            assert_eq!(c.resolve("link/x").unwrap_err().kind, ToolErrorKind::Forbidden);
        }
    }

    #[test]
    fn secret_named_files_are_readable_writable_and_searchable() {
        let c = ctx(&[("a.txt", "x"), (".env", "KEY=old\n")]);
        assert!(run(&c, "read_file", json!({ "path": ".env" })).unwrap()["content"].as_str().unwrap().contains("KEY=old"));
        run(&c, "create_file", json!({ "path": ".env.local", "content": "TOKEN=abc\n" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join(".env.local")).unwrap(), "TOKEN=abc\n");
        let r = run(&c, "replace_text", json!({ "path": ".env", "oldText": "KEY=old", "newText": "KEY=new" })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join(".env")).unwrap(), "KEY=new\n");
        assert!(!r.to_string().contains("KEY="), "secret values stay out of diffs");
        assert!(run(&c, "create_file", json!({ "path": ".git/x", "content": "" })).is_err());
    }

    #[test]
    fn bare_names_resolve_to_one_indexed_file() {
        let c = ctx(&[("src/ui/Grid.vue", "<a/>\n"), ("a/x.ts", "1"), ("b/x.ts", "2")]);
        assert_eq!(c.resolve_existing("Grid.vue").unwrap().1, "src/ui/Grid.vue");
        assert_eq!(c.resolve_existing("ui/Grid.vue").unwrap().1, "src/ui/Grid.vue");
        assert_eq!(c.resolve_existing("x.ts").unwrap_err().kind, ToolErrorKind::Ambiguous);
        assert_eq!(c.resolve_existing("Nope.vue").unwrap().1, "Nope.vue");
        run(&c, "edit_files", json!({ "edits": [{ "path": "Grid.vue", "oldText": "a", "newText": "b" }] })).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("src/ui/Grid.vue")).unwrap(), "<b/>\n");
    }

    #[test]
    fn undo_restores_only_untouched_files() {
        let c = ctx(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+A\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-b\n+B\n";
        run(&c, "apply_patch", json!({ "patch": patch })).unwrap();
        std::fs::write(c.root.join("b.txt"), "B\nuser line\n").unwrap();
        let r = undo(&c, 0).unwrap();
        assert_eq!(r["restored"], json!(["a.txt"]));
        assert_eq!(r["conflicts"][0]["path"], "b.txt");
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "a\n");
        assert_eq!(std::fs::read_to_string(c.root.join("b.txt")).unwrap(), "B\nuser line\n");
        run(&c, "create_file", json!({ "path": "n.txt", "content": "x" })).unwrap();
        let id = c.edits.lock().unwrap().operations.iter().rposition(|o| o.tool == "create_file").unwrap();
        undo(&c, id).unwrap();
        assert!(!c.root.join("n.txt").exists());
        assert!(pending(&c.edits.lock().unwrap()).iter().all(|p| p.path != "n.txt"), "an undo leaves nothing to review");
        assert!(undo(&c, id).is_err());
    }

    #[test]
    fn pending_aggregates_and_keep_revert_settle_files() {
        let c = ctx(&[("a.txt", "1\n2\n"), ("b.txt", "x\n")]);
        run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "1\n", "newText": "one\n" })).unwrap();
        run(&c, "insert_at_line", json!({ "path": "a.txt", "line": 3, "text": "3" })).unwrap();
        run(&c, "replace_text", json!({ "path": "b.txt", "oldText": "x", "newText": "y" })).unwrap();
        let p = pending(&c.edits.lock().unwrap());
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].path.as_str(), p[0].added, p[0].removed, p[0].original.as_deref()), ("a.txt", 2, 1, Some("1\n2\n")));
        assert_eq!(keep(&c, Some(&["b.txt".into()])), ["b.txt"]);
        let r = revert(&c, None).unwrap();
        assert_eq!(r["reverted"], json!(["a.txt"]));
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "1\n2\n");
        assert_eq!(std::fs::read_to_string(c.root.join("b.txt")).unwrap(), "y\n");
        assert!(pending(&c.edits.lock().unwrap()).is_empty());
        run(&c, "replace_text", json!({ "path": "b.txt", "oldText": "y", "newText": "z" })).unwrap();
        std::fs::write(c.root.join("b.txt"), "user\n").unwrap();
        assert_eq!(revert(&c, None).unwrap()["conflicts"][0]["path"], "b.txt");
        assert_eq!(std::fs::read_to_string(c.root.join("b.txt")).unwrap(), "user\n");
    }

    #[test]
    fn checkpoints_survive_a_reload_and_undo_still_works() {
        let c = ctx(&[("a.txt", "a\n")]);
        let file = c.root.join(".anvil-data").join("checkpoints.json");
        *c.edits.lock().unwrap() = EditLog::open(file.clone());
        run(&c, "replace_text", json!({ "path": "a.txt", "oldText": "a", "newText": "b" })).unwrap();
        let reloaded = EditLog::open(file);
        assert_eq!(reloaded.operations.len(), 1);
        assert_eq!(pending(&reloaded).len(), 1);
        *c.edits.lock().unwrap() = reloaded;
        undo(&c, 0).unwrap();
        assert_eq!(std::fs::read_to_string(c.root.join("a.txt")).unwrap(), "a\n");
    }

    #[test]
    fn limits_large_output() {
        let v = limit_output(json!({ "s": "x".repeat(MAX_OUTPUT_CHARS * 2) }));
        assert_eq!(v["truncated"], true);
    }
}
