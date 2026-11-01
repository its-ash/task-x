//! Lifecycle hooks: user-configured commands that run at agent events and receive a JSON payload
//! on stdin.
//!
//! - A "before" hook (and `userPromptSubmit`, `stop`, `subagentStop`) can block: a non-zero exit, or
//!   JSON output `{ "decision": "block", "reason" }`. A Claude-format hook (from a plugin) blocks only
//!   with exit code 2, as in Claude Code.
//! - JSON output can also carry `updatedInput` (a `beforeTool` hook rewrites the tool input),
//!   `additionalContext` (text the model gets), and `decision: "approve"` (skip the approval prompt).
//!   Approval counts only from a trusted hook (user settings, or a plugin the user installed): a
//!   workspace hook can block but never grant, and the permission policy's `deny` always wins.
//!
//! Sources: hooks passed by the client (user settings), `<workspace>/.task-x/hooks.json` (only when
//! the client says the workspace is trusted), and `hooks/hooks.json` of enabled plugins.

use crate::errors::Severity;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Claude Code's "blocking error" exit code.
const CLAUDE_BLOCK_EXIT: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Event {
    #[serde(alias = "SessionStart")]
    BeforeTask,
    #[serde(alias = "SessionEnd")]
    AfterTask,
    #[serde(alias = "PreToolUse")]
    BeforeTool,
    #[serde(alias = "PostToolUse")]
    AfterTool,
    BeforeEdit,
    AfterEdit,
    BeforeValidation,
    AfterValidation,
    /// The user's prompt, before the task starts: can block it or add context.
    #[serde(alias = "UserPromptSubmit")]
    UserPromptSubmit,
    /// The agent is about to finish: a block makes it continue with the reason as feedback.
    #[serde(alias = "Stop")]
    Stop,
    /// A subagent is about to finish (same as `stop`, for subagents).
    #[serde(alias = "SubagentStop")]
    SubagentStop,
    /// The history is about to be compressed.
    #[serde(alias = "PreCompact")]
    PreCompact,
    /// The agent waits for the user: an approval or a question.
    #[serde(alias = "Notification")]
    Notification,
}

impl Event {
    pub fn can_block(self) -> bool {
        matches!(
            self,
            Self::BeforeTask | Self::BeforeTool | Self::BeforeEdit | Self::BeforeValidation | Self::UserPromptSubmit | Self::Stop | Self::SubagentStop
        )
    }

    /// The name Claude Code gives the event, for Claude-format hooks.
    pub fn claude_name(self) -> &'static str {
        match self {
            Self::BeforeTask => "SessionStart",
            Self::AfterTask => "SessionEnd",
            Self::BeforeTool | Self::BeforeEdit | Self::BeforeValidation => "PreToolUse",
            Self::AfterTool | Self::AfterEdit | Self::AfterValidation => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::Notification => "Notification",
        }
    }

    /// Plain stdout of a successful hook becomes context for the model (as in Claude Code).
    fn stdout_is_context(self) -> bool {
        matches!(self, Self::UserPromptSubmit | Self::BeforeTask)
    }
}

/// The Claude Code name of a task-x tool, so Claude-format matchers (`Bash`, `Edit|Write`) work.
pub fn claude_tool_name(tool: &str) -> &str {
    match tool {
        "run_command" | "run_tests" | "check_build" | "run_linter" | "format_code" => "Bash",
        "replace_text" | "edit_files" | "apply_patch" | "insert_at_line" | "delete_range" | "replace_all" => "Edit",
        "create_file" => "Write",
        "read_file" | "read_range" | "read_symbol" | "read_multiple_files" | "get_file_outline" => "Read",
        "search_text" | "search_regex" | "multi_search" | "find_symbol" | "find_references" => "Grep",
        "find_files" | "list_files" | "get_file_tree" => "Glob",
        "web_fetch" => "WebFetch",
        "web_search" => "WebSearch",
        "run_subagent" | "run_plan" => "Task",
        "todo_write" => "TodoWrite",
        other => other,
    }
}

/// What a hook command reads on stdin, as JSON. The snake_case fields are the ones Claude Code
/// hooks read, so plugin hooks written for Claude Code work unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookContext {
    pub event: Event,
    /// The tool, for tool events (serialized as `tool`, the name hooks have always read).
    #[serde(rename = "tool")]
    pub tool_name: Option<String>,
    /// The file the tool works on (its `path` or `from` input), when it has one.
    pub file_path: Option<String>,
    /// Tokens the task has used so far, and its token budget (null when there is none).
    pub budget_used: Option<u64>,
    pub budget_limit: Option<u64>,
    /// After a failed tool call: how severe the failure is.
    pub severity: Option<Severity>,
    /// Event data: the task, the tool input, and the result for after-events.
    pub payload: Value,
    #[serde(rename = "hook_event_name", default)]
    pub claude_event: String,
    #[serde(rename = "tool_name", default, skip_serializing_if = "Option::is_none")]
    pub claude_tool: Option<String>,
    #[serde(rename = "tool_input", default, skip_serializing_if = "Value::is_null")]
    pub tool_input: Value,
    #[serde(rename = "session_id", default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

impl HookContext {
    pub fn new(event: Event, tool: Option<&str>, payload: Value) -> Self {
        let input = &payload["input"];
        let file_path = input["path"].as_str().or(input["from"].as_str()).map(str::to_owned);
        Self {
            event,
            tool_name: tool.map(str::to_owned),
            file_path,
            budget_used: None,
            budget_limit: None,
            severity: None,
            claude_event: event.claude_name().to_owned(),
            claude_tool: tool.map(|t| claude_tool_name(t).to_owned()),
            tool_input: input.clone(),
            session_id: payload["taskId"].as_str().unwrap_or_default().to_owned(),
            cwd: String::new(),
            prompt: payload["prompt"].as_str().map(str::to_owned),
            payload,
        }
    }
}

/// The result of one hook command.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResult {
    pub command: String,
    pub success: bool,
    /// A failed "before" hook, or one that answered `decision: "block"`, blocks the action.
    pub blocked: bool,
    pub duration_ms: u64,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// The reason a JSON answer gave for its decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A trusted `beforeTool` hook approved the call: no approval prompt.
    #[serde(skip)]
    pub approve: bool,
    #[serde(skip)]
    pub updated_input: Option<Value>,
    #[serde(skip)]
    pub context: Option<String>,
}

impl HookResult {
    /// What to show: the decision's reason, else stdout on success, else stderr then stdout.
    pub fn message(&self) -> String {
        if let Some(r) = &self.reason {
            return r.clone();
        }
        let (out, err) = (self.stdout.as_deref().unwrap_or_default(), self.stderr.as_deref().unwrap_or_default());
        if self.success { out.to_owned() } else { format!("{err}{out}") }
    }
}

/// What all the hooks of one event decided together.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub results: Vec<HookResult>,
    pub approve: bool,
    pub updated_input: Option<Value>,
    pub context: Vec<String>,
}

impl Outcome {
    pub fn from_results(results: Vec<HookResult>) -> Self {
        let approve = results.iter().any(|r| r.approve) && !results.iter().any(|r| r.blocked);
        let updated_input = results.iter().rev().find_map(|r| r.updated_input.clone());
        let context = results.iter().filter_map(|r| r.context.clone()).collect();
        Self { results, approve, updated_input, context }
    }

    pub fn blocked(&self) -> Option<&HookResult> {
        self.results.iter().find(|r| r.blocked)
    }
}

const MAX_HOOK_OUTPUT: usize = 2_000;
const MAX_CONTEXT_CHARS: usize = 10_000;

fn kept(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(MAX_HOOK_OUTPUT).collect())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hook {
    pub event: Event,
    /// argv; never run through a shell (Claude-format plugin hooks are the exception: see `claude`).
    pub command: Vec<String>,
    /// Regex on the tool name (tool events only). It matches the task-x name or the Claude Code name.
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// From the user's settings or a plugin the user installed: may approve a call.
    #[serde(skip)]
    pub trusted: bool,
    /// Claude Code semantics: only exit code 2 blocks.
    #[serde(skip)]
    pub claude: bool,
}

impl Hook {
    #[cfg(test)]
    pub fn new(event: Event, command: Vec<String>) -> Self {
        Self { event, command, matcher: None, timeout_ms: None, trusted: false, claude: false }
    }
}

pub fn load_workspace(root: &Path) -> Result<Vec<Hook>, String> {
    // .task-x/hooks.json; .anvil/hooks.json is the old location.
    let p = [".task-x", ".anvil"]
        .iter()
        .map(|d| root.join(d).join("hooks.json"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.join(".task-x").join("hooks.json"));
    if !p.is_file() {
        return Ok(Vec::new());
    }
    #[derive(Deserialize)]
    struct File {
        hooks: Vec<Hook>,
    }
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str::<File>(&text).map(|f| f.hooks).map_err(|e| format!("{}: {e}", p.display()))
}

/// The argv that runs a Claude Code hook command line (a shell string) for a plugin at `root`.
/// The shell expands `$CLAUDE_PLUGIN_ROOT` from the environment, so a folder with spaces stays one word.
fn shell(command: &str, root: &Path) -> Vec<String> {
    let root = root.to_string_lossy();
    if cfg!(windows) {
        let line = command.replace("${CLAUDE_PLUGIN_ROOT}", "%CLAUDE_PLUGIN_ROOT%").replace("$CLAUDE_PLUGIN_ROOT", "%CLAUDE_PLUGIN_ROOT%");
        vec!["cmd".into(), "/C".into(), format!("set \"CLAUDE_PLUGIN_ROOT={root}\" && {line}")]
    } else {
        vec!["env".into(), format!("CLAUDE_PLUGIN_ROOT={root}"), "sh".into(), "-c".into(), command.to_owned()]
    }
}

/// Hooks in Claude Code's format: `{ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "…", "timeout": 30 }] }] } }`.
/// `root` is the plugin folder (`${CLAUDE_PLUGIN_ROOT}`). Unknown events and non-command hooks are skipped.
pub fn parse_claude(v: &Value, root: &Path, trusted: bool) -> Vec<Hook> {
    let mut out = Vec::new();
    let Some(events) = v["hooks"].as_object() else { return out };
    for (name, groups) in events {
        let Ok(event) = serde_json::from_value::<Event>(Value::String(name.clone())) else { continue };
        for g in groups.as_array().into_iter().flatten() {
            let matcher = g["matcher"].as_str().map(str::trim).filter(|m| !m.is_empty() && *m != "*").map(str::to_owned);
            for h in g["hooks"].as_array().into_iter().flatten() {
                let Some(cmd) = h["command"].as_str().filter(|c| !c.trim().is_empty() && h["type"].as_str().unwrap_or("command") == "command") else {
                    continue;
                };
                out.push(Hook {
                    event,
                    command: shell(cmd, root),
                    matcher: matcher.clone(),
                    timeout_ms: h["timeout"].as_u64().map(|s| s.saturating_mul(1000)),
                    trusted,
                    claude: true,
                });
            }
        }
    }
    out
}

/// A plugin's hooks: `hooks/hooks.json`, else the `hooks` object of `.claude-plugin/plugin.json`.
pub fn load_plugin(root: &Path, trusted: bool) -> Vec<Hook> {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let v = read(root.join("hooks").join("hooks.json"))
        .or_else(|| read(root.join(".claude-plugin").join("plugin.json")).filter(|m| m["hooks"].is_object()));
    v.map(|v| parse_claude(&v, root, trusted)).unwrap_or_default()
}

pub fn validate(hooks: &[Hook]) -> Result<(), String> {
    for h in hooks {
        if h.command.is_empty() || h.command.iter().any(|a| a.contains('\0')) {
            return Err("hook command must be a non-empty argv array".into());
        }
        if let Some(m) = &h.matcher {
            Regex::new(m).map_err(|e| format!("invalid hook matcher '{m}': {e}"))?;
        }
    }
    Ok(())
}

/// Reads a JSON answer: Claude Code's `decision`, `continue`, and `hookSpecificOutput`, and task-x's
/// flat `updatedInput` and `additionalContext`.
fn read_answer(r: &mut HookResult, out: &str, event: Event, trusted: bool) {
    let Ok(v) = serde_json::from_str::<Value>(out.trim()) else {
        if r.success && event.stdout_is_context() {
            r.context = kept(out).map(|c| c.chars().take(MAX_CONTEXT_CHARS).collect());
        }
        return;
    };
    if !v.is_object() {
        return;
    }
    let specific = &v["hookSpecificOutput"];
    let reason = v["reason"].as_str().or(specific["permissionDecisionReason"].as_str()).or(v["stopReason"].as_str()).map(str::to_owned);
    let decision = v["decision"].as_str().or(specific["permissionDecision"].as_str()).unwrap_or_default();
    let stop = v["continue"] == Value::Bool(false);
    if (matches!(decision, "block" | "deny") || stop) && event.can_block() {
        r.blocked = true;
        r.reason = Some(reason.unwrap_or_else(|| "blocked by a hook".into()));
    } else if matches!(decision, "approve" | "allow") && trusted && event == Event::BeforeTool {
        r.approve = true;
    }
    if event == Event::BeforeTool {
        r.updated_input = specific["updatedInput"].as_object().or(v["updatedInput"].as_object()).map(|o| Value::Object(o.clone()));
    }
    r.context = specific["additionalContext"]
        .as_str()
        .or(v["additionalContext"].as_str())
        .or(v["systemMessage"].as_str())
        .map(|c| c.chars().take(MAX_CONTEXT_CHARS).collect());
}

/// Runs every hook for the context's event (and tool, if given) in order. Stops at the first block.
pub fn run(hooks: &[Hook], ctx: &HookContext, cwd: &Path, cancel: &CancellationToken) -> Vec<HookResult> {
    let mut out = Vec::new();
    let mut ctx = ctx.clone();
    ctx.cwd = cwd.to_string_lossy().into_owned();
    let input = serde_json::to_string(&ctx).unwrap_or_default();
    for h in hooks.iter().filter(|h| h.event == ctx.event) {
        if let (Some(m), Some(t)) = (&h.matcher, &ctx.tool_name) {
            let alias = claude_tool_name(t);
            if !Regex::new(&format!("^(?:{m})$")).is_ok_and(|r| r.is_match(t) || r.is_match(alias)) && !Regex::new(m).is_ok_and(|r| r.is_match(t)) {
                continue;
            }
        }
        let timeout = Duration::from_millis(h.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));
        let started = std::time::Instant::now();
        let (success, code, raw, stdout, stderr) = match crate::tools::exec::run_with_input(&h.command, cwd, ".", timeout, cancel, Some(&input)) {
            Ok(o) if o.timed_out => (false, None, o.stdout.clone(), kept(&o.stdout), Some(format!("hook timed out after {}ms", timeout.as_millis()))),
            Ok(o) => (o.success, o.exit_code, o.stdout.clone(), kept(&o.stdout), kept(&o.stderr)),
            Err(e) => (false, None, String::new(), None, kept(&e.message)),
        };
        let failed_blocks = if h.claude { code == Some(CLAUDE_BLOCK_EXIT) } else { !success };
        let mut r = HookResult {
            command: h.command.join(" "),
            success,
            blocked: failed_blocks && ctx.event.can_block(),
            duration_ms: started.elapsed().as_millis() as u64,
            stdout,
            stderr,
            ..Default::default()
        };
        if success {
            read_answer(&mut r, &raw, ctx.event, h.trusted);
        }
        let blocked = r.blocked;
        out.push(r);
        if blocked {
            break;
        }
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn hook(event: Event, script: &str, matcher: Option<&str>) -> Hook {
        Hook { matcher: matcher.map(Into::into), timeout_ms: Some(5_000), ..Hook::new(event, vec!["sh".into(), "-c".into(), script.into()]) }
    }

    #[test]
    fn before_hooks_block_on_failure_and_receive_payload() {
        let cwd = std::env::temp_dir();
        let c = CancellationToken::new();
        let hooks = vec![
            hook(Event::BeforeTool, "grep -q delete_file && { echo 'no deletes' >&2; exit 1; } || exit 0", None),
            hook(Event::BeforeTool, "echo second", Some("^run_")),
            hook(Event::AfterTool, "exit 3", None),
        ];
        validate(&hooks).unwrap();
        let ctx = |event, tool| HookContext::new(event, Some(tool), serde_json::json!({ "input": { "path": "src/a.rs" } }));
        let r = run(&hooks, &ctx(Event::BeforeTool, "delete_file"), &cwd, &c);
        assert_eq!(r.len(), 1);
        assert!(r[0].blocked && !r[0].success && r[0].message().contains("no deletes"));
        let r = run(&hooks, &ctx(Event::BeforeTool, "run_tests"), &cwd, &c);
        assert_eq!(r.iter().map(HookResult::message).collect::<Vec<_>>(), ["", "second"]);
        let r = run(&hooks, &ctx(Event::AfterTool, "x"), &cwd, &c);
        assert!(!r[0].blocked && !r[0].success, "after-hooks cannot block");
        // The hook reads the context on stdin, with the Claude Code field names too.
        let echo = vec![hook(Event::AfterTool, "cat", None)];
        let mut after = ctx(Event::AfterTool, "replace_text");
        (after.budget_used, after.severity) = (Some(42), Some(Severity::Permanent));
        let seen: serde_json::Value = serde_json::from_str(&run(&echo, &after, &cwd, &c)[0].message()).unwrap();
        assert_eq!(seen["tool"], "replace_text");
        assert_eq!((seen["tool_name"].as_str(), seen["hook_event_name"].as_str()), (Some("Edit"), Some("PostToolUse")));
        assert_eq!(seen["tool_input"]["path"], "src/a.rs");
        assert_eq!(
            (seen["filePath"].as_str(), seen["budgetUsed"].as_u64(), seen["severity"].as_str()),
            (Some("src/a.rs"), Some(42), Some("permanent"))
        );
        assert!(validate(&[Hook::new(Event::AfterTask, vec![])]).is_err());
    }

    #[test]
    fn json_answers_block_approve_rewrite_and_add_context() {
        let cwd = std::env::temp_dir();
        let c = CancellationToken::new();
        let ctx = HookContext::new(Event::BeforeTool, Some("run_command"), serde_json::json!({ "input": { "command": ["rm", "-rf", "x"] } }));
        let block = vec![hook(Event::BeforeTool, r#"echo '{"decision":"block","reason":"no rm"}'"#, Some("Bash"))];
        let r = Outcome::from_results(run(&block, &ctx, &cwd, &c));
        assert_eq!(r.blocked().map(HookResult::message).as_deref(), Some("no rm"), "a Claude matcher matches the task-x tool");
        let approve = r#"echo '{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"command":["ls"]}}}'"#;
        let untrusted = Outcome::from_results(run(&[hook(Event::BeforeTool, approve, None)], &ctx, &cwd, &c));
        assert!(!untrusted.approve, "an untrusted hook cannot grant");
        assert_eq!(untrusted.updated_input.unwrap()["command"][0], "ls");
        let trusted = Outcome::from_results(run(&[Hook { trusted: true, ..hook(Event::BeforeTool, approve, None) }], &ctx, &cwd, &c));
        assert!(trusted.approve);
        let prompt = HookContext::new(Event::UserPromptSubmit, None, serde_json::json!({ "prompt": "fix it" }));
        let ctxs = Outcome::from_results(run(&[hook(Event::UserPromptSubmit, "echo 'Today is release day.'", None)], &prompt, &cwd, &c));
        assert_eq!(ctxs.context, ["Today is release day."], "plain stdout of a prompt hook is context");
        let stop = HookContext::new(Event::Stop, None, serde_json::json!({}));
        let r = run(&[hook(Event::Stop, r#"echo '{"decision":"block","reason":"run the tests first"}'"#, None)], &stop, &cwd, &c);
        assert!(r[0].blocked && r[0].message() == "run the tests first", "a stop hook can make the agent go on");
        let compact = HookContext::new(Event::PreCompact, None, serde_json::json!({}));
        assert!(!run(&[hook(Event::PreCompact, "exit 1", None)], &compact, &cwd, &c)[0].blocked, "preCompact cannot block");
    }

    #[test]
    fn claude_format_plugin_hooks_block_only_with_exit_two() {
        let root = std::env::temp_dir().join(format!("task-x-plugin-hooks-{}", std::process::id()));
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join("hooks/hooks.json"),
            r#"{ "hooks": {
                "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "[ -f \"${CLAUDE_PLUGIN_ROOT}/hooks/hooks.json\" ] && exit 2" }] }],
                "PostToolUse": [{ "hooks": [{ "type": "command", "command": "exit 1" }] }],
                "Unknown": [{ "hooks": [{ "type": "command", "command": "true" }] }]
            } }"#,
        )
        .unwrap();
        let hooks = load_plugin(&root, true);
        assert_eq!(hooks.len(), 2, "unknown events are skipped");
        assert!(hooks.iter().all(|h| h.claude && h.trusted));
        let c = CancellationToken::new();
        let bash = HookContext::new(Event::BeforeTool, Some("run_command"), serde_json::json!({}));
        assert!(run(&hooks, &bash, &root, &c)[0].blocked, "exit 2 blocks, and CLAUDE_PLUGIN_ROOT is set");
        let read = HookContext::new(Event::BeforeTool, Some("read_file"), serde_json::json!({}));
        assert!(run(&hooks, &read, &root, &c).is_empty(), "the Bash matcher skips other tools");
        let post = HookContext::new(Event::AfterTool, Some("read_file"), serde_json::json!({}));
        assert!(!run(&hooks, &post, &root, &c)[0].blocked);
        std::fs::remove_dir_all(root).ok();
    }
}
