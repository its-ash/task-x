//! Wake-ups: a task can ask to be woken later with a note to itself (`schedule_wakeup`), or when a
//! check command passes (`wait_until`, run in the background). When the model has nothing else to
//! do while some are pending, the task sleeps (status `sleeping`) and wakes when one fires or the
//! user sends a message. Wake-ups live in memory only: they end with their task, and when the
//! runtime (VS Code) stops.

use super::EmitFn;
use crate::provider::ToolSpec;
use regex::Regex;
use crate::tools::schedule::{WAIT_UNTIL, check_wait, clip};
use crate::tools::{self, ToolCtx, ToolError, ToolErrorKind, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const SCHEDULE: &str = "schedule_wakeup";
pub const LIST: &str = "list_wakeups";
pub const CANCEL: &str = "cancel_wakeup";
/// The scheduler's own tools (`wait_until` is a registry tool the scheduler runs in the background).
pub const NAMES: &[&str] = &[SCHEDULE, LIST, CANCEL];
pub const MIN_DELAY_SECS: u64 = 10;
pub const MAX_DELAY_SECS: u64 = 24 * 3600;
/// Wake-ups and checks one task may have pending at once.
pub const MAX_PENDING: usize = 10;
const MAX_PROMPT_CHARS: usize = 4_000;
const MAX_REASON_CHARS: usize = 200;
/// Result of a finished `wait_until` given to the model.
const MAX_REPORT_CHARS: usize = 12_000;
/// While sleeping, how often waiting user messages are looked for.
const STEER_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// `schedule_wakeup`: fires at `due_ms` with the model's prompt.
    Wakeup,
    /// `wait_until`: fires when the check passes, or at `due_ms` when it gives up.
    Watch,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub id: String,
    pub kind: Kind,
    pub reason: String,
    /// The note the model gets on waking (wake-up), or the check command (watch).
    pub prompt: String,
    pub created_ms: u64,
    /// When it fires (wake-up), or gives up at the latest (watch).
    pub due_ms: u64,
}

struct Entry {
    info: Pending,
    stop: CancellationToken,
}

#[derive(Default)]
struct State {
    next: u32,
    pending: Vec<Entry>,
    /// Messages of fired wake-ups the model has not seen yet.
    fired: Vec<String>,
}

/// The wake-ups of one task.
pub struct Scheduler {
    task_id: String,
    emit: EmitFn,
    /// The task's token: cancelling the task ends every wake-up.
    cancel: CancellationToken,
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScheduleIn {
    delay_seconds: u64,
    prompt: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelIn {
    id: String,
}

/// Tool specs of `schedule_wakeup`, `list_wakeups`, and `cancel_wakeup`.
pub fn specs() -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, input_schema: Value| ToolSpec { name: name.into(), description: description.into(), input_schema };
    vec![
        spec(
            SCHEDULE,
            "Come back to something later: check a deploy in 30 minutes, look at a long job's progress, retry after a rate limit. The call returns at once with an id. Keep working if there is more to do; when you finish your turn, the task sleeps and wakes when the wake-up is due, with your prompt as a message. delaySeconds is 10 to 86400 (24 hours). For a pause under 2 minutes use sleep; to wait until a command succeeds use wait_until. Wake-ups end with the task and when task-x restarts.",
            json!({ "type": "object", "properties": {
                "delaySeconds": { "type": "integer", "minimum": MIN_DELAY_SECS, "maximum": MAX_DELAY_SECS, "description": "Seconds from now, e.g. 1800 for 30 minutes" },
                "prompt": { "type": "string", "description": "The note you get on waking: what to check or do then, with the facts you will need" },
                "reason": { "type": "string", "description": "A few words shown to the user while the task sleeps" }
            }, "required": ["delaySeconds", "prompt"], "additionalProperties": false }),
        ),
        spec(
            LIST,
            "List this task's pending wake-ups and wait_until checks: id, kind, reason, and seconds until each is due.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        spec(
            CANCEL,
            "Cancel a pending wake-up or wait_until check by id, or every one with id \"all\".",
            json!({ "type": "object", "properties": { "id": { "type": "string", "description": "An id from schedule_wakeup, wait_until, or list_wakeups; \"all\" for every one" } }, "required": ["id"], "additionalProperties": false }),
        ),
    ]
}

/// The scheduler's own tool called `name`.
pub fn name(name: &str) -> Option<&'static str> {
    NAMES.iter().copied().find(|n| *n == name)
}

/// Whether `name` is run by the scheduler.
pub fn handles(name: &str) -> bool {
    NAMES.contains(&name) || name == WAIT_UNTIL
}

fn now_ms() -> u64 {
    super::store::now_ms()
}

impl Scheduler {
    pub fn new(task_id: &str, emit: EmitFn, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self { task_id: task_id.to_owned(), emit, cancel, state: Mutex::default(), changed: Notify::new() })
    }

    fn emit(&self, event: Value) {
        (self.emit)(&self.task_id, event);
    }

    /// Runs one scheduler tool (see `handles`). `ctx` makes the tool context for a `wait_until`
    /// check from the token that stops it.
    pub fn call(
        self: &Arc<Self>,
        name: &str,
        input: &Value,
        ctx: impl FnOnce(CancellationToken) -> Result<Arc<ToolCtx>, ToolError>,
    ) -> ToolResult {
        match name {
            SCHEDULE => self.schedule(input),
            LIST => Ok(self.list()),
            CANCEL => self.cancel_one(input),
            WAIT_UNTIL => self.watch(input, ctx),
            _ => Err(ToolError::new(ToolErrorKind::NotFound, format!("unknown tool '{name}'"))),
        }
    }

    fn add(&self, kind: Kind, reason: String, prompt: String, after: Duration) -> Result<(Pending, CancellationToken), ToolError> {
        if self.cancel.is_cancelled() {
            return Err(ToolError::new(ToolErrorKind::Cancelled, "the task is ending"));
        }
        let mut s = self.state.lock().expect("wakeups lock");
        if s.pending.len() >= MAX_PENDING {
            return Err(ToolError::invalid(format!(
                "{MAX_PENDING} wake-ups and checks are already pending; cancel one first (list_wakeups shows them)"
            )));
        }
        s.next += 1;
        let now = now_ms();
        let info = Pending { id: format!("w{}", s.next), kind, reason, prompt, created_ms: now, due_ms: now + after.as_millis() as u64 };
        let stop = self.cancel.child_token();
        s.pending.push(Entry { info: info.clone(), stop: stop.clone() });
        drop(s);
        self.emit(json!({ "type": "wakeupScheduled", "wakeup": info }));
        self.changed.notify_waiters();
        Ok((info, stop))
    }

    /// Moves a pending entry to the fired list. Nothing happens when it was cancelled meanwhile.
    fn fire(&self, id: &str, text: String, extra: Value) {
        let mut s = self.state.lock().expect("wakeups lock");
        let Some(pos) = s.pending.iter().position(|e| e.info.id == id) else { return };
        let e = s.pending.remove(pos);
        s.fired.push(text);
        drop(s);
        let mut event = json!({ "type": "wakeupFired", "id": id, "kind": e.info.kind, "reason": e.info.reason });
        if let (Some(o), Value::Object(x)) = (event.as_object_mut(), extra) {
            o.extend(x);
        }
        self.emit(event);
        self.changed.notify_waiters();
    }

    fn schedule(self: &Arc<Self>, input: &Value) -> ToolResult {
        let i: ScheduleIn = tools::parse(input.clone())?;
        if !(MIN_DELAY_SECS..=MAX_DELAY_SECS).contains(&i.delay_seconds) {
            return Err(ToolError::invalid(format!(
                "delaySeconds must be {MIN_DELAY_SECS} to {MAX_DELAY_SECS}; for a shorter pause use sleep"
            )));
        }
        let prompt = i.prompt.trim();
        if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(ToolError::invalid(format!("prompt must be 1 to {MAX_PROMPT_CHARS} characters")));
        }
        let reason = i.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()).unwrap_or_else(|| prompt.lines().next().unwrap_or(prompt));
        let reason = clip(reason, MAX_REASON_CHARS);
        let delay = Duration::from_secs(i.delay_seconds);
        let (info, stop) = self.add(Kind::Wakeup, reason.clone(), prompt.to_owned(), delay)?;
        let (me, id, prompt) = (self.clone(), info.id.clone(), prompt.to_owned());
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = tokio::time::sleep(delay) => {
                    let text = format!(
                        "<wakeup id=\"{id}\">\nWake-up {id} ({reason}) is due; you scheduled it {} ago. Your note for now:\n{prompt}\n</wakeup>",
                        human(delay)
                    );
                    me.fire(&id, text, Value::Null);
                }
            }
        });
        Ok(json!({
            "scheduled": true,
            "id": info.id,
            "dueMs": info.due_ms,
            "in": human(delay),
            "message": "Scheduled. Continue with any other work; when you end your turn the task sleeps until a wake-up is due. Do not sleep or poll for it.",
        }))
    }

    fn watch(self: &Arc<Self>, input: &Value, ctx: impl FnOnce(CancellationToken) -> Result<Arc<ToolCtx>, ToolError>) -> ToolResult {
        let spec = check_wait(input.clone())?;
        let def = tools::find(WAIT_UNTIL).ok_or_else(|| ToolError::new(ToolErrorKind::Failed, "wait_until is missing"))?;
        let (info, stop) = self.add(Kind::Watch, spec.reason.clone(), spec.label.clone(), spec.timeout)?;
        let ctx = match ctx(stop.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.remove(&info.id);
                return Err(e);
            }
        };
        let (me, id, input, label, reason) = (self.clone(), info.id.clone(), input.clone(), spec.label, spec.reason);
        tokio::spawn(async move {
            let res = tools::call(def, ctx, input).await;
            if stop.is_cancelled() {
                return;
            }
            let (met, body) = match &res {
                Ok(v) => (v["met"] == true, v.to_string()),
                Err(e) => (false, json!({ "error": e }).to_string()),
            };
            let state = if met { "passed" } else if res.is_ok() { "did not pass before its timeout" } else { "failed" };
            let text = format!(
                "<wakeup id=\"{id}\">\nThe wait_until check {id} ({reason}) {state}. Command: `{label}`. Result:\n{}\n</wakeup>",
                clip(&body, MAX_REPORT_CHARS)
            );
            me.fire(&id, text, json!({ "met": met }));
        });
        Ok(json!({
            "background": true,
            "id": info.id,
            "command": info.prompt,
            "givesUpAtMs": info.due_ms,
            "message": "The check runs in the background. Its result comes to you as a message when it passes or gives up; continue with other work or end your turn. Do not poll for it.",
        }))
    }

    fn remove(&self, id: &str) -> Option<Pending> {
        let mut s = self.state.lock().expect("wakeups lock");
        let pos = s.pending.iter().position(|e| e.info.id == id)?;
        let e = s.pending.remove(pos);
        e.stop.cancel();
        Some(e.info)
    }

    /// The pending wake-ups, soonest first, with the seconds left.
    pub fn list(&self) -> Value {
        let now = now_ms();
        let mut items: Vec<Pending> = self.state.lock().expect("wakeups lock").pending.iter().map(|e| e.info.clone()).collect();
        items.sort_by_key(|p| p.due_ms);
        let list: Vec<Value> = items
            .into_iter()
            .map(|p| {
                let left = p.due_ms.saturating_sub(now).div_ceil(1000);
                let mut v = serde_json::to_value(&p).unwrap_or_default();
                v["dueInSecs"] = json!(left);
                v
            })
            .collect();
        json!({ "count": list.len(), "pending": list })
    }

    fn cancel_one(&self, input: &Value) -> ToolResult {
        let i: CancelIn = tools::parse(input.clone())?;
        let id = i.id.trim();
        let ids: Vec<String> = if id.eq_ignore_ascii_case("all") {
            self.state.lock().expect("wakeups lock").pending.iter().map(|e| e.info.id.clone()).collect()
        } else {
            vec![id.to_owned()]
        };
        let cancelled: Vec<String> = ids.iter().filter_map(|id| self.remove(id)).map(|p| p.id).collect();
        if cancelled.is_empty() && !id.eq_ignore_ascii_case("all") {
            return Err(ToolError::new(ToolErrorKind::NotFound, format!("no pending wake-up '{id}'; list_wakeups shows the pending ones")));
        }
        for c in &cancelled {
            self.emit(json!({ "type": "wakeupCancelled", "id": c, "by": "model" }));
        }
        self.changed.notify_waiters();
        Ok(json!({ "cancelled": cancelled, "pending": self.pending() }))
    }

    /// How many wake-ups and checks are pending.
    pub fn pending(&self) -> usize {
        self.state.lock().expect("wakeups lock").pending.len()
    }

    /// Whether some fired wake-up waits for the model.
    pub fn has_fired(&self) -> bool {
        !self.state.lock().expect("wakeups lock").fired.is_empty()
    }

    /// The messages of fired wake-ups, for the model's next step.
    pub fn take_fired(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().expect("wakeups lock").fired)
    }

    /// When the next one is due.
    pub fn next_due_ms(&self) -> Option<u64> {
        self.state.lock().expect("wakeups lock").pending.iter().map(|e| e.info.due_ms).min()
    }

    /// The `sleeping` event: when the next wake-up is due, and what is pending.
    pub fn sleeping_event(&self) -> Value {
        json!({ "type": "sleeping", "untilMs": self.next_due_ms(), "pending": self.list()["pending"] })
    }

    /// Sleeps until a wake-up fires (true), a user message waits (`interrupt` says so; true), or
    /// nothing is pending any more or the task is cancelled (false).
    pub async fn wait(&self, interrupt: impl Fn() -> bool) -> bool {
        loop {
            let changed = self.changed.notified();
            {
                let s = self.state.lock().expect("wakeups lock");
                if !s.fired.is_empty() {
                    return true;
                }
                if s.pending.is_empty() {
                    return false;
                }
            }
            if interrupt() {
                return true;
            }
            tokio::select! {
                _ = changed => {}
                _ = self.cancel.cancelled() => return false,
                _ = tokio::time::sleep(STEER_POLL) => {}
            }
        }
    }

    /// Schedules the wake-up a reply promised (`Promise::After`), as if the model had called
    /// `schedule_wakeup`; the delay is kept within `MIN_DELAY_SECS..=MAX_DELAY_SECS`.
    pub fn schedule_promised(self: &Arc<Self>, delay: Duration, said: &str) -> ToolResult {
        let secs = delay.as_secs().clamp(MIN_DELAY_SECS, MAX_DELAY_SECS);
        let prompt = format!(
            "You said: \"{said}\". Nothing was scheduled, so task-x scheduled this wake-up for you. It is time now: do what you said, then continue."
        );
        self.schedule(&json!({ "delaySeconds": secs, "prompt": prompt, "reason": format!("you said: {said}") }))
    }

    /// Ends every pending wake-up (the task is over).
    pub fn cancel_all(&self) {
        let gone: Vec<Pending> = {
            let mut s = self.state.lock().expect("wakeups lock");
            s.fired.clear();
            s.pending.drain(..).map(|e| {
                e.stop.cancel();
                e.info
            })
            .collect()
        };
        for p in gone {
            self.emit(json!({ "type": "wakeupCancelled", "id": p.id, "by": "taskEnded" }));
        }
    }
}

/// A reply that ends a turn with a promise to come back later.
#[derive(Debug, PartialEq, Eq)]
pub enum Promise {
    /// It names a time ("I'll check again in 30 minutes"): task-x schedules the wake-up itself.
    After { delay: Duration, said: String },
    /// It names no time ("I'll report when the build finishes"): the model is reminded to schedule it.
    Unscheduled { said: String },
}

/// The model says it will do something itself, later.
static COME_BACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:i'll|i’ll|i will|i'm going to|i’m going to|i am going to|i shall)\b[^\n]*?\b(?:check|come back|look|wait|report|follow up|get back|revisit|resume|retry|try again|continue|poll|see)\b",
    )
    .expect("come-back regex")
});
/// ...and that it is later, not now.
static LATER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:when|once|after|later|back|until|as soon as|in a (?:bit|while))\b").expect("later regex"));
/// An offer to the user ("if you want, I'll check again later") is not a promise.
static OFFER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:if you|would you|should i|do you want|let me know)\b").expect("offer regex"));
/// "in 30 minutes", "after an hour", "for another 2h", "in half an hour".
static DELAY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:in|after|for|another|wait)\s+(?:about\s+|around\s+|roughly\s+|another\s+|~\s*)?(\d+(?:\.\d+)?|an?|one|two|three|four|five|ten|fifteen|twenty|thirty|forty|sixty|half an?)\s*(seconds?|secs?|minutes?|mins?|hours?|hrs?|h|m)\b",
    )
    .expect("delay regex")
});

/// Sentences of a reply: they end at `.`, `!`, or `?` before a space, or at a line break.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next_space = chars.peek().is_none_or(|(_, n)| n.is_whitespace());
        if c == '\n' || (matches!(c, '.' | '!' | '?') && next_space) {
            out.push(&text[start..i + c.len_utf8()]);
            start = i + c.len_utf8();
        }
    }
    out.push(&text[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

fn delay_in(sentence: &str) -> Option<Duration> {
    let c = DELAY.captures(sentence)?;
    let n = match c[1].to_ascii_lowercase().as_str() {
        "a" | "an" | "one" => 1.0,
        "two" => 2.0,
        "three" => 3.0,
        "four" => 4.0,
        "five" => 5.0,
        "ten" => 10.0,
        "fifteen" => 15.0,
        "twenty" => 20.0,
        "thirty" => 30.0,
        "forty" => 40.0,
        "sixty" => 60.0,
        h if h.starts_with("half") => 0.5,
        num => num.parse::<f64>().ok()?,
    };
    let unit = c[2].to_ascii_lowercase();
    let secs = match unit.as_bytes()[0] {
        b'h' => 3600.0,
        b'm' => 60.0,
        _ => 1.0,
    };
    Some(Duration::from_secs_f64(n * secs))
}

/// The last promise in `text` to come back later, if any.
pub fn promise(text: &str) -> Option<Promise> {
    let mut found = None;
    for s in sentences(text) {
        if !COME_BACK.is_match(s) || OFFER.is_match(s) || s.ends_with('?') {
            continue;
        }
        let said = clip(s, MAX_REASON_CHARS);
        if let Some(delay) = delay_in(s) {
            found = Some(Promise::After { delay, said });
        } else if LATER.is_match(s) {
            found = Some(Promise::Unscheduled { said });
        }
    }
    found
}

/// The reminder for `Promise::Unscheduled`; `prefix` is how the client names the task-x tools
/// (empty for the agent's own tools, `mcp__task-x__` for Claude Code).
pub fn unscheduled_nudge(said: &str, prefix: &str) -> String {
    format!(
        "You said \"{said}\", but nothing is scheduled to wake you, so the task would end here and nothing would bring you back. To wait, call {prefix}{WAIT_UNTIL} now (until a command succeeds: a build, a job, a log line) or {prefix}{SCHEDULE} (after a delay), then end your turn. If you do not need to wait, give your final answer."
    )
}

/// `1h 5m`, `30m`, `45s`.
pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler() -> (Arc<Scheduler>, Arc<Mutex<Vec<Value>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        let emit: EmitFn = Arc::new(move |_, e| log.lock().unwrap().push(e));
        (Scheduler::new("t", emit, CancellationToken::new()), events)
    }

    fn no_ctx(_: CancellationToken) -> Result<Arc<ToolCtx>, ToolError> {
        Err(ToolError::new(ToolErrorKind::Failed, "no context"))
    }

    fn types(events: &Mutex<Vec<Value>>) -> Vec<String> {
        events.lock().unwrap().iter().map(|e| e["type"].as_str().unwrap().to_owned()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_wakeup_fires_with_its_prompt_after_the_delay() {
        let (s, events) = scheduler();
        let r = s.call(SCHEDULE, &json!({ "delaySeconds": 1800, "prompt": "Check the deploy of v2", "reason": "deploy" }), no_ctx).unwrap();
        assert_eq!((r["id"].as_str(), r["in"].as_str()), (Some("w1"), Some("30m")));
        assert_eq!(s.pending(), 1);
        assert_eq!(s.list()["pending"][0]["dueInSecs"], 1800);
        assert!(s.wait(|| false).await, "the wake-up fires");
        let fired = s.take_fired();
        assert_eq!(fired.len(), 1);
        assert!(fired[0].contains("Check the deploy of v2") && fired[0].contains("w1 (deploy)"), "{}", fired[0]);
        assert_eq!(s.pending(), 0);
        assert_eq!(types(&events), ["wakeupScheduled", "wakeupFired"]);
    }

    #[tokio::test(start_paused = true)]
    async fn wakeups_check_their_input_and_limit() {
        let (s, _) = scheduler();
        for bad in [json!({ "delaySeconds": 5, "prompt": "x" }), json!({ "delaySeconds": 90000, "prompt": "x" }), json!({ "delaySeconds": 60, "prompt": " " })] {
            assert_eq!(s.call(SCHEDULE, &bad, no_ctx).unwrap_err().kind, ToolErrorKind::InvalidInput, "{bad}");
        }
        assert!(s.call(SCHEDULE, &json!({ "delaySeconds": 5, "prompt": "x" }), no_ctx).unwrap_err().message.contains("sleep"));
        for _ in 0..MAX_PENDING {
            s.call(SCHEDULE, &json!({ "delaySeconds": 60, "prompt": "x" }), no_ctx).unwrap();
        }
        assert!(s.call(SCHEDULE, &json!({ "delaySeconds": 60, "prompt": "x" }), no_ctx).unwrap_err().message.contains("cancel one"));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_wakeups_never_fire() {
        let (s, events) = scheduler();
        s.call(SCHEDULE, &json!({ "delaySeconds": 60, "prompt": "a" }), no_ctx).unwrap();
        s.call(SCHEDULE, &json!({ "delaySeconds": 120, "prompt": "b" }), no_ctx).unwrap();
        s.call(SCHEDULE, &json!({ "delaySeconds": 180, "prompt": "c" }), no_ctx).unwrap();
        assert_eq!(s.call(CANCEL, &json!({ "id": "w1" }), no_ctx).unwrap()["cancelled"], json!(["w1"]));
        assert_eq!(s.call(CANCEL, &json!({ "id": "w1" }), no_ctx).unwrap_err().kind, ToolErrorKind::NotFound);
        assert_eq!(s.list()["pending"][0]["id"], "w2", "soonest first");
        assert_eq!(s.call(CANCEL, &json!({ "id": "all" }), no_ctx).unwrap()["cancelled"], json!(["w2", "w3"]));
        assert!(!s.wait(|| false).await, "nothing left to wait for");
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert!(!s.has_fired());
        assert_eq!(types(&events).iter().filter(|t| *t == "wakeupCancelled").count(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_message_or_the_task_ending_interrupts_the_sleep() {
        let (s, events) = scheduler();
        s.call(SCHEDULE, &json!({ "delaySeconds": 3600, "prompt": "later" }), no_ctx).unwrap();
        assert!(s.wait(|| true).await, "a waiting message wakes the task");
        assert!(!s.has_fired());
        s.cancel_all();
        assert_eq!(s.pending(), 0);
        assert_eq!(events.lock().unwrap().last().unwrap()["by"], "taskEnded");
        let (s2, _) = scheduler();
        s2.call(SCHEDULE, &json!({ "delaySeconds": 3600, "prompt": "later" }), no_ctx).unwrap();
        s2.cancel.cancel();
        assert!(!s2.wait(|| false).await, "a cancelled task stops sleeping");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_runs_in_the_background_and_reports() {
        let c = crate::tools::tests::ctx(&[("a.txt", "x")]);
        let (s, events) = scheduler();
        let make = |stop: CancellationToken| {
            Ok(Arc::new(ToolCtx {
                root: c.root.clone(),
                workspace: c.workspace.clone(),
                cancel: stop,
                seen: c.seen.clone(),
                edits: c.edits.clone(),
                task_id: None,
            }))
        };
        let r = s.call(WAIT_UNTIL, &json!({ "command": ["sh", "-c", "echo up"], "match": "up", "reason": "server" }), make).unwrap();
        assert_eq!((r["background"].as_bool(), r["id"].as_str()), (Some(true), Some("w1")), "{r}");
        assert!(tokio::time::timeout(Duration::from_secs(30), s.wait(|| false)).await.unwrap());
        let fired = s.take_fired();
        assert!(fired[0].contains("w1 (server) passed"), "{}", fired[0]);
        assert!(events.lock().unwrap().iter().any(|e| e["type"] == "wakeupFired" && e["met"] == true));
        assert_eq!(s.call(WAIT_UNTIL, &json!({ "command": [] }), make).unwrap_err().kind, ToolErrorKind::InvalidInput, "checked before it is scheduled");
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn promises_to_come_back_are_found() {
        let after = |t: &str| match promise(t) {
            Some(Promise::After { delay, .. }) => Some(delay.as_secs()),
            _ => None,
        };
        assert_eq!(after("The build runs. I'll check again in 30 minutes."), Some(1800));
        assert_eq!(after("I will come back after an hour to see the results."), Some(3600));
        assert_eq!(after("I'll look at it in half an hour."), Some(1800));
        assert_eq!(after("I’ll check the job in 1.5 hours."), Some(5400));
        assert_eq!(after("I'm going to wait 10 min and check the deploy."), Some(600));
        assert!(matches!(
            promise("Started the run. I'll come back when the first optimisation reports in."),
            Some(Promise::Unscheduled { said }) if said == "I'll come back when the first optimisation reports in."
        ));
        assert!(matches!(promise("I'll report when they finish."), Some(Promise::Unscheduled { .. })));
        for none in [
            "Done. The tests pass.",
            "I'll check the config file next.",
            "If you want, I'll check again in 30 minutes.",
            "Should I check back in an hour?",
            "This will retry in 5 minutes on its own.",
        ] {
            assert_eq!(promise(none), None, "{none}");
        }
        assert!(unscheduled_nudge("x", "mcp__task-x__").contains("mcp__task-x__wait_until"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_promised_wakeup_is_scheduled_within_limits() {
        let (s, _) = scheduler();
        let r = s.schedule_promised(Duration::from_secs(1), "I'll check in a second.").unwrap();
        assert_eq!(r["in"], "10s", "at least the shortest wake-up");
        assert!(s.wait(|| false).await);
        assert!(s.take_fired()[0].contains("You said: \"I'll check in a second.\""));
    }

    #[test]
    fn durations_read_well() {
        assert_eq!([5, 60, 90, 1800, 3600, 5400].map(|s| human(Duration::from_secs(s))), ["5s", "1m", "1m 30s", "30m", "1h", "1h 30m"]);
    }
}
