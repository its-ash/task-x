//! The agent's plan as a checklist (`todo_write`). The tool only checks the list and returns it:
//! the chat draws the newest list from the call's input, and the model keeps it current by sending
//! the whole list again as items start and finish.

use super::{Permission, ToolCtx, ToolDef, ToolError, ToolResult, parse, tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_ITEMS: usize = 30;
const MAX_ITEM_CHARS: usize = 300;

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "todo_write",
        "Keep your plan for this task as a checklist the user sees. Call it with the whole list: at the start of a task with three or more steps, when a step starts (in_progress, one at a time), and when it is done (completed). Skip it for one-step tasks.",
        Permission::Read,
        || {
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "maxItems": MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "The step, as a short imperative sentence" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            })
        },
        todo_write,
    )]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Pending,
    InProgress,
    Completed,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Item {
    #[serde(alias = "text", alias = "title")]
    content: String,
    status: State,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct In {
    todos: Vec<Item>,
}

fn todo_write(_: &ToolCtx, input: Value) -> ToolResult {
    let i: In = parse(input)?;
    if i.todos.len() > MAX_ITEMS {
        return Err(ToolError::invalid(format!("keep the list to {MAX_ITEMS} items or fewer")));
    }
    if i.todos.iter().any(|t| t.content.trim().is_empty() || t.content.chars().count() > MAX_ITEM_CHARS) {
        return Err(ToolError::invalid(format!("each item needs text of 1 to {MAX_ITEM_CHARS} characters")));
    }
    let open = i.todos.iter().filter(|t| t.status != State::Completed).count();
    let busy = i.todos.iter().filter(|t| t.status == State::InProgress).count();
    Ok(json!({
        "todos": i.todos,
        "open": open,
        "note": if busy > 1 { "Keep one item in_progress at a time." } else if open == 0 { "All items are done." } else { "Saved." },
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use serde_json::json;

    #[test]
    fn checks_and_returns_the_list() {
        let c = ctx(&[("a.txt", "a\n")]);
        let r = run(&c, "todo_write", json!({ "todos": [{ "content": "Read the parser", "status": "completed" }, { "content": "Add color", "status": "in_progress" }] })).unwrap();
        assert_eq!((r["open"].as_u64(), r["todos"][1]["status"].as_str()), (Some(1), Some("in_progress")));
        assert!(run(&c, "todo_write", json!({ "todos": [{ "content": " ", "status": "pending" }] })).is_err());
        assert!(run(&c, "todo_write", json!({ "todos": [{ "content": "x", "status": "maybe" }] })).is_err());
    }
}
