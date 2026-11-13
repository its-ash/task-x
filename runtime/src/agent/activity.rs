//! What every running agent is doing right now, for tracking several at once: each top-level
//! task and each subagent (with its parent), its status, the tool it is running, its turn, tool
//! calls, and tokens. It is kept from the events the tasks already emit, so nothing in the loop
//! reports twice. Finished agents stay listed for `KEEP_FINISHED_MS`, so a short one is not missed.

use super::store::now_ms;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

/// How long a finished agent stays in the list.
const KEEP_FINISHED_MS: u64 = 10 * 60 * 1000;
/// At most this many agents are kept (the oldest finished ones go first).
const MAX_AGENTS: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    pub task_id: String,
    /// The task that started this one; `None` for a top-level task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The subagent's name (`scout-1`), or "lead" for a top-level task.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The task title, or the subagent's objective.
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `running`, `waitingApproval`, `waitingAnswer`, `sleeping`, or the end status
    /// (`completed`, `failed`, `cancelled`, ...).
    pub status: String,
    pub running: bool,
    /// The tool it runs now, with the few words that say what (`read_file src/a.rs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    pub iteration: u64,
    pub tool_calls: u64,
    pub tokens: u64,
    pub started_ms: u64,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
}

static AGENTS: LazyLock<Mutex<BTreeMap<String, AgentState>>> = LazyLock::new(Default::default);

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<String, AgentState>> {
    AGENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fresh(task_id: &str, parent_id: Option<String>, name: &str, title: &str) -> AgentState {
    let now = now_ms();
    AgentState {
        task_id: task_id.to_owned(),
        parent_id,
        name: name.to_owned(),
        role: None,
        agent: None,
        title: title.to_owned(),
        model: None,
        status: "running".into(),
        running: true,
        current_tool: None,
        iteration: 0,
        tool_calls: 0,
        tokens: 0,
        started_ms: now,
        updated_ms: now,
        ended_ms: None,
    }
}

/// A task starts. A subagent was already listed by its parent's `subagentStarted`, which named
/// its parent; that entry is kept and only filled in.
pub fn begin(task_id: &str, title: &str, model: &str) {
    let mut a = lock();
    prune(&mut a);
    let e = a.entry(task_id.to_owned()).or_insert_with(|| fresh(task_id, None, "lead", title));
    // A task continued with a new prompt starts again.
    if !e.running {
        *e = AgentState { parent_id: e.parent_id.clone(), name: e.name.clone(), ..fresh(task_id, None, &e.name, title) };
    }
    e.model.get_or_insert_with(|| model.to_owned());
    if e.title.is_empty() {
        e.title = title.to_owned();
    }
}

/// The few words that say what a call does: its path, command, pattern, or query.
fn hint(input: &Value) -> Option<String> {
    let v = ["path", "command", "pattern", "query", "objective", "url"].iter().find_map(|k| input.get(*k).filter(|v| !v.is_null()))?;
    let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned).replace('\n', " ");
    Some(if s.chars().count() > 60 { format!("{}…", s.chars().take(59).collect::<String>()) } else { s })
}

/// Updates the list from one event of `task_id`.
pub fn observe(task_id: &str, event: &Value) {
    let Some(kind) = event["type"].as_str() else { return };
    let mut a = lock();
    if kind == "subagentStarted" {
        let child = event["taskId"].as_str().unwrap_or_default();
        if !child.is_empty() {
            let mut s = fresh(child, Some(task_id.to_owned()), event["name"].as_str().unwrap_or("subagent"), event["objective"].as_str().unwrap_or_default());
            s.role = event["role"].as_str().map(str::to_owned);
            s.agent = event["agent"].as_str().map(str::to_owned);
            s.model = event["model"].as_str().map(str::to_owned);
            a.insert(child.to_owned(), s);
        }
        return;
    }
    if kind == "subagentFinished" {
        if let Some(s) = event["taskId"].as_str().and_then(|c| a.get_mut(c)) {
            finish(s, event["status"].as_str().unwrap_or("completed"));
        }
        return;
    }
    let Some(s) = a.get_mut(task_id) else { return };
    s.updated_ms = now_ms();
    match kind {
        "iteration" => {
            s.iteration = event["n"].as_u64().unwrap_or(s.iteration + 1);
            s.status = "running".into();
        }
        "toolStarted" => {
            let tool = event["tool"].as_str().unwrap_or_default();
            s.current_tool = Some(match hint(&event["input"]) {
                Some(h) => format!("{tool} {h}"),
                None => tool.to_owned(),
            });
            s.tool_calls += 1;
            s.status = "running".into();
        }
        "toolFinished" => s.current_tool = None,
        "approvalRequested" => {
            s.status = "waitingApproval".into();
            s.current_tool = event["tool"].as_str().map(str::to_owned);
        }
        "approvalResolved" => s.status = "running".into(),
        "sleeping" => s.status = "sleeping".into(),
        "awake" => s.status = "running".into(),
        "usage" => {
            let u = &event["usage"];
            s.tokens = u["inputTokens"].as_u64().unwrap_or(0) + u["outputTokens"].as_u64().unwrap_or(0);
        }
        "done" => {
            let status = event["summary"]["status"].as_str().unwrap_or("completed").to_owned();
            finish(s, &status);
        }
        _ => {}
    }
}

/// A task ended without its usual events (it crashed): it is listed with `status`.
pub fn ended(task_id: &str, status: &str) {
    if let Some(s) = lock().get_mut(task_id).filter(|s| s.running) {
        finish(s, status);
    }
}

/// A question to the user is open (`ask_user`); `false` when it is answered.
pub fn asking(task_id: &str, open: bool) {
    if let Some(s) = lock().get_mut(task_id) {
        s.status = if open { "waitingAnswer" } else { "running" }.into();
        s.updated_ms = now_ms();
    }
}

fn finish(s: &mut AgentState, status: &str) {
    let now = now_ms();
    s.status = status.to_owned();
    s.running = false;
    s.current_tool = None;
    s.updated_ms = now;
    s.ended_ms = Some(now);
}

fn prune(a: &mut BTreeMap<String, AgentState>) {
    let now = now_ms();
    a.retain(|_, s| s.running || s.ended_ms.is_some_and(|e| now.saturating_sub(e) < KEEP_FINISHED_MS));
    while a.len() > MAX_AGENTS {
        let Some(oldest) = a.values().filter(|s| !s.running).min_by_key(|s| s.ended_ms).map(|s| s.task_id.clone()) else { break };
        a.remove(&oldest);
    }
}

/// Every agent, running ones first, each followed by its subagents (`depth` says how deep).
pub fn snapshot() -> Vec<(usize, AgentState)> {
    let mut a = lock();
    prune(&mut a);
    let all: Vec<AgentState> = a.values().cloned().collect();
    drop(a);
    let mut roots: Vec<&AgentState> = all.iter().filter(|s| s.parent_id.as_ref().is_none_or(|p| !all.iter().any(|o| &o.task_id == p))).collect();
    roots.sort_by_key(|s| (!s.running, std::cmp::Reverse(s.started_ms)));
    let mut out = Vec::new();
    fn walk<'a>(s: &'a AgentState, depth: usize, all: &'a [AgentState], out: &mut Vec<(usize, AgentState)>) {
        out.push((depth, s.clone()));
        let mut kids: Vec<&AgentState> = all.iter().filter(|c| c.parent_id.as_deref() == Some(s.task_id.as_str())).collect();
        kids.sort_by_key(|c| c.started_ms);
        for k in kids {
            walk(k, depth + 1, all, out);
        }
    }
    for r in roots {
        walk(r, 0, &all, &mut out);
    }
    out
}

/// The list for `agent/active`: `{ agents: [AgentState + depth], running }`.
pub fn json() -> Value {
    let list = snapshot();
    let running = list.iter().filter(|(_, s)| s.running).count();
    let agents: Vec<Value> = list
        .into_iter()
        .map(|(d, s)| {
            let mut v = serde_json::to_value(s).unwrap_or_default();
            v["depth"] = d.into();
            v
        })
        .collect();
    serde_json::json!({ "agents": agents, "running": running })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracks_a_lead_and_its_subagents_from_their_events() {
        let lead = format!("lead-{}", std::process::id());
        let kid = format!("{lead}-scout");
        begin(&lead, "Fix the login bug", "claude-x");
        observe(&lead, &json!({ "type": "iteration", "n": 1 }));
        observe(&lead, &json!({ "type": "subagentStarted", "taskId": kid, "name": "scout-1", "role": "research", "objective": "find the bug", "model": "m" }));
        begin(&kid, "find the bug", "m");
        observe(&kid, &json!({ "type": "toolStarted", "tool": "read_file", "input": { "path": "src/login.rs" } }));
        observe(&lead, &json!({ "type": "approvalRequested", "tool": "run_command" }));
        observe(&kid, &json!({ "type": "usage", "usage": { "inputTokens": 100, "outputTokens": 20 } }));
        let snap = snapshot();
        let mine: Vec<&(usize, AgentState)> = snap.iter().filter(|(_, s)| s.task_id.starts_with(&lead)).collect();
        assert_eq!(mine.len(), 2);
        let (d0, l) = mine[0];
        let (d1, k) = mine[1];
        assert_eq!((*d0, l.name.as_str(), l.status.as_str(), l.iteration), (0, "lead", "waitingApproval", 1));
        assert_eq!((*d1, k.name.as_str(), k.parent_id.as_deref(), k.current_tool.as_deref(), k.tokens), (1, "scout-1", Some(lead.as_str()), Some("read_file src/login.rs"), 120));
        assert_eq!(k.role.as_deref(), Some("research"));
        observe(&lead, &json!({ "type": "subagentFinished", "taskId": kid, "status": "completed" }));
        observe(&lead, &json!({ "type": "done", "summary": { "status": "completed" } }));
        let v = json();
        let done: Vec<&Value> = v["agents"].as_array().unwrap().iter().filter(|a| a["taskId"].as_str().unwrap().starts_with(&lead)).collect();
        assert!(done.iter().all(|a| a["running"] == false && a["endedMs"].is_u64()), "finished agents stay listed for a while");
        assert_eq!(done[1]["depth"], 1);
    }
}
