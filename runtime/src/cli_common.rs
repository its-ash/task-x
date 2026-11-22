//! What the external CLI engines (`claude_cli` for Claude Code, `agy_cli` for the Antigravity CLI)
//! share: the host they run in, the workspace snapshot that finds the files a CLI changed, the
//! steering inbox, timeline snapshots, and the task record each run is saved in.

use crate::agent::store::{self, Checkpoint, Status, Store, TaskRecord};
use crate::context::Workspace;
use crate::provider::{ContentPart, Message, Role};
use crate::tools::{EditLog, FileChange, edit};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub struct Host {
    pub workspace: Arc<RwLock<Option<Workspace>>>,
    /// Where the run is saved as a task record, for `agent/turns`, `agent/export`, and the history.
    pub store: Store,
    pub edits: Arc<Mutex<EditLog>>,
    pub seen: Arc<Mutex<HashMap<String, u64>>>,
    pub emit: crate::agent::EmitFn,
    pub steers: crate::agent::Steers,
    pub approve: crate::agent::ApproveFn,
    /// Shows `AskUserQuestion` questions to the user (`agent/askUser`).
    pub ask: crate::agent::AskFn,
    /// Shared with `agent/cancelTool`: one token per running shell call, keyed `<taskId>:<callId>`.
    pub tool_cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

/// `s` cut to at most `n` bytes (on a character boundary), noting how much was left out.
pub fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut cut = n;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more characters]", &s[..cut], s.len() - cut)
}

/// Indexed text contents, used to detect what the CLI changed.
pub fn snapshot(ws: &RwLock<Option<Workspace>>) -> BTreeMap<String, Arc<str>> {
    ws.read()
        .expect("workspace lock")
        .as_ref()
        .map(|w| w.index.files().filter_map(|f| f.content.clone().map(|c| (f.rel.clone(), c))).collect())
        .unwrap_or_default()
}

/// The `agent/steer` queue of one CLI run. It is removed when the run stops taking messages.
pub struct Inbox {
    steers: crate::agent::Steers,
    id: String,
}

impl Inbox {
    pub fn open(steers: crate::agent::Steers, id: &str) -> Self {
        steers.lock().expect("steers lock").insert(id.to_owned(), Vec::new());
        Self { steers, id: id.to_owned() }
    }

    pub fn take(&self) -> Vec<String> {
        self.steers.lock().expect("steers lock").get_mut(&self.id).map(std::mem::take).unwrap_or_default()
    }

    /// Stops taking messages, unless some wait: then they are returned and the inbox stays open.
    pub fn close_or_take(&self) -> Vec<String> {
        let mut map = self.steers.lock().expect("steers lock");
        match map.remove(&self.id) {
            Some(texts) if !texts.is_empty() => {
                map.insert(self.id.clone(), Vec::new());
                texts
            }
            _ => Vec::new(),
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.steers.lock().expect("steers lock").remove(&self.id);
    }
}

/// Mirrors shell/command tool calls from external CLI engines (`agy`, `claude`) into `crate::procs`
/// so every command an LLM runs appears live in the bottom Agents & Terminals view.
pub struct ProcTracker {
    task_id: String,
    cwd: String,
    active: HashMap<String, Arc<crate::procs::Proc>>,
}

impl ProcTracker {
    pub fn new(task_id: &str, root: &Path) -> Self {
        Self { task_id: task_id.to_owned(), cwd: root.to_string_lossy().into_owned(), active: HashMap::new() }
    }

    pub fn observe(&mut self, ev: &Value) {
        // Also observe subagent-wrapped events so subagent commands appear in Terminals too.
        if ev["type"].as_str() == Some("subagent") {
            self.observe(&ev["event"]);
            return;
        }
        match ev["type"].as_str() {
            Some("toolStarted") => {
                let Some(call_id) = ev["callId"].as_str() else { return };
                let tool = ev["tool"].as_str().unwrap_or_default().strip_prefix("mcp__task-x__").unwrap_or_else(|| ev["tool"].as_str().unwrap_or_default());
                let input = &ev["input"];
                let cmd = input["CommandLine"]
                    .as_str()
                    .or_else(|| input["command"].as_str())
                    .or_else(|| input["cmd"].as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let is_proc_tool = matches!(
                    tool,
                    "run_command" | "run_shell_command" | "Bash" | "run_tests" | "check_build" | "run_linter" | "format_code" | "execute_command" | "shell"
                );
                if !is_proc_tool && cmd.is_none() {
                    return;
                }
                let display = cmd.unwrap_or(tool);
                let cwd = input["Cwd"].as_str().or_else(|| input["cwd"].as_str()).filter(|s| !s.is_empty()).unwrap_or(&self.cwd);
                let proc = crate::procs::start(display, cwd, 0, Some(&self.task_id), CancellationToken::new());
                self.active.insert(call_id.to_owned(), proc);
            }
            Some("toolFinished") => {
                let Some(call_id) = ev["callId"].as_str() else { return };
                if let Some(proc) = self.active.remove(call_id) {
                    if let Some(out) = ev["output"].as_str().filter(|s| !s.is_empty()) {
                        proc.push(out.as_bytes());
                    }
                    let ok = ev["ok"].as_bool().unwrap_or(false);
                    let status = if ok { crate::procs::Status::Exited } else { crate::procs::Status::Failed };
                    proc.end(status, Some(if ok { 0 } else { 1 }));
                }
            }
            _ => {}
        }
    }
}

impl Drop for ProcTracker {
    fn drop(&mut self) {
        for (_, proc) in self.active.drain() {
            proc.end(crate::procs::Status::Stopped, None);
        }
    }
}

/// A timeline snapshot of the workspace for this run (`timelineSnapshot`); a failure is a warning.
pub async fn snap(root: &Path, task_id: &str, label: String, host: &Host) {
    let (root, id) = (root.to_path_buf(), task_id.to_owned());
    let res = tokio::task::spawn_blocking(move || crate::agent::timeline::Timeline::open(&root)?.snapshot(&id, &label))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match res {
        Ok(Some(s)) => (host.emit)(task_id, json!({ "type": "timelineSnapshot", "snapshot": s })),
        Ok(None) => {}
        Err(e) => (host.emit)(task_id, json!({ "type": "warning", "message": format!("The timeline could not save a snapshot: {e}") })),
    }
}

/// Refreshes the index, diffs it against `before`, and records what the CLI changed in the edit
/// log under `tool` (reviewable, keepable, revertible), emitting `fileChanged` for each file.
/// Returns the changed paths and the edit-log operations.
pub async fn record_changes(host: &Host, task_id: &str, before: &BTreeMap<String, Arc<str>>, tool: &str) -> Result<(Vec<String>, Vec<usize>), String> {
    let ws = host.workspace.clone();
    let after = tokio::task::spawn_blocking(move || {
        if let Some(w) = ws.write().expect("workspace lock").as_mut() {
            w.refresh();
        }
        snapshot(&ws)
    })
    .await
    .map_err(|e| e.to_string())?;
    let mut changes: Vec<FileChange> = Vec::new();
    for (path, old) in before {
        match after.get(path) {
            Some(new) if new == old => {}
            new => changes.push(FileChange {
                path: path.clone(),
                before: Some(old.to_string()),
                after: new.map(|n| n.to_string()),
                diff: edit::diff(path, Some(&**old), new.map(|n| &**n)),
                settled: false,
            }),
        }
    }
    for (path, new) in after.iter().filter(|(k, _)| !before.contains_key(k.as_str())) {
        changes.push(FileChange {
            path: path.clone(),
            before: None,
            after: Some(new.to_string()),
            diff: edit::diff(path, None, Some(&**new)),
            settled: false,
        });
    }
    let changed: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    let mut operations = Vec::new();
    if !changes.is_empty() {
        host.seen.lock().expect("seen lock").clear();
        let diffs: Vec<(String, String)> = changes.iter().map(|c| (c.path.clone(), c.diff.clone())).collect();
        let op = host.edits.lock().expect("edit log lock").push(tool, Some(task_id.to_owned()), changes);
        operations.push(op);
        for (path, diff) in diffs {
            (host.emit)(task_id, json!({ "type": "fileChanged", "path": path, "diff": diff, "operation": op }));
        }
    }
    Ok((changed, operations))
}

/// Saves a CLI run in the task's record (made on the first run): the prompt as a new turn with its
/// checkpoint, then the transcript, and the summary's totals. The CLI keeps the conversation
/// itself; the record is what `agent/turns`, `agent/export`, and the history read. `kind` is the
/// record's `providerKind`.
pub fn save_turn(store: &Store, task_id: &str, prompt: &str, kind: &str, started_ms: u64, transcript: Vec<Message>, s: &Value) -> std::io::Result<()> {
    let prompt = prompt.trim();
    let mut r = store.load(task_id).unwrap_or_else(|| TaskRecord {
        version: store::RECORD_VERSION,
        id: task_id.to_owned(),
        title: prompt.lines().next().unwrap_or_default().chars().take(80).collect(),
        created_ms: started_ms,
        updated_ms: started_ms,
        status: Status::Running,
        chat_mode: Default::default(),
        provider_kind: kind.into(),
        model: String::new(),
        system: String::new(),
        messages: Vec::new(),
        usage: Default::default(),
        iterations: 0,
        tool_calls: 0,
        changed_files: Default::default(),
        operations: Vec::new(),
        validations: Vec::new(),
        final_text: String::new(),
        error: None,
        enrichment: None,
        metrics: Default::default(),
        skipped_checks: Vec::new(),
        checkpoints: Vec::new(),
        forked_from: None,
    });
    r.checkpoints.push(Checkpoint { messages: r.messages.len(), operations: r.operations.len(), prompt: prompt.chars().take(200).collect(), created_ms: started_ms });
    r.messages.push(Message { role: Role::User, content: vec![ContentPart::Text { text: format!("<user_request>\n{prompt}\n</user_request>") }] });
    r.messages.extend(transcript);
    r.updated_ms = store::now_ms();
    r.status = match s["status"].as_str() {
        Some("completed") => Status::Completed,
        Some("cancelled") => Status::Cancelled,
        Some("maxIterations") => Status::MaxIterations,
        _ => Status::Failed,
    };
    r.chat_mode = serde_json::from_value(s["chatMode"].clone()).unwrap_or_default();
    if let Some(m) = s["model"].as_str() {
        r.model = m.to_owned();
    }
    let add = |sum: Option<u64>, n: Option<u64>| n.map(|n| sum.unwrap_or(0) + n).or(sum);
    r.usage.input_tokens = add(r.usage.input_tokens, s["usage"]["inputTokens"].as_u64());
    r.usage.output_tokens = add(r.usage.output_tokens, s["usage"]["outputTokens"].as_u64());
    r.usage.cached_tokens = add(r.usage.cached_tokens, s["usage"]["cachedTokens"].as_u64());
    r.iterations += s["iterations"].as_u64().unwrap_or(0) as u32;
    r.tool_calls += s["toolCalls"].as_u64().unwrap_or(0) as u32;
    r.metrics.model_ms += s["metrics"]["modelMs"].as_u64().unwrap_or(0);
    r.metrics.cost_usd += s["metrics"]["costUsd"].as_f64().unwrap_or(0.0);
    r.changed_files.extend(s["changedFiles"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_owned)));
    r.operations.extend(s["operations"].as_array().into_iter().flatten().filter_map(|o| o.as_u64().map(|o| o as usize)));
    r.final_text = s["text"].as_str().unwrap_or_default().to_owned();
    r.error = s["error"].as_str().map(str::to_owned);
    store.save(&r)
}
