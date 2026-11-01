//! `repo_map`: the ranked map of the project's most-used files and their definitions.

use super::{Permission, ToolCtx, ToolDef, ToolResult, parse, tool};
use crate::context::repomap;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "repo_map",
        "A map of the project: the files the most code depends on (ranked over the import and reference graph), each with its main definitions and line numbers. Give focus (paths) to rank the code around those files first. Use it to find where to look in an unfamiliar project.",
        Permission::Read,
        || {
            json!({ "type": "object", "properties": {
                "focus": { "type": "array", "items": { "type": "string" }, "description": "Paths to lean the ranking towards" },
                "budgetTokens": { "type": "integer", "minimum": 100, "maximum": repomap::MAX_BUDGET }
            }, "additionalProperties": false })
        },
        run,
    )]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct In {
    #[serde(default)]
    focus: Vec<String>,
    #[serde(default)]
    budget_tokens: Option<usize>,
}

fn run(c: &ToolCtx, input: Value) -> ToolResult {
    let i: In = parse(input)?;
    let map = c.with_workspace(|w| repomap::render(w, &i.focus, i.budget_tokens.unwrap_or(2_000)))?;
    Ok(json!({ "map": if map.is_empty() { "No file with definitions is indexed.".to_owned() } else { map } }))
}
