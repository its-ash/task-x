//! Exports a task as one self-contained HTML file: the conversation (the user's prompts, the
//! answers, and every tool call with its input and result), the files the agent changed with
//! their diffs, and the task's timeline. No scripts, fonts, or network requests: the file opens
//! anywhere, and long parts fold away with `<details>`.

use super::store::TaskRecord;
use super::timeline::Snapshot;
use crate::provider::{ContentPart, Role};
use crate::tools::Operation;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Tool input and result text kept per call; the rest is cut with a note.
const MAX_TOOL_CHARS: usize = 20_000;
/// Diff text kept per changed file.
const MAX_DIFF_CHARS: usize = 60_000;
/// Where exports go by default, relative to the workspace root.
pub const EXPORT_DIR: &str = ".task-x/exports";

/// `<tag …>…</tag>` blocks the runtime wraps around context in user messages.
static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<([a-z_]+)(?:\s[^>]*)?>\n?(.*?)\n?</([a-z_]+)>").expect("block regex"));

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more characters not exported]", &s[..cut], s.len() - cut)
}

/// `2026-11-12 15:43 UTC` from epoch milliseconds.
pub fn date(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rest / 3600, (rest % 3600) / 60)
}

/// Inline Markdown on escaped text: `code`, **bold**, and http(s) links.
fn inline(s: &str) -> String {
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("code regex"));
    static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*]+)\*\*").expect("bold regex"));
    static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\((https?://[^)\s]+)\)").expect("link regex"));
    let e = esc(s);
    // Code spans first, kept out of the other rules by a placeholder.
    let mut spans = Vec::new();
    let e = CODE
        .replace_all(&e, |c: &regex::Captures| {
            spans.push(format!("<code>{}</code>", &c[1]));
            format!("\u{0}{}\u{0}", spans.len() - 1)
        })
        .into_owned();
    let e = BOLD.replace_all(&e, "<strong>$1</strong>").into_owned();
    let e = LINK.replace_all(&e, r#"<a href="$2" rel="noopener noreferrer">$1</a>"#).into_owned();
    let mut out = String::new();
    let mut parts = e.split('\u{0}');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    while let (Some(idx), Some(rest)) = (parts.next(), parts.next()) {
        out.push_str(idx.parse::<usize>().ok().and_then(|i| spans.get(i)).map_or("", String::as_str));
        out.push_str(rest);
    }
    out
}

/// A small Markdown renderer for answers: headings, fenced code, lists, tables, quotes, paragraphs.
pub fn markdown(text: &str) -> String {
    let mut out = String::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut String| {
        if !para.is_empty() {
            out.push_str(&format!("<p>{}</p>\n", para.iter().map(|l| inline(l)).collect::<Vec<_>>().join("<br>")));
            para.clear();
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        if let Some(lang) = t.strip_prefix("```") {
            flush(&mut para, &mut out);
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            let lang = lang.trim();
            let label = if lang.is_empty() { String::new() } else { format!(" data-lang=\"{}\"", esc(lang)) };
            out.push_str(&format!("<pre{label}><code>{}</code></pre>\n", esc(&code.join("\n"))));
            i += 1;
            continue;
        }
        if t.is_empty() {
            flush(&mut para, &mut out);
        } else if let Some((level, rest)) = (1..=6).rev().find_map(|n| t.strip_prefix(&format!("{} ", "#".repeat(n))).map(|r| (n, r))) {
            flush(&mut para, &mut out);
            out.push_str(&format!("<h{0}>{1}</h{0}>\n", (level + 2).min(6), inline(rest)));
        } else if t.starts_with('|') && lines.get(i + 1).is_some_and(|n| n.trim_start().starts_with('|') && n.contains("---")) {
            flush(&mut para, &mut out);
            let cells = |l: &str| l.trim().trim_matches('|').split('|').map(|c| inline(c.trim())).collect::<Vec<_>>();
            out.push_str("<table><thead><tr>");
            for c in cells(t) {
                out.push_str(&format!("<th>{c}</th>"));
            }
            out.push_str("</tr></thead><tbody>\n");
            i += 2;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                out.push_str("<tr>");
                for c in cells(lines[i]) {
                    out.push_str(&format!("<td>{c}</td>"));
                }
                out.push_str("</tr>\n");
                i += 1;
            }
            out.push_str("</tbody></table>\n");
            continue;
        } else if t.starts_with("- ") || t.starts_with("* ") || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
            flush(&mut para, &mut out);
            let ordered = !(t.starts_with("- ") || t.starts_with("* "));
            out.push_str(if ordered { "<ol>\n" } else { "<ul>\n" });
            while i < lines.len() {
                let l = lines[i].trim_start();
                let item = if ordered {
                    l.split_once(". ").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).map(|(_, r)| r)
                } else {
                    l.strip_prefix("- ").or_else(|| l.strip_prefix("* "))
                };
                match item {
                    Some(r) => out.push_str(&format!("<li>{}</li>\n", inline(r))),
                    None => break,
                }
                i += 1;
            }
            out.push_str(if ordered { "</ol>\n" } else { "</ul>\n" });
            continue;
        } else if let Some(q) = t.strip_prefix("> ").or_else(|| (t == ">").then_some("")) {
            flush(&mut para, &mut out);
            out.push_str(&format!("<blockquote>{}</blockquote>\n", inline(q)));
        } else {
            para.push(line);
        }
        i += 1;
    }
    flush(&mut para, &mut out);
    out
}

/// A user message: the request itself, with the context blocks the runtime added folded away.
fn user_text(text: &str) -> String {
    let mut out = String::new();
    let mut rest = String::new();
    let mut last = 0;
    for c in BLOCK.captures_iter(text) {
        let (Some(m), tag, end) = (c.get(0), &c[1], &c[3]) else { continue };
        if tag != end {
            continue;
        }
        rest.push_str(&text[last..m.start()]);
        last = m.end();
        if tag == "user_request" {
            rest.push_str(&c[2]);
        } else {
            out.push_str(&format!(
                "<details class=\"ctx\"><summary>{}</summary><pre>{}</pre></details>\n",
                esc(&tag.replace('_', " ")),
                esc(&clip(&c[2], MAX_TOOL_CHARS))
            ));
        }
    }
    rest.push_str(&text[last..]);
    format!("{}{}", markdown(rest.trim()), out)
}

/// The value as indented JSON, or as plain text when it is a string.
fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// A tool result: JSON results are indented.
fn result_text(s: &str) -> String {
    serde_json::from_str::<Value>(s).map(|v| pretty(&v)).unwrap_or_else(|_| s.to_owned())
}

/// The few words that say what a call did: its path, command, pattern, or query.
fn call_hint(input: &Value) -> String {
    let v = ["path", "command", "pattern", "query", "objective", "name", "url", "id"].iter().find_map(|k| input.get(*k).filter(|v| !v.is_null()));
    let s = v.map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).unwrap_or_default().replace('\n', " ");
    if s.chars().count() > 90 { format!("{}…", s.chars().take(89).collect::<String>()) } else { s }
}

fn diff_html(diff: &str) -> String {
    let mut out = String::from("<pre class=\"diff\">");
    for l in clip(diff, MAX_DIFF_CHARS).lines() {
        let class = if l.starts_with("+++") || l.starts_with("---") {
            "h"
        } else if l.starts_with('+') {
            "add"
        } else if l.starts_with('-') {
            "del"
        } else if l.starts_with("@@") {
            "hunk"
        } else {
            ""
        };
        if class.is_empty() {
            out.push_str(&format!("{}\n", esc(l)));
        } else {
            out.push_str(&format!("<span class=\"{class}\">{}</span>\n", esc(l)));
        }
    }
    out.push_str("</pre>");
    out
}

const STYLE: &str = r#"
:root { --bg: #ffffff; --fg: #1f2328; --muted: #656d76; --line: #d0d7de; --soft: #f6f8fa; --accent: #6f42c1;
  --user: #ddf4ff; --add: #e6ffec; --del: #ffebe9; --addfg: #1a7f37; --delfg: #cf222e; --err: #cf222e; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #0d1117; --fg: #e6edf3; --muted: #8d96a0; --line: #30363d; --soft: #161b22; --accent: #a371f7;
    --user: #0c2d48; --add: #12261e; --del: #2d1517; --addfg: #3fb950; --delfg: #f85149; --err: #f85149; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; }
main { max-width: 920px; margin: 0 auto; padding: 32px 20px 64px; }
header h1 { font-size: 24px; margin: 0 0 6px; }
.meta { color: var(--muted); font-size: 13px; display: flex; flex-wrap: wrap; gap: 6px 16px; }
.badge { display: inline-block; padding: 1px 8px; border-radius: 999px; border: 1px solid var(--line); font-size: 12px; }
h2 { font-size: 17px; margin: 36px 0 12px; padding-bottom: 6px; border-bottom: 1px solid var(--line); }
.msg { margin: 14px 0; padding: 12px 16px; border-radius: 10px; border: 1px solid var(--line); }
.msg.user { background: var(--user); }
.who { font-size: 12px; font-weight: 600; color: var(--muted); text-transform: uppercase; letter-spacing: .04em; margin-bottom: 4px; }
.msg p { margin: 6px 0; }
pre { background: var(--soft); border: 1px solid var(--line); border-radius: 8px; padding: 10px 12px; overflow-x: auto; font: 12.5px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; white-space: pre-wrap; word-break: break-word; }
code { font: 0.92em ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: var(--soft); padding: 1px 4px; border-radius: 4px; }
pre code { background: none; padding: 0; }
table { border-collapse: collapse; margin: 8px 0; font-size: 14px; }
th, td { border: 1px solid var(--line); padding: 4px 10px; text-align: left; }
blockquote { margin: 6px 0; padding-left: 12px; border-left: 3px solid var(--line); color: var(--muted); }
details { margin: 6px 0; border: 1px solid var(--line); border-radius: 8px; background: var(--bg); }
details > summary { cursor: pointer; padding: 6px 10px; font-size: 13px; }
details > pre, details > div { margin: 0 10px 10px; }
details.tool > summary b { color: var(--accent); }
details.tool.failed > summary b { color: var(--err); }
details.ctx > summary { color: var(--muted); }
.label { font-size: 12px; color: var(--muted); margin: 8px 10px 2px; }
pre.diff .add { background: var(--add); color: var(--addfg); display: block; }
pre.diff .del { background: var(--del); color: var(--delfg); display: block; }
pre.diff .hunk { color: var(--accent); }
pre.diff .h { color: var(--muted); }
ol.timeline { list-style: none; padding: 0; margin: 0; border-left: 2px solid var(--line); }
ol.timeline li { position: relative; padding: 4px 0 12px 18px; }
ol.timeline li::before { content: ""; position: absolute; left: -6px; top: 10px; width: 10px; height: 10px; border-radius: 50%; background: var(--accent); }
.files { color: var(--muted); font-size: 13px; }
img { max-width: 100%; border-radius: 6px; border: 1px solid var(--line); }
footer { margin-top: 40px; color: var(--muted); font-size: 12px; }
"#;

/// The task as an HTML page. `ops` are the edit-log operations of the task; `timeline` its snapshots.
pub fn render(rec: &TaskRecord, ops: &[Operation], timeline: &[Snapshot]) -> String {
    let results: HashMap<&str, (&str, bool)> = rec
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::ToolResult { tool_use_id, content, is_error, .. } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();
    let mut body = String::new();
    let status = serde_json::to_value(rec.status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    body.push_str(&format!(
        "<header><h1>{}</h1><div class=\"meta\"><span class=\"badge\">{}</span><span>{}</span><span>Model: {}</span><span>Task {}</span></div>\
         <div class=\"meta\"><span>{} input / {} output tokens</span><span>≈ ${:.4}</span><span>{} model turns · {} tool calls</span>{}</div></header>\n",
        esc(&rec.title),
        esc(&status),
        esc(&date(rec.created_ms)),
        esc(&rec.model),
        esc(&rec.id),
        rec.usage.input_tokens.unwrap_or(0),
        rec.usage.output_tokens.unwrap_or(0),
        rec.metrics.cost_usd,
        rec.iterations,
        rec.tool_calls,
        rec.error.as_deref().map(|e| format!("<span style=\"color:var(--err)\">Error: {}</span>", esc(e))).unwrap_or_default(),
    ));

    body.push_str("<h2>Conversation</h2>\n");
    for m in &rec.messages {
        match m.role {
            Role::User => {
                let mut html = String::new();
                for p in &m.content {
                    match p {
                        ContentPart::Text { text } => html.push_str(&user_text(text)),
                        ContentPart::Image { media_type, data } => {
                            html.push_str(&format!("<p><img alt=\"Attached image\" src=\"data:{};base64,{}\"></p>\n", esc(media_type), esc(data)))
                        }
                        _ => {}
                    }
                }
                if !html.trim().is_empty() {
                    body.push_str(&format!("<section class=\"msg user\"><div class=\"who\">You</div>{html}</section>\n"));
                }
            }
            Role::Assistant => {
                let mut html = String::new();
                for p in &m.content {
                    match p {
                        ContentPart::Text { text } if text != "(no response)" => html.push_str(&markdown(text)),
                        ContentPart::ToolUse { id, name, input } => {
                            let (result, failed) = results.get(id.as_str()).copied().unwrap_or(("", false));
                            html.push_str(&format!(
                                "<details class=\"tool{}\"><summary><b>{}</b> {}{}</summary><div class=\"label\">Input</div><pre>{}</pre><div class=\"label\">Result</div><pre>{}</pre></details>\n",
                                if failed { " failed" } else { "" },
                                esc(name),
                                esc(&call_hint(input)),
                                if failed { " — failed" } else { "" },
                                esc(&clip(&pretty(input), MAX_TOOL_CHARS)),
                                esc(&clip(&result_text(result), MAX_TOOL_CHARS)),
                            ));
                        }
                        _ => {}
                    }
                }
                if !html.trim().is_empty() {
                    body.push_str(&format!("<section class=\"msg\"><div class=\"who\">Agent</div>{html}</section>\n"));
                }
            }
        }
    }

    let changes: Vec<_> = ops.iter().filter(|o| !o.undone).flat_map(|o| &o.changes).collect();
    if !changes.is_empty() {
        body.push_str(&format!("<h2>Changed files ({})</h2>\n", changes.len()));
        for c in changes {
            let kind = match (&c.before, &c.after) {
                (None, Some(_)) => "added",
                (Some(_), None) => "deleted",
                _ => "modified",
            };
            body.push_str(&format!("<details><summary><code>{}</code> · {kind}</summary>{}</details>\n", esc(&c.path), diff_html(&c.diff)));
        }
    }

    if !timeline.is_empty() {
        body.push_str("<h2>Timeline</h2>\n<ol class=\"timeline\">\n");
        for s in timeline {
            let files = if s.base {
                "The workspace before the task".to_owned()
            } else {
                s.files.iter().map(|f| format!("{} ({})", esc(&f.path), f.status)).collect::<Vec<_>>().join(", ")
            };
            body.push_str(&format!(
                "<li><b>Step {}</b> · {} · <span class=\"files\">{}</span><div class=\"files\">{files}</div><div class=\"files\"><code>{}</code></div></li>\n",
                s.step,
                esc(&s.label),
                esc(&date(s.time_ms)),
                esc(&s.id[..s.id.len().min(12)]),
            ));
        }
        body.push_str("</ol>\n");
    }

    format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>\n{body}<footer>Exported by task-x on {}.</footer></main></body></html>\n",
        esc(&rec.title),
        esc(&date(super::store::now_ms()))
    )
}

/// `<root>/.task-x/exports/2026-11-12-fix-the-login-bug-<id>.html`.
pub fn default_path(root: &Path, rec: &TaskRecord) -> PathBuf {
    let slug: String = rec
        .title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let day = date(rec.created_ms).chars().take(10).collect::<String>();
    let slug = if slug.is_empty() { "task".to_owned() } else { slug };
    root.join(EXPORT_DIR).join(format!("{day}-{slug}-{}.html", rec.id))
}

/// Renders `rec` (with its edits from `log` and, in a workspace, its timeline) and writes it to
/// `path`, or to `default_path` in the workspace. Returns the path and the size in bytes.
pub fn export_task(rec: &TaskRecord, log: &crate::tools::EditLog, root: Option<&Path>, path: Option<&str>) -> Result<(PathBuf, usize), String> {
    let path = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => root.ok_or("give an absolute path: no workspace is open")?.join(p),
        None => default_path(root.ok_or("give a path: no workspace is open")?, rec),
    };
    let ops: Vec<Operation> = rec.operations.iter().filter_map(|i| log.operations.get(*i).cloned()).collect();
    // Only an existing timeline is read: exporting never creates one.
    let snapshots = root
        .filter(|r| r.join(super::timeline::DIR).join("HEAD").is_file())
        .and_then(|r| super::timeline::Timeline::open(r).ok()?.list(&rec.id).ok())
        .unwrap_or_default();
    let html = render(rec, &ops, &snapshots);
    write(&path, &html)?;
    Ok((path, html.len()))
}

/// Writes the page atomically, creating its folder.
pub fn write(path: &Path, html: &str) -> Result<(), String> {
    if path.extension().and_then(|e| e.to_str()).is_none_or(|e| !e.eq_ignore_ascii_case("html") && !e.eq_ignore_ascii_case("htm")) {
        return Err("the export path must end in .html".into());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("html.tmp");
    std::fs::write(&tmp, html).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::store::Status;
    use crate::agent::store::tests::record;
    use crate::provider::Message;
    use crate::tools::FileChange;
    use serde_json::json;

    fn task() -> TaskRecord {
        let mut r = record("abc123", Status::Completed);
        r.title = "Fix <the> login & bug".into();
        r.created_ms = 1_762_958_580_000;
        r.messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "<repository_context>\nsrc/login.rs: fn login()\n</repository_context>\n<user_request>\nFix the **login** bug in `login.rs`\n</user_request>".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text { text: "Looking at it.".into() },
                    ContentPart::ToolUse { id: "c1".into(), name: "replace_text".into(), input: json!({ "path": "src/login.rs", "oldText": "a < b", "newText": "a <= b" }) },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::ToolResult { tool_use_id: "c1".into(), tool_name: None, content: "{\"changed\":[\"src/login.rs\"]}".into(), is_error: false }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::Text { text: "## Done\n\n- fixed the check\n- added a test\n\n```rust\nif a <= b {}\n```\n\n| File | Change |\n|---|---|\n| `login.rs` | fixed |".into() }],
            },
        ];
        r
    }

    #[test]
    fn exports_a_readable_self_contained_page() {
        let ops = vec![Operation {
            id: 0,
            tool: "replace_text".into(),
            task_id: Some("abc123".into()),
            changes: vec![FileChange {
                path: "src/login.rs".into(),
                before: Some("a < b\n".into()),
                after: Some("a <= b\n".into()),
                diff: "--- a/src/login.rs\n+++ b/src/login.rs\n@@ -1 +1 @@\n-a < b\n+a <= b\n".into(),
                settled: false,
            }],
            undone: false,
            expired: false,
        }];
        let snap = Snapshot { id: "0123456789abcdef".into(), task_id: "abc123".into(), step: 0, label: "Before: fix".into(), time_ms: 0, base: true, files: vec![] };
        let html = render(&task(), &ops, &[snap]);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>Fix &lt;the&gt; login &amp; bug</title>"), "the title is escaped");
        assert!(!html.contains("<script"), "no scripts");
        assert!(html.contains("default-src 'none'"), "no network requests");
        assert!(html.contains("<p>Fix the <strong>login</strong> bug in <code>login.rs</code></p>"), "the request is the message");
        assert!(html.contains("<summary>repository context</summary>"), "context is folded away");
        assert!(html.contains("<b>replace_text</b> src/login.rs"));
        assert!(html.contains("&quot;newText&quot;: &quot;a &lt;= b&quot;"), "tool input is escaped JSON");
        assert!(html.contains("&quot;changed&quot;"), "the result is attached to its call");
        assert!(html.contains("<h4>Done</h4>") && html.contains("<li>fixed the check</li>"));
        assert!(html.contains("<pre data-lang=\"rust\"><code>if a &lt;= b {}</code></pre>"));
        assert!(html.contains("<th>File</th>") && html.contains("<td><code>login.rs</code></td>"));
        assert!(html.contains("<span class=\"add\">+a &lt;= b</span>"));
        assert!(html.contains("Changed files (1)") && html.contains("<h2>Timeline</h2>") && html.contains("The workspace before the task"));
        assert_eq!(html.matches("class=\"msg user\"").count(), 1, "a message of tool results is not shown as the user");
    }

    #[test]
    fn markdown_never_lets_html_through() {
        let html = markdown("<img src=x onerror=alert(1)> **<b>x</b>** [a](javascript:alert(1)) [ok](https://example.com)");
        assert!(!html.contains("<img") && !html.contains("<b>x") && !html.contains("href=\"javascript"), "{html}");
        assert!(html.contains("<a href=\"https://example.com\" rel=\"noopener noreferrer\">ok</a>"), "{html}");
    }

    #[test]
    fn dates_paths_and_the_html_extension() {
        assert_eq!(date(0), "1970-01-01 00:00 UTC");
        assert_eq!(date(1_762_958_580_000), "2025-11-12 14:43 UTC");
        let p = default_path(Path::new("/w"), &task());
        assert_eq!(p, Path::new("/w/.task-x/exports/2025-11-12-fix-the-login-bug-abc123.html"));
        assert!(write(Path::new("/tmp/x.txt"), "").unwrap_err().contains(".html"));
    }
}
