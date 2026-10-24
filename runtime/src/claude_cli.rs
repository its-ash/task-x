//! Claude Code CLI engine: runs `claude -p` headless in the workspace and maps its stream-json
//! output onto the same `agent/event` stream as the built-in agent. File changes are detected by
//! comparing indexed contents before and after the run, then recorded in the edit log so they can
//! be reviewed, kept, or undone like any other agent edit.

use crate::context::Workspace;
use crate::tools::{EditLog, FileChange, edit};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const PERMISSION_MODES: &[&str] = &["default", "acceptEdits", "plan"];
const TASK_TOOL_PREFIX: &str = "mcp__task-x__";
const MAX_TOOL_OUTPUT: usize = 120_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    pub prompt: String,
    pub task_id: String,
    /// Absolute path of the `claude` executable.
    pub cli_path: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    /// Claude Code session to resume (from an earlier run's result).
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default = "default_mode")]
    pub permission_mode: String,
    /// Tools Claude Code may use without asking (e.g. "Bash").
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Spending limit for one run (`--max-budget-usd`).
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    #[serde(default)]
    pub chat_mode: Option<String>,
    /// Give Claude the task-x tools (over MCP) instead of its own file tools and shell edits.
    #[serde(default = "yes")]
    pub use_task_tools: bool,
}

fn default_mode() -> String {
    "default".into()
}

fn yes() -> bool {
    true
}

/// Claude Code's own file tools, replaced by the task-x tools when `use_task_tools` is on.
const BUILTIN_FILE_TOOLS: &[&str] = &["Read", "Edit", "MultiEdit", "Write", "NotebookEdit", "Grep", "Glob", "LS"];
/// Shell commands that rewrite files; edits go through `replace_text`/`apply_patch` instead.
/// Claude Code matches `Bash(prefix:*)` rules against the start of the command.
const SHELL_EDITS: &[&str] = &[
    "Bash(python3 - <<:*)",
    "Bash(python - <<:*)",
    "Bash(python3 -c:*)",
    "Bash(python -c:*)",
    "Bash(node -e:*)",
    "Bash(sed -i:*)",
    "Bash(perl -i:*)",
    "Bash(perl -pi:*)",
    "Bash(cat >:*)",
    "Bash(cat <<:*)",
    "Bash(tee:*)",
];
const TOOL_NOTE: &str = "Work on files only through the mcp__task-x__ tools: read_file, read_range, search_text, search_regex, find_files, find_symbol, find_references, get_dependencies, replace_text, edit_files, insert_at_line, apply_patch, create_file, rename_file, delete_file. Read a file before you edit it. For several replacements in one or more files, call edit_files once with all of them. Never write or rewrite files with shell commands (python, node, sed, perl, cat, tee, heredocs) and never script edits; a hook blocks them. Use Bash only to build, test, lint, and run git.";
/// Catches shell file rewrites anywhere in a command (after `cd x &&`, in a pipe, in `$(...)`),
/// which the prefix rules in `SHELL_EDITS` miss.
static SHELL_EDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[;&|(\n]|\$\()\s*(?:sudo\s+)?(?:(?:python[0-9.]*|node|deno|bun|ruby|php|perl)\s+(?:-(?:\s|$)|<<|-[a-zA-Z]*[ceEi]\b)|sed\s+(?:-[a-zA-Z]*i|--in-place)|tee\b|cat\s*(?:>|<<[^\n]*>))",
    )
    .expect("shell edit regex")
});
const SHELL_EDIT_BLOCKED: &str = "Blocked: this command writes files from the shell. Use the mcp__task-x__ tools instead: replace_text for one change, edit_files for many changes across files, apply_patch for a diff, create_file for a new file.";

/// `--hook-shell-edit`: a Claude Code PreToolUse hook. Reads the hook JSON from stdin; exit code 2
/// blocks the Bash call and sends the reason to Claude.
pub fn shell_edit_hook(input: &str) -> Result<(), &'static str> {
    let cmd = serde_json::from_str::<Value>(input).ok().and_then(|v| v["tool_input"]["command"].as_str().map(String::from)).unwrap_or_default();
    if SHELL_EDIT.is_match(&cmd) { Err(SHELL_EDIT_BLOCKED) } else { Ok(()) }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn valid_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:()*/ ".contains(c))
}

pub fn validate(p: &Params) -> Result<(), String> {
    if p.prompt.trim().is_empty() || p.prompt.len() > 200_000 {
        return Err("prompt must be 1..=200000 characters".into());
    }
    if !p.cli_path.is_absolute() || !p.cli_path.is_file() {
        return Err(format!("Claude Code CLI not found at {}; install it or set task-x.claudeCode.path", p.cli_path.display()));
    }
    if !PERMISSION_MODES.contains(&p.permission_mode.as_str()) {
        return Err(format!("permissionMode must be one of {PERMISSION_MODES:?}"));
    }
    for t in p.model.iter().chain(p.session_id.iter()).chain(p.allowed_tools.iter()) {
        if !valid_token(t) {
            return Err(format!("invalid value '{t}'"));
        }
    }
    Ok(())
}

/// MCP tool names Claude may call without asking: reads always; edits and file deletes (both
/// undoable through the edit log) when edits are accepted; commands when Bash is allowed (autonomous).
fn task_tools_allowed(p: &Params) -> Vec<String> {
    use crate::tools::Permission;
    let bash = p.allowed_tools.iter().any(|t| t == "Bash");
    crate::tools::infos()
        .into_iter()
        .filter(|t| match t.permission {
            Permission::Read => true,
            Permission::Write | Permission::Delete => p.permission_mode == "acceptEdits",
            Permission::Execute => bash,
            _ => false,
        })
        .map(|t| format!("mcp__{}__{}", crate::mcp_server::SERVER_NAME, t.name))
        .collect()
}

/// `exe` and `root` start the runtime itself as the task-x MCP server for this run.
pub fn args(p: &Params, task_tools: Option<(&Path, &Path)>, instructions: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--permission-mode"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.push(p.permission_mode.clone());
    if let Some(m) = &p.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(s) = &p.session_id {
        a.extend(["--resume".into(), s.clone()]);
    }
    if let Some(b) = p.max_budget_usd.filter(|b| b.is_finite() && *b > 0.0) {
        a.extend(["--max-budget-usd".into(), format!("{b:.2}")]);
    }
    let mut allowed = p.allowed_tools.clone();
    if let Some((exe, root)) = task_tools.filter(|_| p.use_task_tools) {
        let server = json!({ "mcpServers": { crate::mcp_server::SERVER_NAME: {
            "type": "stdio",
            "command": exe,
            "args": ["--mcp-server", "--root", root],
        } } });
        let hook = format!("{} --hook-shell-edit", sh_quote(&exe.to_string_lossy()));
        let settings = json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": hook }] }] } });
        a.extend(["--mcp-config".into(), server.to_string(), "--settings".into(), settings.to_string()]);
        a.push("--disallowedTools".into());
        a.extend(BUILTIN_FILE_TOOLS.iter().chain(SHELL_EDITS).map(|t| t.to_string()));
        allowed.extend(task_tools_allowed(p));
    }
    // Claude Code reads only CLAUDE.md files itself, so task-x passes the other guidance it loaded.
    let system: Vec<&str> = [task_tools.filter(|_| p.use_task_tools).map(|_| TOOL_NOTE), Some(instructions).filter(|s| !s.is_empty())]
        .into_iter()
        .flatten()
        .collect();
    if !system.is_empty() {
        a.extend(["--append-system-prompt".into(), system.join("\n\n")]);
    }
    if !allowed.is_empty() {
        a.push("--allowedTools".into());
        a.extend(allowed);
    }
    a
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut cut = n;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more characters]", &s[..cut], s.len() - cut)
}

/// Converts one stream-json line into agent events. Keeps tool names and streaming state.
#[derive(Default)]
pub struct Mapper {
    tool_names: HashMap<String, String>,
    streamed_text: bool,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub tool_calls: u32,
    pub result: Option<Value>,
}

impl Mapper {
    pub fn map(&mut self, line: &str, chat_mode: &str) -> Vec<Value> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(s) = v["session_id"].as_str() {
            self.session_id = Some(s.to_owned());
        }
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                self.model = v["model"].as_str().map(str::to_owned);
                out.push(json!({
                    "type": "started",
                    "chatMode": chat_mode,
                    "engine": "claudeCode",
                    "enrichment": { "kind": "Claude Code", "plan": [], "instructions": [], "sources": [], "ambiguity": null },
                    "tools": v["tools"],
                }));
                if let Some(m) = &self.model {
                    out.push(json!({ "type": "model", "kind": "claudeCode", "model": m, "reason": "Claude Code CLI" }));
                }
            }
            Some("stream_event") => {
                let e = &v["event"];
                if e["type"] == "message_start" {
                    self.streamed_text = false;
                }
                if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                    if let Some(t) = e["delta"]["text"].as_str() {
                        self.streamed_text = true;
                        out.push(json!({ "type": "textDelta", "text": t }));
                    }
                }
            }
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") if !self.streamed_text => {
                            if let Some(t) = block["text"].as_str() {
                                out.push(json!({ "type": "textDelta", "text": t }));
                            }
                        }
                        Some("tool_use") => {
                            let id = block["id"].as_str().unwrap_or_default().to_owned();
                            // task-x tools show under their own names (read_file, replace_text, …).
                            let raw = block["name"].as_str().unwrap_or("tool");
                            let name = raw.strip_prefix(TASK_TOOL_PREFIX).unwrap_or(raw).to_owned();
                            self.tool_names.insert(id.clone(), name.clone());
                            self.tool_calls += 1;
                            out.push(json!({ "type": "toolStarted", "callId": id, "tool": name, "input": block["input"], "permission": "claude" }));
                        }
                        _ => {}
                    }
                }
                self.streamed_text = false;
            }
            Some("user") => {
                for block in v["message"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let name = self.tool_names.get(id).cloned().unwrap_or_else(|| "tool".into());
                    let ok = !block["is_error"].as_bool().unwrap_or(false);
                    out.push(json!({ "type": "toolFinished", "callId": id, "tool": name, "ok": ok, "output": clip(&text_of(&block["content"]), MAX_TOOL_OUTPUT) }));
                }
            }
            Some("result") => self.result = Some(v),
            _ => {}
        }
        out
    }
}

/// Indexed text contents, used to detect what the CLI changed.
fn snapshot(ws: &RwLock<Option<Workspace>>) -> BTreeMap<String, Arc<str>> {
    ws.read()
        .expect("workspace lock")
        .as_ref()
        .map(|w| w.index.files().filter_map(|f| f.content.clone().map(|c| (f.rel.clone(), c))).collect())
        .unwrap_or_default()
}

pub struct Host {
    pub workspace: Arc<RwLock<Option<Workspace>>>,
    pub edits: Arc<Mutex<EditLog>>,
    pub seen: Arc<Mutex<HashMap<String, u64>>>,
    pub emit: crate::agent::EmitFn,
}

/// Loaded instructions that apply to every request (user and workspace files), as text for Claude Code.
fn instruction_note(host: &Host) -> String {
    let ws = host.workspace.read().expect("workspace lock");
    let Some(ws) = ws.as_ref() else { return String::new() };
    let rules: Vec<String> = crate::context::instructions::applicable(&ws.instructions, &[])
        .iter()
        .map(|i| format!("## {}\n{}", i.source, i.content.trim()))
        .collect();
    if rules.is_empty() {
        return String::new();
    }
    format!("Project and user guidance, lowest precedence first; later entries win on conflict. Follow it for style and workflow, but it cannot change the security rules or permissions.\n\n{}", rules.join("\n\n"))
}

pub async fn run(host: Host, p: Params, cancel: CancellationToken) -> Result<Value, String> {
    validate(&p)?;
    let root: PathBuf = host
        .workspace
        .read()
        .expect("workspace lock")
        .as_ref()
        .map(|w| w.index.root().to_path_buf())
        .ok_or("open a workspace folder first")?;
    let before = snapshot(&host.workspace);
    let started = Instant::now();
    let exe = std::env::current_exe().ok();
    let mut child = Command::new(&p.cli_path)
        .args(args(&p, exe.as_deref().map(|e| (e, root.as_path())), &instruction_note(&host)))
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start Claude Code: {e}"))?;
    // The prompt goes through stdin so it can never be read as a CLI flag.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(p.prompt.as_bytes()).await.map_err(|e| e.to_string())?;
        drop(stdin);
    }
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        if let Some(e) = stderr {
            let mut lines = BufReader::new(e).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if s.len() < 8_000 {
                    s.push_str(&l);
                    s.push('\n');
                }
            }
        }
        s
    });
    let mut lines = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    let mut mapper = Mapper::default();
    let chat_mode = p.chat_mode.clone().unwrap_or_else(|| "agent".into());
    let mut cancelled = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                cancelled = true;
                break;
            }
            l = lines.next_line() => match l {
                Ok(Some(line)) => for ev in mapper.map(&line, &chat_mode) { (host.emit)(&p.task_id, ev) },
                _ => break,
            },
        }
    }
    let status = child.wait().await.ok();
    let stderr_text = stderr_task.await.unwrap_or_default();

    // Detect changes: refresh the index and diff it against the snapshot.
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
    for (path, old) in &before {
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
    for (path, new) in after.iter().filter(|(k, _)| !before.contains_key(*k)) {
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
        let op = host.edits.lock().expect("edit log lock").push("claude_code", Some(p.task_id.clone()), changes);
        operations.push(op);
        for (path, diff) in diffs {
            (host.emit)(&p.task_id, json!({ "type": "fileChanged", "path": path, "diff": diff, "operation": op }));
        }
    }

    let r = mapper.result.clone().unwrap_or(Value::Null);
    let status_name = if cancelled {
        "cancelled"
    } else if r["subtype"] == "error_max_turns" {
        "maxIterations"
    } else if r.is_null() || r["is_error"].as_bool() == Some(true) || status.is_some_and(|s| !s.success()) {
        "failed"
    } else {
        "completed"
    };
    let error = (status_name == "failed").then(|| {
        let msg = r["result"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| stderr_text.trim().lines().last().unwrap_or("Claude Code exited with an error").to_owned());
        clip(&msg, 2_000)
    });
    let usage = &r["usage"];
    // Claude Code reports cached prompt tokens apart from `input_tokens`; all of them were sent.
    let input = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().filter_map(|k| usage[*k].as_u64()).reduce(|a, b| a + b);
    let summary = json!({
        "taskId": p.task_id,
        "status": status_name,
        "chatMode": chat_mode,
        "engine": "claudeCode",
        "text": r["result"].as_str().unwrap_or_default(),
        "changedFiles": changed,
        "operations": operations,
        "validations": [],
        "validationRan": false,
        "skippedChecks": [],
        "usage": { "inputTokens": input, "outputTokens": usage["output_tokens"].as_u64() },
        "metrics": { "modelMs": started.elapsed().as_millis() as u64, "costUsd": r["total_cost_usd"] },
        "model": mapper.model,
        "iterations": r["num_turns"].as_u64().unwrap_or(0),
        "toolCalls": mapper.tool_calls,
        "error": error,
        "sessionId": mapper.session_id,
    });
    (host.emit)(&p.task_id, json!({ "type": "done", "summary": summary }));
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stream_json_to_agent_events() {
        let mut m = Mapper::default();
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-sonnet-5-5","tools":["Read","Edit","Bash"]}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Look"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a.txt"}],"is_error":false}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","num_turns":2,"total_cost_usd":0.01,"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":20,"output_tokens":5},"session_id":"s1"}"#,
        ];
        let evs: Vec<Value> = lines.iter().flat_map(|l| m.map(l, "agent")).collect();
        let types: Vec<&str> = evs.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["started", "model", "textDelta", "toolStarted", "toolFinished", "textDelta"]);
        assert_eq!(evs[2]["text"], "Look", "streamed text is not repeated from the full message");
        assert_eq!(evs[4]["tool"], "Bash");
        assert_eq!(evs[4]["output"], "a.txt");
        assert_eq!(evs[5]["text"], "Done.", "unstreamed text comes from the full message");
        assert_eq!(m.session_id.as_deref(), Some("s1"));
        assert_eq!(m.result.as_ref().unwrap()["num_turns"], 2);
    }

    #[test]
    fn builds_safe_arguments() {
        let p = Params {
            prompt: "--help me".into(),
            task_id: "t".into(),
            cli_path: "/bin/sh".into(),
            model: Some("sonnet".into()),
            session_id: Some("abc-123".into()),
            permission_mode: "acceptEdits".into(),
            allowed_tools: vec!["Bash".into()],
            max_budget_usd: Some(1.5),
            chat_mode: None,
            use_task_tools: true,
        };
        validate(&p).unwrap();
        let plain = args(&p, None, "");
        assert!(!plain.iter().any(|x| x == "--append-system-prompt"), "nothing to add, no flag");
        let rules = args(&p, None, "## user\nBe brief.");
        assert!(rules.windows(2).any(|w| w[0] == "--append-system-prompt" && w[1].contains("Be brief.")));
        assert!(!plain.iter().any(|x| x == "--mcp-config"), "no runtime path, no task-x tools");
        let a = args(&p, Some((Path::new("/opt/task-x-runtime"), Path::new("/work/repo"))), "## user\nBe brief.");
        let note = &a[a.iter().position(|x| x == "--append-system-prompt").unwrap() + 1];
        assert!(note.contains("mcp__task-x__") && note.contains("Be brief."), "tool note and rules share one system prompt");
        let cfg: Value = serde_json::from_str(&a[a.iter().position(|x| x == "--mcp-config").unwrap() + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["task-x"]["args"], json!(["--mcp-server", "--root", "/work/repo"]));
        let settings: Value = serde_json::from_str(&a[a.iter().position(|x| x == "--settings").unwrap() + 1]).unwrap();
        assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/opt/task-x-runtime' --hook-shell-edit");
        let disallowed = &a[a.iter().position(|x| x == "--disallowedTools").unwrap()..];
        assert!(disallowed.iter().any(|x| x == "Edit") && disallowed.iter().any(|x| x == "Bash(python3 - <<:*)"));
        let allowed = &a[a.iter().position(|x| x == "--allowedTools").unwrap()..];
        assert!(allowed.iter().any(|x| x == "mcp__task-x__replace_text"), "edits are accepted in acceptEdits mode");
        assert!(allowed.iter().any(|x| x == "mcp__task-x__delete_file"), "undoable deletes are accepted in acceptEdits mode");
        assert!(allowed.iter().any(|x| x == "mcp__task-x__run_tests") && allowed.iter().any(|x| x == "Bash"));
        let ask = args(&Params { permission_mode: "default".into(), allowed_tools: vec![], ..p.clone() }, Some((Path::new("/x"), Path::new("/r"))), "");
        let allowed = &ask[ask.iter().position(|x| x == "--allowedTools").unwrap()..];
        assert!(allowed.iter().any(|x| x == "mcp__task-x__read_file") && !allowed.iter().any(|x| x == "mcp__task-x__replace_text") && !allowed.iter().any(|x| x == "mcp__task-x__delete_file"), "edits need approval");
        let off = args(&Params { use_task_tools: false, ..p.clone() }, Some((Path::new("/x"), Path::new("/r"))), "");
        assert!(!off.iter().any(|x| x == "--disallowedTools"));
        assert!(!a.iter().any(|x| x.contains("help me")), "the prompt goes through stdin, not argv");
        assert!(a.windows(2).any(|w| w == ["--resume", "abc-123"]));
        assert!(a.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(a.windows(2).any(|w| w == ["--max-budget-usd", "1.50"]));
        let bad = Params { permission_mode: "bypassPermissions".into(), ..p.clone() };
        assert!(validate(&bad).is_err(), "bypass mode is never used");
        let bad = Params { model: Some("--dangerous".into()), ..p };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn hook_blocks_shell_edits_anywhere_in_a_command() {
        let hook = |c: &str| shell_edit_hook(&json!({ "tool_name": "Bash", "tool_input": { "command": c } }).to_string());
        for bad in [
            "python3 - <<'EOF'\nopen('a').write('x')\nEOF",
            "cd ext && python3 - <<'EOF'\nx\nEOF",
            "python -c 'open(1)'",
            "node -e \"fs.writeFileSync('a','b')\"",
            "sed -i '' 's/a/b/' f.rs",
            "perl -pi -e 's/a/b/' f",
            "echo x | tee f.txt",
            "cat > f.txt <<EOF\nx\nEOF",
            "cat <<EOF > f.txt\nx\nEOF",
        ] {
            assert!(hook(bad).is_err(), "{bad}");
        }
        for ok in ["cargo test", "npm --prefix extension test", "git diff", "python3 tools/gen.py", "sed -n 1,5p f.rs", "cat f.rs", "grep -rn tee src", "rg python3", ""] {
            assert!(hook(ok).is_ok(), "{ok}");
        }
        assert!(hook("not json").is_ok() && shell_edit_hook("{}").is_ok());
    }
}
