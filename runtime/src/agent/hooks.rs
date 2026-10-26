//! Lifecycle hooks: user-configured commands that run at agent events and receive a JSON payload
//! on stdin. A "before" hook can block the action (non-zero exit); no hook can grant permissions,
//! because hooks run only after the policy already allowed the action.
//!
//! Sources: hooks passed by the client (user settings) and `<workspace>/.anvil/hooks.json`, the
//! latter only when the client says the workspace is trusted.

use crate::errors::Severity;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Event {
    BeforeTask,
    AfterTask,
    BeforeTool,
    AfterTool,
    BeforeEdit,
    AfterEdit,
    BeforeValidation,
    AfterValidation,
}

/// What a hook command reads on stdin, as JSON.
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
}

impl HookContext {
    pub fn new(event: Event, tool: Option<&str>, payload: Value) -> Self {
        let input = &payload["input"];
        let file_path = input["path"].as_str().or(input["from"].as_str()).map(str::to_owned);
        Self { event, tool_name: tool.map(str::to_owned), file_path, budget_used: None, budget_limit: None, severity: None, payload }
    }
}

/// The result of one hook command.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResult {
    pub command: String,
    pub success: bool,
    /// A failed "before" hook blocks the action.
    pub blocked: bool,
    pub duration_ms: u64,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

impl HookResult {
    /// What to show: stdout on success; stderr, then stdout, on failure.
    pub fn message(&self) -> String {
        let (out, err) = (self.stdout.as_deref().unwrap_or_default(), self.stderr.as_deref().unwrap_or_default());
        if self.success { out.to_owned() } else { format!("{err}{out}") }
    }
}

const MAX_HOOK_OUTPUT: usize = 2_000;

fn kept(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(MAX_HOOK_OUTPUT).collect())
}

impl Event {
    pub fn can_block(self) -> bool {
        matches!(self, Self::BeforeTask | Self::BeforeTool | Self::BeforeEdit | Self::BeforeValidation)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hook {
    pub event: Event,
    /// argv; never run through a shell.
    pub command: Vec<String>,
    /// Regex on the tool name (tool events only).
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
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

/// Runs every hook for the context's event (and tool, if given) in order. Stops at the first block.
pub fn run(hooks: &[Hook], ctx: &HookContext, cwd: &Path, cancel: &CancellationToken) -> Vec<HookResult> {
    let mut out = Vec::new();
    let input = serde_json::to_string(ctx).unwrap_or_default();
    for h in hooks.iter().filter(|h| h.event == ctx.event) {
        if let (Some(m), Some(t)) = (&h.matcher, &ctx.tool_name) {
            if !Regex::new(m).is_ok_and(|r| r.is_match(t)) {
                continue;
            }
        }
        let timeout = Duration::from_millis(h.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));
        let started = std::time::Instant::now();
        let (success, stdout, stderr) = match crate::tools::exec::run_with_input(&h.command, cwd, ".", timeout, cancel, Some(&input)) {
            Ok(o) if o.timed_out => (false, kept(&o.stdout), Some(format!("hook timed out after {}ms", timeout.as_millis()))),
            Ok(o) => (o.success, kept(&o.stdout), kept(&o.stderr)),
            Err(e) => (false, None, kept(&e.message)),
        };
        let blocked = !success && ctx.event.can_block();
        let duration_ms = started.elapsed().as_millis() as u64;
        out.push(HookResult { command: h.command.join(" "), success, blocked, duration_ms, stdout, stderr });
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
        Hook { event, command: vec!["sh".into(), "-c".into(), script.into()], matcher: matcher.map(Into::into), timeout_ms: Some(5_000) }
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
        // The hook reads the context on stdin.
        let echo = vec![hook(Event::AfterTool, "cat", None)];
        let mut after = ctx(Event::AfterTool, "replace_text");
        (after.budget_used, after.severity) = (Some(42), Some(Severity::Permanent));
        let seen: serde_json::Value = serde_json::from_str(&run(&echo, &after, &cwd, &c)[0].message()).unwrap();
        assert_eq!(seen["tool"], "replace_text");
        assert_eq!(
            (seen["filePath"].as_str(), seen["budgetUsed"].as_u64(), seen["severity"].as_str()),
            (Some("src/a.rs"), Some(42), Some("permanent"))
        );
        assert!(validate(&[Hook { event: Event::AfterTask, command: vec![], matcher: None, timeout_ms: None }]).is_err());
    }
}
