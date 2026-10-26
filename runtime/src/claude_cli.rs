//! Claude Code CLI engine: runs `claude -p` headless in the workspace and maps its stream-json
//! output onto the same `agent/event` stream as the built-in agent. File changes are detected by
//! comparing indexed contents before and after the run, then recorded in the edit log so they can
//! be reviewed, kept, or undone like any other agent edit.

use crate::context::Workspace;
use crate::tools::{EditLog, FileChange, edit};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, Command};
use tokio_util::sync::CancellationToken;

const PERMISSION_MODES: &[&str] = &["default", "acceptEdits", "plan"];
const TASK_TOOL_PREFIX: &str = "mcp__task-x__";
const MAX_TOOL_OUTPUT: usize = 120_000;
/// How often waiting steering messages are sent to Claude Code.
const STEER_POLL: Duration = Duration::from_millis(250);
/// After a result, stdin closes when Claude Code stays silent this long (it merged the queued messages).
const IDLE_CLOSE: Duration = Duration::from_secs(30);
/// Gives the MCP server the approval address and token, as `<addr> <token>`.
pub const APPROVAL_ENV: &str = "TASK_X_APPROVAL";
/// The MCP tool Claude Code calls (`--permission-prompt-tool`) when a tool call needs permission.
pub const APPROVE_TOOL: &str = "approve";
const MAX_APPROVAL_BYTES: u64 = 1024 * 1024;

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
/// Long commands run in parallel with the rest of the work.
const BACKGROUND_NOTE: &str = "For slow commands (package installs, builds, long test runs, dev servers) call Bash with run_in_background: true and keep working on other parts of the task; do not sleep or wait for them. Read their output with BashOutput when you need it, and check it before you finish.";
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

/// `exe` and `root` start the runtime itself as the task-x MCP server for this run. With `approval`
/// (the `APPROVAL_ENV` value), tool calls that need permission are sent to the user through it.
pub fn args(p: &Params, task_tools: Option<(&Path, &Path)>, instructions: &str, approval: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--permission-mode"]
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
    // Claude Code's own Bash runs in its OS sandbox: writes stay inside the workspace.
    let mut settings = json!({ "sandbox": { "enabled": true, "autoAllowBashIfSandboxed": true } });
    if let Some((exe, root)) = task_tools.filter(|_| p.use_task_tools || approval.is_some()) {
        let mut server_args = vec!["--mcp-server".to_owned(), "--root".into(), root.to_string_lossy().into_owned()];
        if !p.use_task_tools {
            server_args.push("--no-tools".into());
        }
        let mut server = json!({ "type": "stdio", "command": exe, "args": server_args });
        if let Some(v) = approval {
            server["env"] = json!({ APPROVAL_ENV: v });
            a.extend(["--permission-prompt-tool".into(), format!("mcp__{}__{APPROVE_TOOL}", crate::mcp_server::SERVER_NAME)]);
        }
        a.extend(["--mcp-config".into(), json!({ "mcpServers": { crate::mcp_server::SERVER_NAME: server } }).to_string()]);
        if p.use_task_tools {
            let hook = format!("{} --hook-shell-edit", sh_quote(&exe.to_string_lossy()));
            settings["hooks"] = json!({ "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": hook }] }] });
            a.push("--disallowedTools".into());
            a.extend(BUILTIN_FILE_TOOLS.iter().chain(SHELL_EDITS).map(|t| t.to_string()));
            allowed.extend(task_tools_allowed(p));
        }
    }
    a.extend(["--settings".into(), settings.to_string()]);
    // Claude Code reads only CLAUDE.md files itself, so task-x passes the other guidance it loaded.
    let system: Vec<&str> = [task_tools.filter(|_| p.use_task_tools).map(|_| TOOL_NOTE), Some(BACKGROUND_NOTE), Some(instructions).filter(|s| !s.is_empty())]
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
    /// The last `result` line, the number of them (one per user message), and their summed token counts.
    pub result: Option<Value>,
    pub results: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
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
            Some("result") => {
                let usage = &v["usage"];
                // Claude Code reports cached prompt tokens apart from `input_tokens`; all of them were sent.
                let input = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().filter_map(|k| usage[*k].as_u64()).reduce(|a, b| a + b);
                let add = |sum: Option<u64>, n: Option<u64>| n.map(|n| sum.unwrap_or(0) + n).or(sum);
                self.input_tokens = add(self.input_tokens, input);
                self.output_tokens = add(self.output_tokens, usage["output_tokens"].as_u64());
                self.results += 1;
                self.result = Some(v);
            }
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
    pub steers: crate::agent::Steers,
    pub approve: crate::agent::ApproveFn,
    /// Shows `AskUserQuestion` questions to the user (`agent/askUser`).
    pub ask: crate::agent::AskFn,
    /// Shared with `agent/cancelTool`: one token per running `Bash` call, keyed `<taskId>:<callId>`.
    pub tool_cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

/// Process names of the shells Claude Code starts for its `Bash` tool (with or without its OS sandbox).
const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "sandbox-exec", "bwrap"];

/// `(pid, parent pid, command)` of every process; empty where `ps` is missing.
fn processes() -> Vec<(u32, u32, String)> {
    let Ok(out) = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,comm="]).stderr(Stdio::null()).output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            Some((pid, ppid, it.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

fn is_shell_of(parent: u32, ppid: u32, comm: &str) -> bool {
    ppid == parent && SHELLS.contains(&comm.rsplit('/').next().unwrap_or(comm).trim_start_matches('-'))
}

/// The shells Claude Code (`pid`) runs now.
fn shell_children(pid: u32) -> HashSet<u32> {
    processes().into_iter().filter(|(_, pp, c)| is_shell_of(pid, *pp, c)).map(|(p, ..)| p).collect()
}

/// Ends the shells Claude Code started after `before`, with every process under them.
fn kill_new_shells(pid: u32, before: &HashSet<u32>) {
    let all = processes();
    let mut doomed: Vec<u32> = all.iter().filter(|(p, pp, c)| is_shell_of(pid, *pp, c) && !before.contains(p)).map(|(p, ..)| *p).collect();
    let mut i = 0;
    while i < doomed.len() {
        let parent = doomed[i];
        doomed.extend(all.iter().filter(|(p, pp, _)| *pp == parent && !doomed.contains(p)).map(|(p, ..)| *p).collect::<Vec<_>>());
        i += 1;
    }
    if !doomed.is_empty() {
        let _ = std::process::Command::new("kill").arg("-KILL").args(doomed.iter().map(u32::to_string)).stderr(Stdio::null()).status();
    }
}

/// A random token, so only the MCP server of this run can ask for approvals.
fn approval_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(i, |d| d.as_nanos() + i));
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// The task-x permission class of a Claude Code tool, for the approval prompt.
fn permission_of(tool: &str) -> crate::tools::Permission {
    use crate::tools::Permission;
    if let Some(def) = tool.strip_prefix(TASK_TOOL_PREFIX).and_then(crate::tools::find) {
        return def.permission;
    }
    match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Permission::Write,
        "WebFetch" | "WebSearch" => Permission::Network,
        "Read" | "Grep" | "Glob" | "LS" => Permission::Read,
        _ => Permission::Execute,
    }
}

/// Asks the user about one tool call from Claude Code. "Allow for this session" covers later calls
/// of the same tool in this run. Returns Claude Code's permission-prompt answer.
async fn decide(line: &str, token: &str, call_id: String, task_id: &str, host: &ApprovalHost, granted: &Mutex<HashSet<String>>) -> Value {
    let deny = |message: &str| json!({ "behavior": "deny", "message": message });
    let Ok(v) = serde_json::from_str::<Value>(line) else { return deny("invalid approval request") };
    if v["token"].as_str() != Some(token) {
        return deny("invalid approval token");
    }
    let raw = v["tool_name"].as_str().unwrap_or("tool");
    let tool = raw.strip_prefix(TASK_TOOL_PREFIX).unwrap_or(raw).to_owned();
    let input = if v["input"].is_object() { v["input"].clone() } else { json!({}) };
    if tool == ASK_TOOL {
        return ask_user(input, call_id, task_id, host).await;
    }
    if !granted.lock().expect("grants lock").contains(&tool) {
        (host.emit)(task_id, json!({ "type": "approvalRequested", "callId": call_id, "tool": tool }));
        let req = crate::agent::ApprovalRequest {
            task_id: task_id.to_owned(),
            call_id: call_id.clone(),
            tool: tool.clone(),
            permission: permission_of(raw),
            input: input.clone(),
            preview: None,
        };
        let decision = (host.approve)(req, host.cancel.clone()).await;
        use crate::agent::ApprovalDecision as D;
        let name = match decision {
            D::Allow => "allow",
            D::AllowSession => "allowSession",
            D::Deny => "deny",
        };
        (host.emit)(task_id, json!({ "type": "approvalResolved", "callId": call_id, "decision": name }));
        match decision {
            D::Deny => return deny("The user denied this tool call in task-x."),
            D::AllowSession => {
                granted.lock().expect("grants lock").insert(tool);
            }
            D::Allow => {}
        }
    }
    json!({ "behavior": "allow", "updatedInput": input })
}

/// Claude Code's question tool. In `-p` mode it cannot show the questions, so task-x does.
const ASK_TOOL: &str = "AskUserQuestion";

/// Shows Claude Code's questions in the chat. The answers go back as `updatedInput.answers`
/// (question text → chosen labels, joined by ", "), the form Claude Code reads.
async fn ask_user(mut input: Value, call_id: String, task_id: &str, host: &ApprovalHost) -> Value {
    use crate::agent::{Question, QuestionKind};
    let asked: Vec<&Value> = input["questions"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let questions: Vec<Question> = asked
        .iter()
        .enumerate()
        .map(|(i, q)| Question {
            id: format!("q{}", i + 1),
            text: q["question"].as_str().unwrap_or_default().to_owned(),
            kind: if q["multiSelect"].as_bool() == Some(true) { QuestionKind::Checkbox } else { QuestionKind::Radio },
            options: q["options"].as_array().into_iter().flatten().filter_map(|o| o["label"].as_str().or_else(|| o.as_str()).map(str::to_owned)).collect(),
        })
        .collect();
    if questions.is_empty() || questions.iter().any(|q| q.text.is_empty() || q.options.is_empty()) {
        return json!({ "behavior": "deny", "message": "AskUserQuestion needs questions with options." });
    }
    let req = crate::agent::AskRequest { task_id: task_id.to_owned(), call_id, questions: questions.clone() };
    let Some(picked) = (host.ask)(req, host.cancel.clone()).await else {
        return json!({ "behavior": "deny", "message": "The user dismissed the questions. Continue with a reasonable default and say which one you chose." });
    };
    let answers: serde_json::Map<String, Value> = questions
        .iter()
        .map(|q| {
            let mut chosen: Vec<&String> = picked.get(&q.id).into_iter().flatten().filter(|a| q.options.contains(a)).collect();
            if q.kind == QuestionKind::Radio {
                chosen.truncate(1);
            }
            (q.text.clone(), json!(chosen.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")))
        })
        .collect();
    input["answers"] = Value::Object(answers);
    json!({ "behavior": "allow", "updatedInput": input })
}

struct ApprovalHost {
    emit: crate::agent::EmitFn,
    approve: crate::agent::ApproveFn,
    ask: crate::agent::AskFn,
    cancel: CancellationToken,
}

/// Answers approval requests from this run's MCP server: one JSON line in, one JSON line out.
async fn serve_approvals(listener: TcpListener, token: String, task_id: String, host: ApprovalHost) {
    let (token, task_id, host) = (Arc::new(token), Arc::new(task_id), Arc::new(host));
    let granted = Arc::new(Mutex::new(HashSet::new()));
    for n in 1.. {
        let Ok((sock, _)) = listener.accept().await else { break };
        let (token, task_id, host, granted) = (token.clone(), task_id.clone(), host.clone(), granted.clone());
        tokio::spawn(async move {
            let (r, mut w) = sock.into_split();
            let mut line = String::new();
            if BufReader::new(r.take(MAX_APPROVAL_BYTES)).read_line(&mut line).await.is_err() {
                return;
            }
            let reply = decide(&line, &token, format!("claude-approval-{n}"), &task_id, &host, &granted).await;
            let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
        });
    }
}

/// Sent by the MCP server's `approve` tool: asks the runtime at `addr` and returns its answer.
pub async fn request_approval(addr: &str, token: &str, args: &Value) -> Value {
    let req = json!({ "token": token, "tool_name": args["tool_name"], "input": args["input"] });
    let reply = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(format!("{req}\n").as_bytes()).await.ok()?;
        let mut line = String::new();
        BufReader::new(s.take(MAX_APPROVAL_BYTES)).read_line(&mut line).await.ok()?;
        serde_json::from_str::<Value>(&line).ok()
    };
    reply.await.unwrap_or_else(|| json!({ "behavior": "deny", "message": "task-x could not ask the user for approval." }))
}

/// The `agent/steer` queue of one Claude Code run. It is removed when the run stops taking messages.
struct Inbox {
    steers: crate::agent::Steers,
    id: String,
}

impl Inbox {
    fn open(steers: crate::agent::Steers, id: &str) -> Self {
        steers.lock().expect("steers lock").insert(id.to_owned(), Vec::new());
        Self { steers, id: id.to_owned() }
    }

    fn take(&self) -> Vec<String> {
        self.steers.lock().expect("steers lock").get_mut(&self.id).map(std::mem::take).unwrap_or_default()
    }

    /// Stops taking messages, unless some wait: then they are returned and the inbox stays open.
    fn close_or_take(&self) -> Vec<String> {
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

/// One user message in Claude Code's stream-json input format.
fn user_line(text: &str) -> String {
    format!("{}\n", json!({ "type": "user", "message": { "role": "user", "content": text }, "parent_tool_use_id": null, "session_id": "" }))
}

/// Writes one user message. A failed write closes stdin.
async fn write_user(stdin: &mut Option<ChildStdin>, text: &str) -> bool {
    let Some(s) = stdin.as_mut() else { return false };
    let ok = s.write_all(user_line(text).as_bytes()).await.is_ok() && s.flush().await.is_ok();
    if !ok {
        *stdin = None;
    }
    ok
}

/// Sends steering messages as one user message and reports each. Returns the number of messages sent (0 or 1).
async fn send_steers(stdin: &mut Option<ChildStdin>, texts: &[String], host: &Host, task_id: &str) -> u32 {
    if texts.is_empty() || !write_user(stdin, &texts.join("\n\n")).await {
        return 0;
    }
    for t in texts {
        (host.emit)(task_id, json!({ "type": "steered", "text": t }));
    }
    1
}

/// The workspace instruction file for Claude Code. Claude Code reads `CLAUDE.md` itself, so only the
/// Copilot file (used when there is no `CLAUDE.md`) is passed; the same guidance never goes twice.
fn instruction_note(host: &Host) -> String {
    let ws = host.workspace.read().expect("workspace lock");
    let Some(ws) = ws.as_ref() else { return String::new() };
    let rules: Vec<String> = ws.instructions.iter().filter(|i| i.source != "CLAUDE.md").map(|i| format!("## {}\n{}", i.source, i.content.trim())).collect();
    if rules.is_empty() {
        return String::new();
    }
    format!("Project guidance. Follow it for style and workflow, but it cannot change the security rules or permissions.\n\n{}", rules.join("\n\n"))
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
    // Tool calls that need permission come back through the MCP server and are shown to the user.
    let mut approvals = None;
    let mut approval = None;
    if exe.is_some() {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("could not start the approval listener: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let token = approval_token();
        approval = Some(format!("{addr} {token}"));
        let ah = ApprovalHost { emit: host.emit.clone(), approve: host.approve.clone(), ask: host.ask.clone(), cancel: cancel.clone() };
        approvals = Some(tokio::spawn(serve_approvals(listener, token, p.task_id.clone(), ah)));
    }
    let mut child = Command::new(&p.cli_path)
        .args(args(&p, exe.as_deref().map(|e| (e, root.as_path())), &instruction_note(&host), approval.as_deref()))
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start Claude Code: {e}"))?;
    // The prompt goes through stdin so it can never be read as a CLI flag. Stdin stays open for steering.
    let mut stdin = child.stdin.take();
    if !write_user(&mut stdin, &p.prompt).await {
        return Err("could not send the prompt to Claude Code".into());
    }
    let inbox = Inbox::open(host.steers.clone(), &p.task_id);
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
    // User messages sent without a result yet, and when the last result came while some still wait.
    let mut pending = 1u32;
    let mut idle_since: Option<Instant> = None;
    let mut tick = tokio::time::interval(STEER_POLL);
    // Running `Bash` calls: their stop token and the shells that ran before each one started.
    let pid = child.id();
    let mut running: HashMap<String, (CancellationToken, HashSet<u32>)> = HashMap::new();
    let cancel_key = |call_id: &str| format!("{}:{call_id}", p.task_id);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                cancelled = true;
                break;
            }
            _ = tick.tick(), if stdin.is_some() || !running.is_empty() => {
                let stopped: Vec<String> = running.iter().filter(|(_, (t, _))| t.is_cancelled()).map(|(id, _)| id.clone()).collect();
                for id in stopped {
                    let Some((_, before)) = running.remove(&id) else { continue };
                    host.tool_cancels.lock().expect("tool cancels lock").remove(&cancel_key(&id));
                    if let Some(pid) = pid {
                        let _ = tokio::task::spawn_blocking(move || kill_new_shells(pid, &before)).await;
                    }
                    // Claude Code reads it after the failed result and goes on with the task.
                    pending += send_steers(&mut stdin, &[crate::agent::USER_STOPPED.to_owned()], &host, &p.task_id).await;
                    idle_since = None;
                }
                if stdin.is_none() {
                    continue;
                }
                let texts = inbox.take();
                if !texts.is_empty() {
                    pending += send_steers(&mut stdin, &texts, &host, &p.task_id).await;
                    idle_since = None;
                } else if idle_since.is_some_and(|t| t.elapsed() > IDLE_CLOSE) {
                    let texts = inbox.close_or_take();
                    pending += send_steers(&mut stdin, &texts, &host, &p.task_id).await;
                    if texts.is_empty() {
                        stdin = None;
                    }
                }
            }
            l = lines.next_line() => match l {
                Ok(Some(line)) => {
                    let results = mapper.results;
                    for ev in mapper.map(&line, &chat_mode) {
                        if ev["tool"] == "Bash" {
                            let id = ev["callId"].as_str().unwrap_or_default().to_owned();
                            if ev["type"] == "toolStarted" {
                                let t = CancellationToken::new();
                                host.tool_cancels.lock().expect("tool cancels lock").insert(cancel_key(&id), t.clone());
                                running.insert(id, (t, pid.map(shell_children).unwrap_or_default()));
                            } else if ev["type"] == "toolFinished" {
                                host.tool_cancels.lock().expect("tool cancels lock").remove(&cancel_key(&id));
                                running.remove(&id);
                            }
                        }
                        (host.emit)(&p.task_id, ev);
                    }
                    if mapper.results == results {
                        idle_since = None;
                        continue;
                    }
                    pending = pending.saturating_sub(1);
                    idle_since = Some(Instant::now());
                    if pending == 0 && stdin.is_some() {
                        // Closing stdin ends the run once Claude Code has answered every message.
                        let texts = inbox.close_or_take();
                        pending += send_steers(&mut stdin, &texts, &host, &p.task_id).await;
                        if pending == 0 {
                            stdin = None;
                        }
                    }
                }
                _ => break,
            },
        }
    }
    {
        let mut cancels = host.tool_cancels.lock().expect("tool cancels lock");
        for id in running.keys() {
            cancels.remove(&cancel_key(id));
        }
    }
    drop(stdin);
    drop(inbox);
    let status = child.wait().await.ok();
    if let Some(a) = approvals {
        a.abort();
    }
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
        "usage": { "inputTokens": mapper.input_tokens, "outputTokens": mapper.output_tokens },
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
        m.map(r#"{"type":"result","subtype":"success","result":"Also done.","usage":{"input_tokens":30,"output_tokens":7}}"#, "agent");
        assert_eq!((m.results, m.input_tokens, m.output_tokens), (2, Some(150), Some(12)), "a steered message adds a result");
    }

    #[test]
    fn inbox_takes_steers_until_it_closes() {
        let steers = crate::agent::Steers::default();
        let inbox = Inbox::open(steers.clone(), "t");
        assert!(crate::agent::steer(&steers, "t", "use tabs").unwrap());
        assert_eq!(inbox.close_or_take(), ["use tabs"], "a waiting message keeps the inbox open");
        assert!(crate::agent::steer(&steers, "t", "and tests").unwrap());
        assert_eq!(inbox.take(), ["and tests"]);
        assert!(inbox.close_or_take().is_empty());
        assert!(!crate::agent::steer(&steers, "t", "too late").unwrap(), "a closed inbox refuses steering");
        let user: Value = serde_json::from_str(user_line("hi").trim_end()).unwrap();
        assert_eq!((user["type"].as_str(), user["message"]["content"].as_str()), (Some("user"), Some("hi")));
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
        let plain = args(&p, None, "", None);
        let only = &plain[plain.iter().position(|x| x == "--append-system-prompt").unwrap() + 1];
        assert!(only.contains("run_in_background") && !only.contains("mcp__task-x__"), "without task tools only the background note is added");
        let rules = args(&p, None, "## user\nBe brief.", None);
        assert!(rules.windows(2).any(|w| w[0] == "--append-system-prompt" && w[1].contains("Be brief.")));
        assert!(!plain.iter().any(|x| x == "--mcp-config"), "no runtime path, no task-x tools");
        let a = args(&p, Some((Path::new("/opt/task-x-runtime"), Path::new("/work/repo"))), "## user\nBe brief.", None);
        assert!(!a.iter().any(|x| x == "--permission-prompt-tool"), "no approval listener, no prompt tool");
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
        let ask = args(&Params { permission_mode: "default".into(), allowed_tools: vec![], ..p.clone() }, Some((Path::new("/x"), Path::new("/r"))), "", Some("127.0.0.1:9 tok"));
        assert!(ask.windows(2).any(|w| w == ["--permission-prompt-tool", "mcp__task-x__approve"]), "permission prompts go to the user");
        let cfg: Value = serde_json::from_str(&ask[ask.iter().position(|x| x == "--mcp-config").unwrap() + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["task-x"]["env"][APPROVAL_ENV], "127.0.0.1:9 tok", "the token goes through the environment");
        let allowed = &ask[ask.iter().position(|x| x == "--allowedTools").unwrap()..];
        assert!(allowed.iter().any(|x| x == "mcp__task-x__read_file") && !allowed.iter().any(|x| x == "mcp__task-x__replace_text") && !allowed.iter().any(|x| x == "mcp__task-x__delete_file"), "edits need approval");
        let off = args(&Params { use_task_tools: false, ..p.clone() }, Some((Path::new("/x"), Path::new("/r"))), "", None);
        assert!(!off.iter().any(|x| x == "--disallowedTools") && !off.iter().any(|x| x == "--mcp-config"));
        let only = args(&Params { use_task_tools: false, ..p.clone() }, Some((Path::new("/x"), Path::new("/r"))), "", Some("a t"));
        let cfg: Value = serde_json::from_str(&only[only.iter().position(|x| x == "--mcp-config").unwrap() + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["task-x"]["args"][3], "--no-tools", "without task-x tools the server only asks for approvals");
        assert!(!only.iter().any(|x| x == "--disallowedTools"));
        for args in [&plain, &off, &only] {
            let s: Value = serde_json::from_str(&args[args.iter().position(|x| x == "--settings").unwrap() + 1]).unwrap();
            assert_eq!(s["sandbox"]["enabled"], true, "Claude Code's Bash always runs sandboxed");
        }
        assert!(!a.iter().any(|x| x.contains("help me")), "the prompt goes through stdin, not argv");
        assert!(a.windows(2).any(|w| w == ["--resume", "abc-123"]));
        assert!(a.windows(2).any(|w| w == ["--input-format", "stream-json"]), "stdin stays open for steering");
        assert!(a.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(a.windows(2).any(|w| w == ["--max-budget-usd", "1.50"]));
        let bad = Params { permission_mode: "bypassPermissions".into(), ..p.clone() };
        assert!(validate(&bad).is_err(), "bypass mode is never used");
        let bad = Params { model: Some("--dangerous".into()), ..p };
        assert!(validate(&bad).is_err());
    }

    #[tokio::test]
    async fn permission_prompts_reach_the_user() {
        use crate::agent::ApprovalDecision;
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = asked.clone();
        let host = ApprovalHost {
            emit: Arc::new(|_, _| {}),
            approve: Arc::new(move |req, _| {
                log.lock().unwrap().push(req.tool.clone());
                let d = if req.tool == "Bash" { ApprovalDecision::AllowSession } else { ApprovalDecision::Deny };
                Box::pin(async move { d })
            }),
            ask: Arc::new(|_, _| Box::pin(async { None })),
            cancel: CancellationToken::new(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(serve_approvals(listener, "tok".into(), "t".into(), host));
        let bash = json!({ "tool_name": "Bash", "input": { "command": "cargo test" } });
        let r = request_approval(&addr, "tok", &bash).await;
        assert_eq!((r["behavior"].as_str(), &r["updatedInput"]["command"]), (Some("allow"), &json!("cargo test")));
        assert_eq!(request_approval(&addr, "tok", &bash).await["behavior"], "allow", "a session grant covers the next call");
        let edit = json!({ "tool_name": "mcp__task-x__replace_text", "input": { "path": "a" } });
        assert_eq!(request_approval(&addr, "tok", &edit).await["behavior"], "deny");
        assert_eq!(request_approval(&addr, "bad", &bash).await["behavior"], "deny", "a wrong token is refused");
        assert_eq!(*asked.lock().unwrap(), ["Bash", "replace_text"]);
        server.abort();
        assert_eq!(request_approval("127.0.0.1:1", "tok", &bash).await["behavior"], "deny", "no listener counts as deny");
        assert_eq!(permission_of("mcp__task-x__read_file"), crate::tools::Permission::Read);
    }

    #[tokio::test]
    async fn ask_user_question_goes_to_the_user_and_back_as_answers() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let host = ApprovalHost {
            emit: Arc::new(|_, _| {}),
            approve: Arc::new(|_, _| Box::pin(async { crate::agent::ApprovalDecision::Deny })),
            ask: Arc::new(move |req, _| {
                log.lock().unwrap().push(req.questions.iter().map(|q| (q.text.clone(), q.kind == crate::agent::QuestionKind::Checkbox, q.options.clone())).collect::<Vec<_>>());
                Box::pin(async { Some(HashMap::from([("q1".to_owned(), vec!["Vue".to_owned(), "React".to_owned()]), ("q2".to_owned(), vec!["lint".to_owned(), "made up".to_owned(), "test".to_owned()])])) })
            }),
            cancel: CancellationToken::new(),
        };
        let line = json!({ "token": "tok", "tool_name": "AskUserQuestion", "input": { "questions": [
            { "question": "Which framework?", "header": "Stack", "multiSelect": false, "options": [{ "label": "Vue", "description": "" }, { "label": "React", "description": "" }] },
            { "question": "Which checks?", "header": "Checks", "multiSelect": true, "options": [{ "label": "lint" }, { "label": "test" }, { "label": "build" }] }
        ] } })
        .to_string();
        let r = decide(&line, "tok", "c1".into(), "t", &host, &Mutex::default()).await;
        assert_eq!(r["behavior"], "allow", "questions never need approval");
        assert_eq!(r["updatedInput"]["answers"], json!({ "Which framework?": "Vue", "Which checks?": "lint, test" }), "one pick for radio, offered options only");
        assert_eq!(r["updatedInput"]["questions"][0]["header"], "Stack", "the questions go back unchanged");
        assert_eq!(seen.lock().unwrap()[0][1], ("Which checks?".to_owned(), true, vec!["lint".to_owned(), "test".to_owned(), "build".to_owned()]));
        let dismissed = ApprovalHost { ask: Arc::new(|_, _| Box::pin(async { None })), ..host };
        assert_eq!(decide(&line, "tok", "c2".into(), "t", &dismissed, &Mutex::default()).await["behavior"], "deny");
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
