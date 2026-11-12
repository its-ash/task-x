//! Session controls on a saved task: the checkpoint list, fork, rewind, manual compaction, and the
//! cost report. They work on the task record; `agent/rewind` also undoes the edits made after the
//! checkpoint (in the server, which owns the edit log).

use super::compact;
use super::store::{Status, Store, TaskRecord, new_id, now_ms};
use crate::provider::{ContentPart, Role};
use serde_json::{Value, json};

/// The prompts of a task, oldest first: `index` is what `agent/fork` and `agent/rewind` take.
pub fn checkpoints(r: &TaskRecord) -> Value {
    json!(
        r.checkpoints
            .iter()
            .enumerate()
            .map(|(i, c)| json!({ "index": i, "prompt": c.prompt, "messages": c.messages, "operations": c.operations, "createdMs": c.created_ms }))
            .collect::<Vec<_>>()
    )
}

/// The text of the last assistant message, the task's answer after a cut.
fn last_answer(r: &TaskRecord) -> String {
    r.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .and_then(|m| m.content.iter().find_map(|p| if let ContentPart::Text { text } = p { Some(text.clone()) } else { None }))
        .unwrap_or_default()
}

/// Cuts a record back to the state before checkpoint `at`. Returns the edit operations made since,
/// newest first, for the caller to undo.
pub fn cut(r: &mut TaskRecord, at: usize) -> Result<Vec<usize>, String> {
    let cp = r.checkpoints.get(at).cloned().ok_or_else(|| format!("task {} has no checkpoint {at} (it has {})", r.id, r.checkpoints.len()))?;
    let ops: Vec<usize> = r.operations.get(cp.operations..).unwrap_or_default().iter().rev().copied().collect();
    r.messages.truncate(cp.messages);
    r.operations.truncate(cp.operations);
    r.checkpoints.truncate(at);
    r.final_text = last_answer(r);
    r.error = None;
    if matches!(r.status, Status::Running | Status::Sleeping) {
        r.status = Status::Cancelled;
    }
    r.updated_ms = now_ms();
    Ok(ops)
}

/// A copy of a task under a new id, optionally cut back to before checkpoint `at`. The copy owns no
/// edits: undoing them stays with the original.
pub fn fork(store: &Store, id: &str, at: Option<usize>) -> Result<TaskRecord, String> {
    let mut r = store.load(id).ok_or_else(|| format!("unknown task {id}"))?;
    if let Some(at) = at {
        cut(&mut r, at)?;
    }
    r.forked_from = Some(r.id.clone());
    r.id = new_id();
    r.title = format!("Fork: {}", r.title.trim_start_matches("Fork: "));
    r.operations.clear();
    for c in &mut r.checkpoints {
        c.operations = 0;
    }
    if matches!(r.status, Status::Running | Status::Sleeping) {
        r.status = Status::Cancelled;
    }
    (r.created_ms, r.updated_ms) = (now_ms(), now_ms());
    store.save(&r).map_err(|e| format!("could not save the fork: {e}"))?;
    Ok(r)
}

/// Compresses a saved conversation now (`/compact`), with the same stages the loop uses.
pub fn compact_record(r: &mut TaskRecord) -> Value {
    let before = compact::estimate_tokens(&r.system, &r.messages);
    let pruned = compact::prune(&mut r.messages);
    let files: Vec<String> = r.changed_files.iter().cloned().collect();
    let folded = compact::compact(&mut r.messages, &files);
    let dropped = compact::drop_retrieval(&mut r.messages);
    let after = compact::estimate_tokens(&r.system, &r.messages);
    if pruned.parts > 0 || folded > 0 || dropped > 0 {
        r.metrics.compactions += 1;
        r.updated_ms = now_ms();
    }
    json!({ "taskId": r.id, "pruned": pruned.parts, "folded": folded, "droppedRetrieval": dropped > 0, "beforeTokens": before, "afterTokens": after })
}

/// What a task cost (`/cost`): tokens, money, model time, and subagents.
pub fn cost(r: &TaskRecord) -> Value {
    let m = &r.metrics;
    json!({
        "taskId": r.id,
        "model": r.model,
        "usage": r.usage,
        "costUsd": m.cost_usd,
        "unpricedTokens": m.unpriced_tokens,
        "billedTokens": m.billed_tokens,
        "localTokens": m.local_tokens,
        "modelCalls": m.model_calls,
        "modelMs": m.model_ms,
        "toolMs": m.tool_ms,
        "toolCalls": r.tool_calls,
        "iterations": r.iterations,
        "prompts": r.checkpoints.len(),
        "subagents": m.subagents,
        "subagentCostUsd": m.subagent_cost_usd,
        "subagentTokens": m.subagent_tokens,
        "compactions": m.compactions,
    })
}

#[cfg(test)]
mod tests {
    use super::super::store::{Checkpoint, tests::record};
    use super::*;
    use crate::provider::Message;

    fn msg(role: Role, text: &str) -> Message {
        Message { role, content: vec![ContentPart::Text { text: text.into() }] }
    }

    fn two_turns() -> TaskRecord {
        let mut r = record("t1", Status::Completed);
        r.messages = vec![msg(Role::User, "first"), msg(Role::Assistant, "one"), msg(Role::User, "second"), msg(Role::Assistant, "two")];
        r.operations = vec![3, 4, 7];
        r.checkpoints = vec![
            Checkpoint { messages: 0, operations: 0, prompt: "first".into(), created_ms: 1 },
            Checkpoint { messages: 2, operations: 2, prompt: "second".into(), created_ms: 2 },
        ];
        r.final_text = "two".into();
        r
    }

    #[test]
    fn rewinding_cuts_the_conversation_and_returns_the_later_edits() {
        let mut r = two_turns();
        assert_eq!(checkpoints(&r)[1]["prompt"], "second");
        assert_eq!(cut(&mut r, 1).unwrap(), [7], "only the edits of the second prompt, newest first");
        assert_eq!((r.messages.len(), r.operations.as_slice(), r.checkpoints.len()), (2, [3, 4].as_slice(), 1));
        assert_eq!(r.final_text, "one");
        assert!(cut(&mut r, 5).is_err());
        let mut all = two_turns();
        assert_eq!(cut(&mut all, 0).unwrap(), [7, 4, 3]);
        assert!(all.messages.is_empty() && all.final_text.is_empty());
    }

    #[test]
    fn a_fork_is_a_new_task_without_edits() {
        let dir = std::env::temp_dir().join(format!("task-x-fork-{}", std::process::id()));
        let store = Store::new(Some(&dir));
        store.save(&two_turns()).unwrap();
        let f = fork(&store, "t1", Some(1)).unwrap();
        assert_ne!(f.id, "t1");
        assert_eq!((f.forked_from.as_deref(), f.messages.len(), f.operations.len()), (Some("t1"), 2, 0));
        assert!(f.title.starts_with("Fork: "));
        assert_eq!(store.load(&f.id).unwrap().messages.len(), 2, "the fork is saved");
        assert_eq!(store.load("t1").unwrap().messages.len(), 4, "the original is unchanged");
        let whole = fork(&store, "t1", None).unwrap();
        assert_eq!(whole.messages.len(), 4);
        assert!(fork(&store, "nope", None).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cost_and_compaction_report_on_the_record() {
        let mut r = two_turns();
        r.metrics.cost_usd = 0.25;
        let c = cost(&r);
        assert_eq!((c["costUsd"].as_f64(), c["prompts"].as_u64()), (Some(0.25), Some(2)));
        let out = compact_record(&mut r);
        assert!(out["beforeTokens"].as_u64().unwrap() >= out["afterTokens"].as_u64().unwrap());
    }
}
