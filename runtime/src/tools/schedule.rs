//! Waiting tools: `sleep` pauses for a few seconds; `wait_until` runs a check command again and
//! again until it passes. The agent loop runs `wait_until` in the background (see
//! `agent::wakeup`), so the result arrives later as a message; a caller without a way to deliver
//! it later (the MCP server on its own) waits for the result.

use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const SLEEP: &str = "sleep";
pub const WAIT_UNTIL: &str = "wait_until";
/// The longest `sleep`; longer waits belong to `schedule_wakeup`, which does not hold the turn.
pub const MAX_SLEEP_SECS: f64 = 120.0;
pub const MIN_INTERVAL_SECS: u64 = 2;
pub const MAX_INTERVAL_SECS: u64 = 600;
const DEFAULT_INTERVAL_SECS: u64 = 15;
pub const MIN_WAIT_SECS: u64 = 5;
pub const MAX_WAIT_SECS: u64 = 3_600;
const DEFAULT_WAIT_SECS: u64 = 600;
/// One run of the check command may take this long at most.
const MAX_ATTEMPT_SECS: u64 = 120;
/// Output of the last check given back.
const MAX_TAIL_CHARS: usize = 2_000;
const MAX_REASON_CHARS: usize = 200;
/// How often a wait looks at its cancellation token.
const SLICE: Duration = Duration::from_millis(100);

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: SLEEP,
            description: "Pause for a few seconds (up to 120), then continue: for example wait 5 seconds for a server to start before calling it. The turn waits, so keep it short. To come back after minutes or hours, use schedule_wakeup; to wait until a command succeeds, use wait_until.",
            schema: || {
                json!({ "type": "object", "properties": {
                    "seconds": { "type": "number", "exclusiveMinimum": 0, "maximum": MAX_SLEEP_SECS, "description": "How long to pause, 0.1 to 120 seconds" },
                    "reason": { "type": "string", "description": "What you are waiting for (shown to the user)" }
                }, "required": ["seconds"], "additionalProperties": false })
            },
            permission: Permission::Read,
            timeout: Duration::from_secs(MAX_SLEEP_SECS as u64 + 5),
            handler: sleep,
        },
        ToolDef {
            name: WAIT_UNTIL,
            description: "Run a check command every intervalSecs until it passes, for up to timeoutSecs: until it exits with code 0, or, with match, until its output matches the regular expression. Use it to wait for a server to come up, a deploy or CI run to finish, or a file to appear, without spending a turn on every check. In the task-x agent it runs in the background: the call returns at once with an id, and the result comes to you as a message when the check passes or the time is up (list_wakeups and cancel_wakeup also cover it). The command takes no shell: pass the program and its arguments.",
            schema: || {
                json!({ "type": "object", "properties": {
                    "command": {
                        "type": ["array", "string"], "items": { "type": "string" }, "minItems": 1,
                        "description": "The check: program and arguments, e.g. [\"curl\", \"-sf\", \"http://localhost:3000/health\"]"
                    },
                    "cwd": { "type": "string", "description": "Working directory relative to the workspace root (default '.')" },
                    "match": { "type": "string", "description": "Pass when stdout or stderr matches this regular expression (any exit code). Unset: pass on exit code 0" },
                    "intervalSecs": { "type": "integer", "minimum": MIN_INTERVAL_SECS, "maximum": MAX_INTERVAL_SECS, "description": "Seconds between checks (default 15)" },
                    "timeoutSecs": { "type": "integer", "minimum": MIN_WAIT_SECS, "maximum": MAX_WAIT_SECS, "description": "Give up after this many seconds (default 600, max 3600)" },
                    "reason": { "type": "string", "description": "What you are waiting for (shown to the user)" }
                }, "required": ["command"], "additionalProperties": false })
            },
            permission: Permission::Execute,
            timeout: Duration::from_secs(MAX_WAIT_SECS + MAX_ATTEMPT_SECS + 10),
            handler: wait_until,
        },
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepIn {
    seconds: f64,
    #[serde(default)]
    reason: Option<String>,
}

/// Waits `d`, checking for cancellation. Err when cancelled.
fn pause(c: &ToolCtx, d: Duration) -> Result<(), ToolError> {
    let end = Instant::now() + d;
    loop {
        c.check_cancel()?;
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep(SLICE.min(end - now));
    }
}

fn sleep(c: &ToolCtx, input: Value) -> ToolResult {
    let i: SleepIn = parse(input)?;
    if !i.seconds.is_finite() || i.seconds <= 0.0 || i.seconds > MAX_SLEEP_SECS {
        return Err(ToolError::invalid(format!(
            "seconds must be more than 0 and at most {MAX_SLEEP_SECS}; to come back later use schedule_wakeup, to wait for a condition use wait_until"
        )));
    }
    let started = Instant::now();
    pause(c, Duration::from_secs_f64(i.seconds))?;
    Ok(json!({ "slept": true, "seconds": (started.elapsed().as_millis() as f64 / 1000.0), "reason": reason(i.reason.as_deref()) }))
}

/// Input of `wait_until`, checked before anything runs (the agent checks it before it schedules).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitIn {
    pub command: Value,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default, rename = "match")]
    pub pattern: Option<String>,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A checked `wait_until` input.
#[derive(Debug)]
pub struct WaitSpec {
    pub input: WaitIn,
    pub pattern: Option<Regex>,
    pub interval: Duration,
    pub timeout: Duration,
    /// The command as one line, for messages.
    pub label: String,
    pub reason: String,
}

pub fn check_wait(input: Value) -> Result<WaitSpec, ToolError> {
    let i: WaitIn = parse(input)?;
    let label = match &i.command {
        Value::String(s) if !s.trim().is_empty() => s.trim().to_owned(),
        Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_string) => {
            a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")
        }
        _ => return Err(ToolError::invalid("command must be a program and its arguments (a list of strings, or one line)")),
    };
    let pattern = match i.pattern.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Some(Regex::new(p).map_err(|e| ToolError::invalid(format!("match is not a valid regular expression: {e}")))?),
        None => None,
    };
    let interval = i.interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS);
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&interval) {
        return Err(ToolError::invalid(format!("intervalSecs must be {MIN_INTERVAL_SECS} to {MAX_INTERVAL_SECS}")));
    }
    let timeout = i.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS);
    if !(MIN_WAIT_SECS..=MAX_WAIT_SECS).contains(&timeout) {
        return Err(ToolError::invalid(format!(
            "timeoutSecs must be {MIN_WAIT_SECS} to {MAX_WAIT_SECS}; for a longer wait, schedule_wakeup and check again then"
        )));
    }
    let reason = i.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()).map_or_else(|| label.clone(), str::to_owned);
    Ok(WaitSpec {
        reason: clip(&reason, MAX_REASON_CHARS),
        label,
        pattern,
        interval: Duration::from_secs(interval),
        timeout: Duration::from_secs(timeout),
        input: i,
    })
}

fn wait_until(c: &ToolCtx, input: Value) -> ToolResult {
    let spec = check_wait(input)?;
    let run = super::find("run_command").ok_or_else(|| ToolError::new(ToolErrorKind::Failed, "run_command is missing"))?;
    let started = Instant::now();
    let attempt_secs = spec.interval.as_secs().clamp(10, MAX_ATTEMPT_SECS).min(spec.timeout.as_secs());
    let mut attempts = 0u32;
    loop {
        c.check_cancel()?;
        attempts += 1;
        let mut cmd = json!({ "command": spec.input.command, "timeoutSecs": attempt_secs });
        if let Some(cwd) = &spec.input.cwd {
            cmd["cwd"] = json!(cwd);
        }
        // A command that cannot start (bad cwd, empty program) fails the same way every time.
        let out = (run.handler)(c, cmd)?;
        let r = &out["result"];
        let text = format!("{}\n{}", r["stdout"].as_str().unwrap_or_default(), r["stderr"].as_str().unwrap_or_default());
        let met = match &spec.pattern {
            Some(p) => p.is_match(&text),
            None => r["success"].as_bool().unwrap_or(false),
        };
        let elapsed = started.elapsed();
        let next = spec.interval;
        if met || elapsed + next > spec.timeout {
            return Ok(json!({
                "met": met,
                "attempts": attempts,
                "elapsedSecs": elapsed.as_secs(),
                "command": spec.label,
                "reason": spec.reason,
                "last": { "exitCode": r["exitCode"], "timedOut": r["timedOut"], "output": tail(text.trim(), MAX_TAIL_CHARS) },
                "message": if met { "The check passed." } else { "The time ran out before the check passed." },
            }));
        }
        pause(c, next)?;
    }
}

fn reason(r: Option<&str>) -> Value {
    r.map(str::trim).filter(|r| !r.is_empty()).map_or(Value::Null, |r| json!(clip(r, MAX_REASON_CHARS)))
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max { s.to_owned() } else { format!("…{}", s.chars().skip(n - max).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn sleep_pauses_and_checks_its_input() {
        let c = ctx(&[("a.txt", "x")]);
        let t = Instant::now();
        let r = run(&c, SLEEP, json!({ "seconds": 0.2, "reason": "server start" })).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(200));
        assert_eq!((r["slept"].as_bool(), r["reason"].as_str()), (Some(true), Some("server start")));
        for bad in [json!({ "seconds": 0 }), json!({ "seconds": 121 }), json!({ "seconds": -1 })] {
            let e = run(&c, SLEEP, bad).unwrap_err();
            assert_eq!(e.kind, ToolErrorKind::InvalidInput);
        }
        assert!(run(&c, SLEEP, json!({ "seconds": 500 })).unwrap_err().message.contains("schedule_wakeup"));
    }

    #[test]
    fn sleep_stops_when_cancelled() {
        let c = ctx(&[("a.txt", "x")]);
        c.cancel.cancel();
        let t = Instant::now();
        assert_eq!(run(&c, SLEEP, json!({ "seconds": 60 })).unwrap_err().kind, ToolErrorKind::Cancelled);
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_until_checks_its_input() {
        assert!(check_wait(json!({ "command": [] })).is_err());
        assert!(check_wait(json!({ "command": ["true"], "match": "(" })).unwrap_err().message.contains("regular expression"));
        assert!(check_wait(json!({ "command": ["true"], "intervalSecs": 1 })).is_err());
        assert!(check_wait(json!({ "command": ["true"], "timeoutSecs": 7200 })).unwrap_err().message.contains("schedule_wakeup"));
        let s = check_wait(json!({ "command": ["curl", "-sf", "http://x"] })).unwrap();
        assert_eq!((s.label.as_str(), s.reason.as_str(), s.interval.as_secs(), s.timeout.as_secs()), ("curl -sf http://x", "curl -sf http://x", 15, 600));
    }

    #[cfg(unix)]
    #[test]
    fn wait_until_passes_on_exit_zero_or_a_match() {
        let c = ctx(&[("a.txt", "x")]);
        let ok = run(&c, WAIT_UNTIL, json!({ "command": ["true"], "timeoutSecs": 5 })).unwrap();
        assert_eq!((ok["met"].as_bool(), ok["attempts"].as_u64()), (Some(true), Some(1)), "{ok}");
        let matched = run(&c, WAIT_UNTIL, json!({ "command": ["sh", "-c", "echo build READY; exit 3"], "match": "READY", "timeoutSecs": 5 })).unwrap();
        assert_eq!(matched["met"], true, "{matched}");
    }

    #[cfg(unix)]
    #[test]
    fn wait_until_gives_up_at_its_timeout() {
        let c = ctx(&[("a.txt", "x")]);
        let r = run(&c, WAIT_UNTIL, json!({ "command": ["false"], "intervalSecs": 2, "timeoutSecs": 5 })).unwrap();
        assert_eq!(r["met"], false, "{r}");
        assert!(r["attempts"].as_u64().unwrap() >= 2, "{r}");
        assert!(r["elapsedSecs"].as_u64().unwrap() <= 5, "{r}");
    }
}
