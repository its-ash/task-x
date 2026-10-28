//! Project memory: `.task-x/memory.md`, short notes the agent keeps about the project (build and
//! test commands, layout, conventions, known traps). Every new task starts with them, so the agent
//! does not explore the repository again to learn the same facts.

use super::{Permission, ToolCtx, ToolDef, ToolError, ToolResult, parse, tool};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const MEMORY_FILE: &str = ".task-x/memory.md";
/// The whole file stays small: it is sent with every task.
pub const MAX_MEMORY_CHARS: usize = 6_000;
const MAX_NOTE_CHARS: usize = 400;
const HEADER: &str = "# Project memory\n\nNotes task-x keeps about this project. Edit or delete them freely.\n";

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "remember",
        "Save a short, lasting fact about this project for later tasks: how to build or test, where things live, a convention, or a trap. Use note for one new fact. Use content to rewrite the whole memory (to fix a wrong fact or make room). Do not save task progress or secrets.",
        Permission::Write,
        || {
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "One fact, one line" },
                    "content": { "type": "string", "description": "The whole memory, as a Markdown list" }
                },
                "additionalProperties": false
            })
        },
        remember,
    )]
}

fn path(root: &Path) -> PathBuf {
    root.join(MEMORY_FILE)
}

/// The saved memory of a workspace, if there is any.
pub fn load(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path(root)).ok()?;
    let body = text.trim();
    (!body.is_empty()).then(|| body.chars().take(MAX_MEMORY_CHARS).collect())
}

/// The memory as a system-prompt block, or an empty string.
pub fn prompt_block(root: &Path) -> String {
    load(root).map_or_else(String::new, |m| {
        format!(
            "<project_memory>\n{m}\n</project_memory>\nThese are notes from earlier tasks in this project ({MEMORY_FILE}). Use them instead of exploring again, but trust the code over them; fix a wrong note with remember."
        )
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct In {
    #[serde(default, alias = "fact")]
    note: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

fn remember(c: &ToolCtx, input: Value) -> ToolResult {
    let i: In = parse(input)?;
    let file = path(&c.root);
    let text = match (i.note.map(|n| n.trim().replace('\n', " ")), i.content) {
        (Some(n), None) if !n.is_empty() => {
            if n.chars().count() > MAX_NOTE_CHARS {
                return Err(ToolError::invalid(format!("a note is one fact of up to {MAX_NOTE_CHARS} characters")));
            }
            let old = std::fs::read_to_string(&file).unwrap_or_else(|_| HEADER.to_owned());
            if old.lines().any(|l| l.trim_start_matches("- ").trim() == n) {
                return Ok(json!({ "path": MEMORY_FILE, "unchanged": true }));
            }
            format!("{}\n- {n}\n", old.trim_end())
        }
        (None, Some(all)) => format!("{HEADER}\n{}\n", all.trim()),
        _ => return Err(ToolError::invalid("give note (one new fact) or content (the whole memory), not both")),
    };
    if text.chars().count() > MAX_MEMORY_CHARS {
        return Err(ToolError::invalid(format!(
            "the memory would pass {MAX_MEMORY_CHARS} characters; rewrite it shorter with content (merge or drop old notes)"
        )));
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::io(e, ".task-x"))?;
    }
    std::fs::write(&file, &text).map_err(|e| ToolError::io(e, MEMORY_FILE))?;
    Ok(json!({ "path": MEMORY_FILE, "chars": text.chars().count(), "limit": MAX_MEMORY_CHARS }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn notes_are_kept_once_and_come_back_in_the_prompt() {
        let c = ctx(&[("a.txt", "a\n")]);
        assert_eq!(prompt_block(&c.root), "", "no memory yet");
        run(&c, "remember", json!({ "note": "Tests: cargo test --manifest-path runtime/Cargo.toml" })).unwrap();
        let again = run(&c, "remember", json!({ "note": "Tests: cargo test --manifest-path runtime/Cargo.toml" })).unwrap();
        assert_eq!(again["unchanged"], true, "the same fact is not saved twice");
        run(&c, "remember", json!({ "note": "The webview is extension/media/chat.js" })).unwrap();
        let block = prompt_block(&c.root);
        assert!(block.contains("<project_memory>") && block.contains("cargo test") && block.contains("chat.js"), "{block}");
        run(&c, "remember", json!({ "content": "- Only this" })).unwrap();
        assert!(!prompt_block(&c.root).contains("cargo test"), "content rewrites the memory");
        let long = "x".repeat(MAX_MEMORY_CHARS);
        assert!(run(&c, "remember", json!({ "content": long })).is_err());
        assert!(run(&c, "remember", json!({})).is_err());
    }
}
