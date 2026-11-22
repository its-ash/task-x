//! Antigravity CLI engine: runs `agy` (Google's Gemini coding agent) headless in the workspace and
//! maps its stream-json output onto the same `agent/event` stream as the built-in agent. Like the
//! Claude Code engine, file changes are found by comparing indexed contents before and after the
//! run and recorded in the edit log, so they can be reviewed, kept, or undone.
//!
//! `agy` protocol (print mode):
//! - flags first, then `-p=` (a bare `-p` takes the next flag as its prompt);
//! - `--input-format stream-json` reads one `{"event":"user","message":{"role":"user","content":"…"}}`
//!   line per turn from stdin; `--output-format stream-json` writes `init`, `step_update`, and
//!   `result` events;
//! - headless mode cannot ask for permission: a tool that needs it is denied and listed in the
//!   result's `denied_actions`;
//! - `-p=/usage` and `-p=/credits` with `--output-format json` return the quota in `command.data`.

use crate::agent::store;
use crate::cli_common::{Host, Inbox, ProcTracker, clip, record_changes, save_turn, snap, snapshot};
use crate::provider::{ContentPart, Message, Role};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio_util::sync::CancellationToken;

/// The record's `providerKind` and the summary's `engine`.
pub const KIND: &str = "antigravity";
/// `default`: agy's own mode (edits are denied headless); the others are agy `--mode` values.
const PERMISSION_MODES: &[&str] = &["default", "accept-edits", "auto"];
const MAX_TOOL_OUTPUT: usize = 120_000;
const MAX_RECORD_OUTPUT: usize = 20_000;
const STEER_POLL: Duration = Duration::from_millis(250);
/// After stdin closes, agy that has not exited by then is stopped.
const EXIT_GRACE: Duration = Duration::from_secs(30);
/// Time limit of `agy models`, `/usage`, and `/credits`.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub prompt: String,
    pub task_id: String,
    /// Absolute path of the `agy` executable.
    pub cli_path: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    /// agy conversation to resume (`conversationId` of an earlier run's result).
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub chat_mode: Option<String>,
    /// `default`, `accept-edits`, or `auto`. Chat modes other than agent run `--mode plan`.
    #[serde(default = "default_mode")]
    pub permission_mode: String,
    /// `--dangerously-skip-permissions`: every tool runs without asking.
    #[serde(default)]
    pub skip_permissions: bool,
    #[serde(default)]
    pub effort: Option<crate::provider::types::Effort>,
    /// `--sandbox=true` / `--sandbox=false`: syncs agy's terminal restrictions with the extension setting.
    #[serde(default)]
    pub sandbox: Option<bool>,
    /// Images attached to the prompt; saved to a per-run temp dir passed via `--add-dir`.
    #[serde(default)]
    pub images: Vec<crate::provider::types::ImageInput>,
}

fn default_mode() -> String {
    "default".into()
}

/// A value passed as a CLI argument: never empty, never read as a flag.
fn valid_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:()*/ ".contains(c))
}

pub fn validate(p: &Params) -> Result<(), String> {
    if p.prompt.trim().is_empty() {
        return Err("prompt is empty".into());
    }
    if !p.cli_path.is_absolute() {
        return Err("cliPath must be an absolute path".into());
    }
    if !PERMISSION_MODES.contains(&p.permission_mode.as_str()) {
        return Err(format!("permissionMode must be one of {}", PERMISSION_MODES.join(", ")));
    }
    if p.model.as_deref().is_some_and(|m| !valid_token(m)) {
        return Err("invalid model".into());
    }
    if p.conversation_id.as_deref().is_some_and(|c| !valid_token(c)) {
        return Err("invalid conversationId".into());
    }
    crate::provider::types::image_parts(&p.images)?;
    Ok(())
}

/// The agy `--mode` of a run: chat modes other than agent (ask, plan) only plan; agent mode uses
/// the permission mode.
fn mode(p: &Params) -> Option<&str> {
    if p.chat_mode.as_deref().is_some_and(|m| m != "agent") {
        return Some("plan");
    }
    (p.permission_mode != "default").then_some(p.permission_mode.as_str())
}

/// Returns true when a model id already encodes an effort tier (e.g. `gemini-4-neon-high`).
/// Passing `--effort` alongside such a model makes `agy` fail with a conflict error.
fn model_has_effort(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    ["-low", "-medium", "-high", "-xhigh", "-max"].iter().any(|s| lower.ends_with(s))
}

/// Command-line arguments. The prompt goes through stdin, so it can never be read as a flag.
pub fn args(p: &Params) -> Vec<String> {
    let mut a: Vec<String> = ["--input-format", "stream-json", "--output-format", "stream-json"].map(String::from).to_vec();
    if let Some(m) = &p.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(c) = &p.conversation_id {
        a.extend(["--conversation".into(), c.clone()]);
    }
    if let Some(m) = mode(p) {
        a.extend(["--mode".into(), m.into()]);
    }
    if let Some(e) = p.effort {
        if p.model.as_deref().is_none_or(|m| !model_has_effort(m)) {
            a.extend(["--effort".into(), e.as_str().into()]);
        }
    }
    if let Some(sandbox) = p.sandbox {
        a.push(if sandbox { "--sandbox=true".into() } else { "--sandbox=false".into() });
    }
    if p.skip_permissions && mode(p) != Some("plan") {
        a.push("--dangerously-skip-permissions".into());
    }
    // Last, with `=`: a bare `-p` would take the next argument as its prompt.
    a.push("-p=".into());
    a
}

fn user_line(text: &str) -> String {
    format!("{}\n", json!({ "event": "user", "message": { "role": "user", "content": text } }))
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

async fn send_steers(stdin: &mut Option<ChildStdin>, texts: &[String], host: &Host, task_id: &str) -> u32 {
    if texts.is_empty() || !write_user(stdin, &texts.join("\n\n")).await {
        return 0;
    }
    for t in texts {
        (host.emit)(task_id, json!({ "type": "steered", "text": t }));
    }
    1
}

/// Turns agy's stream-json lines into `agent/event`s and the transcript.
#[derive(Default)]
pub struct Mapper {
    pub conversation_id: Option<String>,
    pub model: Option<String>,
    /// The last `result`, and how many came.
    pub result: Option<Value>,
    pub results: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub tool_calls: u32,
    pub turns: u64,
    pub transcript: Vec<Message>,
    /// Text of the reply steps (`step_index` → text), saved when the step is done.
    replies: HashMap<u64, String>,
    /// Tool steps already reported as started.
    tools: HashSet<u64>,
    /// Actions denied this run (headless mode cannot ask), shown once each.
    denied: HashSet<String>,
}

fn add(sum: &mut Option<u64>, n: Option<u64>) {
    if let Some(n) = n {
        *sum = Some(sum.unwrap_or(0) + n);
    }
}

impl Mapper {
    fn note(&mut self, role: Role, part: ContentPart) {
        match self.transcript.last_mut() {
            Some(m) if m.role == role => m.content.push(part),
            _ => self.transcript.push(Message { role, content: vec![part] }),
        }
    }

    pub fn map(&mut self, line: &str, chat_mode: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(line) else { return out };
        match v["event"].as_str() {
            Some("init") => {
                self.conversation_id = v["conversation_id"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
                self.model = v["init"]["model"].as_str().map(str::to_owned);
                out.push(json!({
                    "type": "started",
                    "chatMode": chat_mode,
                    "engine": KIND,
                    "enrichment": { "kind": "Antigravity", "plan": [], "instructions": [], "sources": [], "ambiguity": null },
                    "tools": v["init"]["tools"],
                }));
                if let Some(m) = &self.model {
                    out.push(json!({ "type": "model", "kind": KIND, "model": m, "reason": "Antigravity CLI" }));
                }
            }
            Some("step_update") => out.extend(self.step(&v["step_update"])),
            Some("result") => {
                let r = &v["result"];
                if let Some(c) = r["conversation_id"].as_str().filter(|s| !s.is_empty()) {
                    self.conversation_id = Some(c.to_owned());
                }
                let u = &r["usage"];
                let (in_tok, out_tok, cache_tok) = (u["input_tokens"].as_u64(), u["output_tokens"].as_u64(), u["cache_read_tokens"].as_u64());
                add(&mut self.input_tokens, in_tok);
                add(&mut self.output_tokens, out_tok);
                add(&mut self.cached_tokens, cache_tok);
                if in_tok.is_some() || out_tok.is_some() {
                    out.push(json!({
                        "type": "usage",
                        "usage": {
                            "inputTokens": self.input_tokens,
                            "outputTokens": self.output_tokens,
                            "cachedTokens": self.cached_tokens,
                        },
                    }));
                }
                if let Some(resp) = r["response"].as_str().filter(|t| !t.trim().is_empty()) {
                    let has_text = self.transcript.last().is_some_and(|m| {
                        m.role == Role::Assistant && m.content.iter().any(|p| matches!(p, ContentPart::Text { text } if !text.trim().is_empty()))
                    });
                    if !has_text {
                        out.push(json!({ "type": "assistantMessage", "text": resp }));
                        self.note(Role::Assistant, ContentPart::Text { text: resp.to_owned() });
                    }
                }
                self.turns += r["num_turns"].as_u64().unwrap_or(0);
                for d in r["denied_actions"].as_array().into_iter().flatten() {
                    let action = d["action"].as_str().unwrap_or("a tool").to_owned();
                    if self.denied.insert(action.clone()) {
                        let name = d["display_name"].as_str().unwrap_or(&action);
                        out.push(json!({
                            "type": "warning",
                            "message": format!("Antigravity was not allowed to use {name} ({action}): headless mode cannot ask. Use Auto mode (and turn Sandbox off for unsandboxed commands) to allow it."),
                        }));
                    }
                }
                self.result = Some(r.clone());
                self.results += 1;
            }
            _ => {}
        }
        out
    }

    fn step(&mut self, s: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        let idx = s["step_index"].as_u64().unwrap_or(0);
        let state = s["state"].as_str().unwrap_or_default();
        let done = matches!(state, "DONE" | "ERROR" | "CANCELLED");
        match s["step_type"].as_str() {
            Some("agent_response") => {
                if let Some(t) = s["text_delta"].as_str().filter(|t| !t.is_empty()) {
                    self.replies.entry(idx).or_default().push_str(t);
                    out.push(json!({ "type": "textDelta", "text": t }));
                }
                if done {
                    let streamed = self.replies.remove(&idx).filter(|t| !t.trim().is_empty());
                    if let Some(text) = streamed {
                        self.note(Role::Assistant, ContentPart::Text { text });
                    } else if let Some(full) = s["text"].as_str().filter(|t| !t.trim().is_empty()) {
                        out.push(json!({ "type": "assistantMessage", "text": full }));
                        self.note(Role::Assistant, ContentPart::Text { text: full.to_owned() });
                    }
                }
            }
            Some("tool") => {
                let info = &s["tool_info"];
                let name = s["tool_name"].as_str().or(info["name"].as_str()).unwrap_or("tool").to_owned();
                let id = format!("agy-{idx}");
                if self.tools.insert(idx) {
                    self.tool_calls += 1;
                    let input = info["parameters"].clone();
                    out.push(json!({ "type": "toolStarted", "callId": id, "tool": name, "input": input, "permission": KIND }));
                    self.note(Role::Assistant, ContentPart::ToolUse { id: id.clone(), name: name.clone(), input });
                }
                if done {
                    let ok = state == "DONE";
                    let text = if ok {
                        match &info["output"] {
                            Value::String(o) => o.clone(),
                            Value::Null => String::new(),
                            o => o.to_string(),
                        }
                    } else {
                        info["error"]["message"].as_str().unwrap_or(if state == "CANCELLED" { "cancelled" } else { "failed" }).to_owned()
                    };
                    out.push(json!({ "type": "toolFinished", "callId": id, "tool": name, "ok": ok, "output": clip(&text, MAX_TOOL_OUTPUT) }));
                    self.note(Role::User, ContentPart::ToolResult { tool_use_id: id, tool_name: Some(name), content: clip(&text, MAX_RECORD_OUTPUT), is_error: !ok });
                }
            }
            _ => {}
        }
        out
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("invalid base64 image data".into()),
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

fn image_ext(media_type: &str) -> &'static str {
    match media_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes attached images to a per-run temp directory so `agy` can read them with `read_file`.
fn save_attached_images(images: &[crate::provider::types::ImageInput]) -> Result<Option<(TempDirGuard, Vec<PathBuf>)>, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if images.is_empty() {
        return Ok(None);
    }
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("task-x-agy-img-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create image temp dir: {e}"))?;
    let guard = TempDirGuard(dir.clone());
    let mut paths = Vec::with_capacity(images.len());
    for (i, img) in images.iter().enumerate() {
        let bytes = decode_base64(&img.data)?;
        let file = dir.join(format!("attached-{}.{}", i + 1, image_ext(&img.media_type)));
        std::fs::write(&file, &bytes).map_err(|e| format!("could not write attached image: {e}"))?;
        paths.push(file);
    }
    Ok(Some((guard, paths)))
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
    snap(&root, &p.task_id, format!("Before: {}", p.prompt.trim()), &host).await;
    let started = Instant::now();
    let started_ms = store::now_ms();
    let saved_images = save_attached_images(&p.images)?;
    let mut cli_args = args(&p);
    let mut prompt = p.prompt.clone();
    if let Some((ref guard, ref paths)) = saved_images {
        // Insert `--add-dir <dir>` before `-p=` (the last argument) so `agy` can read the images.
        let pos = cli_args.len().saturating_sub(1);
        cli_args.splice(pos..pos, ["--add-dir".to_owned(), guard.0.to_string_lossy().into_owned()]);
        prompt.push_str("\n\nAttached image(s) saved locally (use read_file to view them):");
        for path in paths {
            prompt.push_str(&format!("\n- {}", path.display()));
        }
    }
    let mut child = Command::new(&p.cli_path)
        .args(&cli_args)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start Antigravity: {e}"))?;
    let mut stdin = child.stdin.take();
    if !write_user(&mut stdin, &prompt).await {
        return Err("could not send the prompt to Antigravity".into());
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
    let mut proc_tracker = ProcTracker::new(&p.task_id, &root);
    let chat_mode = p.chat_mode.clone().unwrap_or_else(|| "agent".into());
    let mut cancelled = false;
    let mut stopped_after_end = false;
    // User messages sent without a result yet.
    let mut pending = 1u32;
    let mut closed_at: Option<Instant> = None;
    let mut tick = tokio::time::interval(STEER_POLL);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                cancelled = true;
                break;
            }
            _ = tick.tick(), if stdin.is_some() || closed_at.is_some() => {
                if closed_at.is_some_and(|t| t.elapsed() > EXIT_GRACE) {
                    closed_at = None;
                    stopped_after_end = true;
                    (host.emit)(&p.task_id, json!({ "type": "warning", "message": "Antigravity did not exit after the task ended; task-x stopped it." }));
                    let _ = child.start_kill();
                }
                let texts = inbox.take();
                pending += send_steers(&mut stdin, &texts, &host, &p.task_id).await;
            }
            l = lines.next_line() => match l {
                Ok(Some(line)) => {
                    let results = mapper.results;
                    for ev in mapper.map(&line, &chat_mode) {
                        proc_tracker.observe(&ev);
                        (host.emit)(&p.task_id, ev);
                    }
                    if mapper.results == results {
                        continue;
                    }
                    pending = pending.saturating_sub(1);
                    if pending == 0 && stdin.is_some() {
                        // Every message has its answer: closing stdin ends the run, unless more wait.
                        let texts = inbox.close_or_take();
                        pending += send_steers(&mut stdin, &texts, &host, &p.task_id).await;
                        if pending == 0 {
                            stdin = None;
                            closed_at = Some(Instant::now());
                        }
                    }
                }
                _ => break,
            },
        }
    }
    drop(stdin);
    drop(inbox);
    let status = child.wait().await.ok();
    let stderr_text = stderr_task.await.unwrap_or_default();

    let (changed, operations) = record_changes(&host, &p.task_id, &before, "agy_cli").await?;

    let r = mapper.result.clone().unwrap_or(Value::Null);
    let status_name = if cancelled {
        "cancelled"
    } else if r.is_null() || r["status"] == "ERROR" || (!stopped_after_end && status.is_some_and(|s| !s.success())) {
        "failed"
    } else {
        "completed"
    };
    snap(&root, &p.task_id, format!("End: {status_name}"), &host).await;
    let error = (status_name == "failed").then(|| {
        let msg = r["error"]
            .as_str()
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| stderr_text.trim().lines().last().unwrap_or("Antigravity exited with an error").to_owned());
        clip(&msg, 2_000)
    });
    let summary = json!({
        "taskId": p.task_id,
        "status": status_name,
        "chatMode": chat_mode,
        "engine": KIND,
        "text": r["response"].as_str().unwrap_or_default(),
        "changedFiles": changed,
        "operations": operations,
        "validations": [],
        "validationRan": false,
        "skippedChecks": [],
        "usage": { "inputTokens": mapper.input_tokens, "outputTokens": mapper.output_tokens, "cachedTokens": mapper.cached_tokens },
        "metrics": { "modelMs": started.elapsed().as_millis() as u64, "costUsd": null },
        "model": mapper.model,
        "iterations": mapper.turns,
        "toolCalls": mapper.tool_calls,
        "error": error,
        "conversationId": mapper.conversation_id,
    });
    if let Err(e) = save_turn(&host.store, &p.task_id, &p.prompt, KIND, started_ms, std::mem::take(&mut mapper.transcript), &summary) {
        (host.emit)(&p.task_id, json!({ "type": "warning", "message": format!("The chat could not be saved: {e}") }));
    }
    (host.emit)(&p.task_id, json!({ "type": "done", "summary": summary }));
    Ok(summary)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryParams {
    pub cli_path: PathBuf,
}

/// Runs `agy` with `args` and returns its stdout. `cwd` is any folder (the workspace when open).
async fn query(cli: &Path, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    if !cli.is_absolute() {
        return Err("cliPath must be an absolute path".into());
    }
    let mut cmd = Command::new(cli);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = tokio::time::timeout(QUERY_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "Antigravity did not answer in time".to_owned())?
        .map_err(|e| format!("could not start Antigravity: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() && stdout.trim().is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(clip(err.trim().lines().last().unwrap_or("Antigravity exited with an error"), 500));
    }
    Ok(stdout)
}

/// The `command.data` of a slash command run with `--output-format json`.
fn command_data(out: &str) -> Result<Value, String> {
    let v: Value = out.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).ok_or("Antigravity returned no JSON")?;
    if v["status"] == "ERROR" {
        return Err(v["error"].as_str().unwrap_or("Antigravity reported an error").to_owned());
    }
    Ok(v["command"]["data"].clone())
}

/// `/usage` data in the protocol's camelCase shape.
pub fn usage_from(data: &Value) -> Value {
    let groups: Vec<Value> = data["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|g| {
            let buckets: Vec<Value> = g["buckets"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| {
                    json!({
                        "id": b["id"],
                        "name": b["name"],
                        "window": b["window"],
                        "remainingFraction": b["remaining_fraction"],
                        "resetTime": b["reset_time"],
                        "description": b["description"],
                    })
                })
                .collect();
            json!({ "name": g["name"], "description": g["description"], "buckets": buckets })
        })
        .collect();
    json!({ "description": data["description"], "groups": groups })
}

/// `/credits` data in the protocol's shape; null when agy sent none.
pub fn credits_from(data: &Value) -> Value {
    if data.is_null() {
        return Value::Null;
    }
    json!({ "remaining": data["remaining_credits"], "upgradeUri": data["upgrade_uri"] })
}

/// The quota: shared 5-hour and weekly limits per model group, and the AI credits left.
pub async fn usage(p: QueryParams, cwd: Option<PathBuf>) -> Result<Value, String> {
    let (u, c) = tokio::join!(
        query(&p.cli_path, &["--output-format", "json", "-p=/usage"], cwd.as_deref()),
        query(&p.cli_path, &["--output-format", "json", "-p=/credits"], cwd.as_deref()),
    );
    let mut out = usage_from(&command_data(&u?)?);
    // Credits are extra: the quota still shows when they cannot be read.
    out["credits"] = c.and_then(|c| command_data(&c)).map(|d| credits_from(&d)).unwrap_or(Value::Null);
    Ok(out)
}

/// `agy models` output: one `id<TAB>label` per line.
pub fn parse_models(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|l| {
            let (id, label) = l.split_once('\t')?;
            let id = id.trim();
            valid_token(id).then(|| json!({ "id": id, "label": label.trim() }))
        })
        .collect()
}

pub async fn models(p: QueryParams) -> Result<Value, String> {
    let out = query(&p.cli_path, &["models"], None).await?;
    Ok(json!({ "models": parse_models(&out) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        serde_json::from_value(json!({ "prompt": "hi", "taskId": "t1", "cliPath": "/usr/local/bin/agy" })).unwrap()
    }

    #[test]
    fn args_put_the_prompt_flag_last_and_map_modes() {
        let mut p = params();
        assert_eq!(args(&p), ["--input-format", "stream-json", "--output-format", "stream-json", "-p="]);
        p.model = Some("gemini-4-neon".into());
        p.conversation_id = Some("c1".into());
        p.permission_mode = "accept-edits".into();
        p.effort = Some(crate::provider::types::Effort::High);
        let a = args(&p);
        assert_eq!(a.last().unwrap(), "-p=");
        let s = a.join(" ");
        assert!(s.contains("--model gemini-4-neon"), "{s}");
        assert!(s.contains("--conversation c1"), "{s}");
        assert!(s.contains("--mode accept-edits"), "{s}");
        assert!(s.contains("--effort high"), "{s}");
        // Models whose id already encodes an effort suffix must not also pass `--effort`.
        p.model = Some("gemini-4-neon-high".into());
        p.effort = Some(crate::provider::types::Effort::Medium);
        let s = args(&p).join(" ");
        assert!(s.contains("--model gemini-4-neon-high") && !s.contains("--effort"), "{s}");
        p.chat_mode = Some("agent".into());
        assert!(args(&p).join(" ").contains("--mode accept-edits"));
        p.chat_mode = Some("ask".into());
        p.skip_permissions = true;
        let s = args(&p).join(" ");
        assert!(s.contains("--mode plan") && !s.contains("dangerously"), "{s}");
        p.sandbox = Some(true);
        assert!(args(&p).contains(&"--sandbox=true".to_string()));
        p.sandbox = Some(false);
        assert!(args(&p).contains(&"--sandbox=false".to_string()));
    }

    #[test]
    fn validate_rejects_flags_and_bad_modes() {
        let mut p = params();
        assert!(validate(&p).is_ok());
        p.model = Some("--dangerously-skip-permissions".into());
        assert!(validate(&p).is_err());
        p.model = None;
        p.permission_mode = "plan".into();
        assert!(validate(&p).is_err());
        p.permission_mode = "auto".into();
        p.cli_path = "agy".into();
        assert!(validate(&p).is_err());
    }

    #[test]
    fn user_line_is_the_stream_json_input_shape() {
        let v: Value = serde_json::from_str(user_line("do \"it\"").trim()).unwrap();
        assert_eq!(v, json!({ "event": "user", "message": { "role": "user", "content": "do \"it\"" } }));
    }

    #[test]
    fn maps_stream_json_to_agent_events() {
        let mut m = Mapper::default();
        let lines = [
            r#"{"event":"init","conversation_id":"c1","init":{"model":"gemini-3.8-flash-low","cwd":"/w","tools":["view_file","write_to_file"]}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":0,"state":"DONE","step_type":"user_input"}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"Path":"/w/a.txt"}}}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":2,"state":"DONE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"Path":"/w/a.txt"},"output":"2 lines"}}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":3,"state":"ERROR","step_type":"tool","tool_name":"write_file","tool_info":{"name":"write_file","parameters":{"Path":"/w/b.txt"},"error":{"type":"TOOL_ERROR","message":"user denied permission"}}}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":4,"state":"ACTIVE","step_type":"agent_response","text_delta":"done."}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":4,"state":"DONE","step_type":"agent_response","text_delta":"\n"}}"#,
            r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"done.\n","num_turns":1,"usage":{"input_tokens":100,"output_tokens":5,"cache_read_tokens":40},"denied_actions":[{"action":"write_file","display_name":"Edit"}]}}"#,
        ];
        let events: Vec<Value> = lines.iter().flat_map(|l| m.map(l, "agent")).collect();
        let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["started", "model", "toolStarted", "toolFinished", "toolStarted", "toolFinished", "textDelta", "textDelta", "usage", "warning"]);
        assert_eq!(events[0]["engine"], KIND);
        assert_eq!(events[3]["ok"], true);
        assert_eq!(events[3]["output"], "2 lines");
        assert_eq!(events[5]["ok"], false);
        assert_eq!(events[5]["output"], "user denied permission");
        assert_eq!(events[8]["usage"]["inputTokens"], 100);
        assert!(events[9]["message"].as_str().unwrap().contains("Edit"));
        assert_eq!(m.conversation_id.as_deref(), Some("c1"));
        assert_eq!(m.model.as_deref(), Some("gemini-3.8-flash-low"));
        assert_eq!((m.input_tokens, m.output_tokens, m.cached_tokens), (Some(100), Some(5), Some(40)));
        assert_eq!((m.results, m.tool_calls, m.turns), (1, 2, 1));
        // Transcript: two calls with results, then the reply.
        assert_eq!(m.transcript.len(), 5);
        assert!(matches!(&m.transcript[4].content[0], ContentPart::Text { text } if text == "done.\n"));
    }

    #[test]
    fn parses_usage_and_credits() {
        let usage = r#"{"conversation_id":"","status":"SUCCESS","response":"…","command":{"name":"usage","data":{"description":"Shared limits.","groups":[{"name":"Gemini Models","description":"Models: Gemini 3.8 Flash","buckets":[{"id":"gemini-weekly","name":"Weekly Limit Remaining","description":"Refreshes in 6 days.","window":"weekly","remaining_fraction":0.75,"reset_time":"2026-11-28T02:51:28Z"},{"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1,"reset_time":"2026-11-21T07:51:28Z"}]}]}}}"#;
        let u = usage_from(&command_data(usage).unwrap());
        assert_eq!(u["description"], "Shared limits.");
        assert_eq!(u["groups"][0]["name"], "Gemini Models");
        assert_eq!(u["groups"][0]["buckets"][0]["remainingFraction"], 0.75);
        assert_eq!(u["groups"][0]["buckets"][1]["resetTime"], "2026-11-21T07:51:28Z");
        let credits = r#"{"status":"SUCCESS","command":{"name":"credits","data":{"remaining_credits":0,"upgrade_uri":"https://example.test/upgrade"}}}"#;
        let c = credits_from(&command_data(credits).unwrap());
        assert_eq!(c, json!({ "remaining": 0, "upgradeUri": "https://example.test/upgrade" }));
        assert!(command_data(r#"{"status":"ERROR","error":"not signed in"}"#).unwrap_err().contains("signed in"));
    }

    #[test]
    fn parses_model_list() {
        let m = parse_models("gemini-4-neon-high\tGemini 4 Neon (High)\ngemini-3.8-flash-low\tGemini 3.8 Flash (Low)\nnot a row\n");
        assert_eq!(m, vec![json!({ "id": "gemini-4-neon-high", "label": "Gemini 4 Neon (High)" }), json!({ "id": "gemini-3.8-flash-low", "label": "Gemini 3.8 Flash (Low)" })]);
    }

    #[test]
    fn saves_and_cleans_up_attached_images() {
        let img = crate::provider::types::ImageInput { media_type: "image/png".into(), data: "aGVsbG8=".into() };
        let (guard, paths) = save_attached_images(&[img]).unwrap().unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].extension().is_some_and(|e| e == "png"));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"hello");
        let dir = guard.0.clone();
        drop(guard);
        assert!(!dir.exists(), "temp image dir should be cleaned up on drop");
    }
}
