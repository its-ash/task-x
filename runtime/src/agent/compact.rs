//! Context compression, in three stages with no extra model call. The agent loop runs them in order
//! and stops as soon as the history fits:
//! 1. `prune`: old tool results shrink. A file read that a later read or edit of the same file
//!    replaced becomes a stub, a result repeated later becomes a stub, and a large output keeps
//!    its head and tail. Errors and the recent turns are never touched.
//! 2. `compact`: the middle of the history becomes a deterministic progress summary. The original
//!    task, the most recent turns, files changed, and the latest unresolved errors are kept.
//! 3. `drop_retrieval`: the repository excerpts that were added to the first request go; the
//!    agent can search and read again.

use crate::provider::{ContentPart, Message, Role};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

pub const CHARS_PER_TOKEN: usize = 4;

/// Rough tokens for a text of `chars` characters.
pub fn chars_to_tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}
const KEEP_TAIL: usize = 6;
const MAX_ERRORS: usize = 5;
const MAX_ERROR_CHARS: usize = 1_500;
const MAX_NOTE_CHARS: usize = 300;
/// Marks compressed content so it is never compressed twice.
pub const MARK: &str = "[context compressed]";
const MIN_DUPLICATE_CHARS: usize = 200;
const MAX_OLD_RESULT_CHARS: usize = 2_000;
const HEAD_CHARS: usize = 1_200;
const TAIL_CHARS: usize = 400;
const READ_TOOLS: &[&str] = &["read_file", "read_range", "read_symbol", "get_file_outline", "Read"];
const WRITE_TOOLS: &[&str] = &[
    "replace_text",
    "replace_all",
    "insert_at_line",
    "delete_range",
    "create_file",
    "delete_file",
    "rename_file",
    "Write",
    "Edit",
    "MultiEdit",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub parts: usize,
    pub chars_saved: usize,
}

pub fn estimate_tokens(system: &str, messages: &[Message]) -> usize {
    let mut chars = system.len();
    for m in messages {
        for p in &m.content {
            chars += match p {
                ContentPart::Text { text } => text.len(),
                ContentPart::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentPart::ToolResult { content, .. } => content.len(),
                // Providers bill an image at roughly 1600 tokens, whatever its base64 size.
                ContentPart::Image { .. } => 1_600 * CHARS_PER_TOKEN,
            };
        }
    }
    chars / CHARS_PER_TOKEN
}

fn boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The first `HEAD_CHARS` and last `TAIL_CHARS` of a long output.
fn head_tail(name: &str, content: &str) -> String {
    let head = &content[..boundary(content, HEAD_CHARS)];
    let tail = &content[boundary(content, content.len() - TAIL_CHARS)..];
    format!(
        "{MARK} {name} output cut from {} to {} characters; run it again for the full result.\n{head}\n…\n{tail}",
        content.len(),
        HEAD_CHARS + TAIL_CHARS
    )
}

fn files_of(name: &str, input: &serde_json::Value) -> Vec<String> {
    let mut v: Vec<String> = ["path", "file_path", "from", "to"].iter().filter_map(|k| input[*k].as_str().map(str::to_owned)).collect();
    if name == "apply_patch" {
        v.extend(input["patch"].as_str().unwrap_or("").lines().filter_map(|l| l.strip_prefix("+++ b/").map(str::to_owned)));
    }
    v
}

/// Stage 1: shrinks old tool results (everything before the last `KEEP_TAIL` messages).
pub fn prune(messages: &mut [Message]) -> Pruned {
    let n = messages.len();
    if n <= KEEP_TAIL + 1 {
        return Pruned::default();
    }
    let recent = n - KEEP_TAIL;
    // tool call id → (tool, files, index of the message with the call)
    let mut calls: HashMap<&str, (&str, Vec<String>, usize)> = HashMap::new();
    let mut last_touch: HashMap<String, usize> = HashMap::new();
    let mut last_same: HashMap<u64, usize> = HashMap::new();
    let hash = |c: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        c.hash(&mut h);
        h.finish()
    };
    for (i, m) in messages.iter().enumerate() {
        for p in &m.content {
            match p {
                ContentPart::ToolUse { id, name, input } => {
                    let files = files_of(name, input);
                    if READ_TOOLS.contains(&name.as_str()) || WRITE_TOOLS.contains(&name.as_str()) || name == "apply_patch" {
                        for f in &files {
                            last_touch.insert(f.clone(), i);
                        }
                    }
                    calls.insert(id, (name, files, i));
                }
                ContentPart::ToolResult { content, .. } if content.len() >= MIN_DUPLICATE_CHARS => {
                    last_same.insert(hash(content), i);
                }
                _ => {}
            }
        }
    }
    let calls: HashMap<String, (String, Vec<String>, usize)> =
        calls.into_iter().map(|(k, (n, f, i))| (k.to_owned(), (n.to_owned(), f, i))).collect();
    let mut out = Pruned::default();
    for (i, m) in messages.iter_mut().enumerate().take(recent).skip(1) {
        for p in &mut m.content {
            let ContentPart::ToolResult { tool_use_id, content, is_error: false, .. } = p else { continue };
            if content.starts_with(MARK) {
                continue;
            }
            let (name, files, at) = calls.get(tool_use_id).cloned().unwrap_or_default();
            let superseded = READ_TOOLS.contains(&name.as_str()) && files.iter().any(|f| last_touch.get(f).is_some_and(|&j| j > at));
            let replacement = if superseded {
                Some(format!("{MARK} {name} of {}: a later read or edit of this file replaced it.", files.join(", ")))
            } else if content.len() >= MIN_DUPLICATE_CHARS && last_same.get(&hash(content)).is_some_and(|&j| j > i) {
                Some(format!("{MARK} same output as a later {name} call."))
            } else if content.len() > MAX_OLD_RESULT_CHARS {
                Some(head_tail(&name, content))
            } else {
                None
            };
            if let Some(r) = replacement.filter(|r| r.len() < content.len()) {
                out.chars_saved += content.len() - r.len();
                out.parts += 1;
                *content = r;
            }
        }
    }
    out
}

/// Stage 3: removes the repository excerpts added to the first request. Returns the characters saved.
pub fn drop_retrieval(messages: &mut [Message]) -> usize {
    let Some(first) = messages.first_mut() else { return 0 };
    let mut saved = 0;
    for p in &mut first.content {
        let ContentPart::Text { text } = p else { continue };
        let (Some(a), Some(b)) = (text.find("<repository_context>"), text.find("</repository_context>")) else { continue };
        let end = b + "</repository_context>".len();
        let note = format!("{MARK} The repository excerpts for this request were removed; search and read files with the tools.");
        if end - a > note.len() {
            saved += end - a - note.len();
            text.replace_range(a..end, &note);
        }
    }
    saved
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut cut = n;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// Compacts `messages` in place if needed. Returns how many messages were folded into the summary.
pub fn compact(messages: &mut Vec<Message>, changed_files: &[String]) -> usize {
    if messages.len() <= KEEP_TAIL + 2 {
        return 0;
    }
    // The tail must start with an assistant message so its tool calls keep their results.
    let mut start = messages.len() - KEEP_TAIL;
    while start > 1 && messages[start].role != Role::Assistant {
        start -= 1;
    }
    if start <= 1 {
        return 0;
    }
    let middle: Vec<Message> = messages.drain(1..start).collect();
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    let mut notes = Vec::new();
    let mut names = std::collections::HashMap::new();
    for m in &middle {
        for p in &m.content {
            match p {
                ContentPart::ToolUse { id, name, input } => {
                    names.insert(id.clone(), name.clone());
                    let target = ["path", "query", "pattern", "name", "glob"].iter().find_map(|k| input[*k].as_str()).unwrap_or("");
                    calls.push(format!("{name}({})", clip(target, 80)));
                }
                ContentPart::ToolResult { tool_use_id, content, is_error: true, .. } => {
                    let n = names.get(tool_use_id).cloned().unwrap_or_default();
                    errors.push(format!("{n}: {}", clip(content, MAX_ERROR_CHARS)));
                }
                ContentPart::Text { text } if m.role == Role::Assistant && !text.trim().is_empty() => {
                    notes.push(clip(text.trim(), MAX_NOTE_CHARS))
                }
                _ => {}
            }
        }
    }
    let mut summary = format!(
        "<progress_summary>\nEarlier steps were compacted to save context. {} messages folded.\nTool calls so far: {}\n",
        middle.len(),
        if calls.is_empty() { "none".into() } else { calls.join(", ") }
    );
    if !changed_files.is_empty() {
        summary.push_str(&format!("Files changed so far: {}\n", changed_files.join(", ")));
    }
    if !notes.is_empty() {
        summary.push_str("Your earlier notes:\n");
        for n in notes.iter().rev().take(5).rev() {
            summary.push_str(&format!("- {n}\n"));
        }
    }
    if !errors.is_empty() {
        summary.push_str("Recent errors (may still be unresolved):\n");
        for e in errors.iter().rev().take(MAX_ERRORS).rev() {
            summary.push_str(&format!("- {e}\n"));
        }
    }
    summary.push_str("Re-read files before editing them; earlier file contents are no longer in context.\n</progress_summary>");
    if let Some(first) = messages.first_mut() {
        // Drop an older summary so they do not stack.
        first.content.retain(|p| !matches!(p, ContentPart::Text { text } if text.starts_with("<progress_summary>")));
        first.content.push(ContentPart::Text { text: summary });
    }
    middle.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(t: &str) -> Message {
        Message { role: Role::User, content: vec![ContentPart::Text { text: t.into() }] }
    }

    fn call(id: &str, path: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse { id: id.into(), name: "read_file".into(), input: json!({ "path": path }) }],
        }
    }

    fn result(id: &str, err: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                tool_name: None,
                content: if err { "boom".into() } else { "x".repeat(400) },
                is_error: err,
            }],
        }
    }

    fn result_of(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult { tool_use_id: id.into(), tool_name: None, content: content.into(), is_error: false }],
        }
    }

    fn text_of(m: &Message) -> &str {
        match &m.content[0] {
            ContentPart::ToolResult { content, .. } => content,
            _ => "",
        }
    }

    #[test]
    fn prunes_superseded_reads_duplicates_and_long_outputs_but_not_recent_turns() {
        let big = "line of output\n".repeat(400);
        let excerpts = "src/x.rs:1-9\n".repeat(40);
        let mut m = vec![user(&format!("<repository_context>\n{excerpts}</repository_context>\n\nTASK"))];
        m.push(call("r1", "a.rs"));
        m.push(result_of("r1", &"old a.rs ".repeat(100)));
        m.push(call("r2", "a.rs"));
        m.push(result_of("r2", "new a.rs"));
        m.push(Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse { id: "t1".into(), name: "run_tests".into(), input: json!({}) }],
        });
        m.push(result_of("t1", &big));
        m.push(Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse { id: "t2".into(), name: "run_tests".into(), input: json!({}) }],
        });
        m.push(result_of("t2", &big));
        for i in 0..4 {
            m.push(call(&format!("k{i}"), "z.rs"));
            m.push(result_of(&format!("k{i}"), &"recent ".repeat(500)));
        }
        let before = estimate_tokens("", &m);
        let p = prune(&mut m);
        // 17 messages; the last 6 are kept. Before them: a superseded read (2), a duplicate (6), a long
        // output (8), and a read of z.rs that later reads replaced (10).
        assert_eq!(p.parts, 4);
        assert!(text_of(&m[2]).contains("a later read or edit of this file replaced it"));
        assert_eq!(text_of(&m[4]), "new a.rs", "short results stay");
        assert!(text_of(&m[6]).contains("same output as a later run_tests call"));
        assert!(
            text_of(&m[8]).starts_with(MARK)
                && text_of(&m[8]).contains("cut from 6000 to 1600 characters")
                && text_of(&m[8]).ends_with("line of output\n")
        );
        assert!(text_of(&m[10]).contains("read_file of z.rs"));
        assert!(text_of(&m[m.len() - 1]).starts_with("recent"), "the last turns are kept as they are");
        assert!(estimate_tokens("", &m) < before);
        assert_eq!(prune(&mut m).parts, 0, "pruning twice changes nothing");
        assert!(drop_retrieval(&mut m) > 0);
        assert!(matches!(&m[0].content[0], ContentPart::Text { text } if text.contains(MARK) && text.ends_with("TASK")));
    }

    #[test]
    fn folds_middle_keeps_task_tail_and_errors() {
        let mut m = vec![user("TASK: fix the bug")];
        for i in 0..10 {
            m.push(call(&format!("c{i}"), &format!("f{i}.rs")));
            m.push(result(&format!("c{i}"), i == 2));
        }
        let before = estimate_tokens("", &m);
        let folded = compact(&mut m, &["f1.rs".into()]);
        assert!(folded > 0 && estimate_tokens("", &m) < before);
        assert_eq!(m[1].role, Role::Assistant, "tail starts at an assistant turn");
        let ContentPart::Text { text } = &m[0].content[1] else { panic!() };
        assert!(text.contains("read_file(f0.rs)") && text.contains("read_file: boom") && text.contains("Files changed so far: f1.rs"));
        assert!(matches!(&m[0].content[0], ContentPart::Text { text } if text == "TASK: fix the bug"));
        // Every remaining tool result still has its tool call.
        let uses: Vec<String> = m
            .iter()
            .flat_map(|x| &x.content)
            .filter_map(|p| if let ContentPart::ToolUse { id, .. } = p { Some(id.clone()) } else { None })
            .collect();
        for p in m.iter().flat_map(|x| &x.content) {
            if let ContentPart::ToolResult { tool_use_id, .. } = p {
                assert!(uses.contains(tool_use_id));
            }
        }
        compact(&mut m, &[]);
        assert_eq!(
            m[0].content.iter().filter(|p| matches!(p, ContentPart::Text { text } if text.starts_with("<progress_summary>"))).count(),
            1
        );
    }
}
