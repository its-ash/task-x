//! Exports a task as one self-contained HTML file: the conversation (the user's prompts, the
//! answers, and every tool call with its input and result), the files the agent changed with
//! their diffs, and the task's timeline. No scripts, fonts, or network requests: the file opens
//! anywhere, long parts fold away with `<details>`, and a CSS-only switch picks the theme.
//!
//! An export can cover a range of **turns**: a turn is one user prompt and everything up to the
//! next one. The task's checkpoints mark where each turn starts in the messages, the edit list,
//! and time, so a range takes its messages, its edits, and its timeline snapshots.

use super::store::TaskRecord;
use super::timeline::Snapshot;
use crate::provider::{ContentPart, Message, Role};
use crate::tools::Operation;
use regex::Regex;
use serde::Serialize;
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
    let e = inline_tags(&e);
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

/// Inline HTML tags an answer may use, such as `<kbd>Cmd</kbd>` for a key. Only these, only
/// without attributes, and only in closed pairs (`<br>` stands alone); every other tag stays text.
const INLINE_TAGS: [&str; 9] = ["kbd", "sub", "sup", "mark", "ins", "del", "s", "u", "small"];

/// Turns the allowed tags back into HTML in escaped text.
fn inline_tags(escaped: &str) -> String {
    static PAIRS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
        INLINE_TAGS.iter().map(|t| (*t, Regex::new(&format!(r"(?i)&lt;{t}&gt;(.*?)&lt;/{t}&gt;")).expect("tag regex"))).collect()
    });
    static BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&lt;br\s*/?&gt;").expect("br regex"));
    let mut s = BR.replace_all(escaped, "<br>").into_owned();
    for (tag, re) in PAIRS.iter() {
        s = re.replace_all(&s, format!("<{tag}>$1</{tag}>").as_str()).into_owned();
    }
    s
}

/// The cells of a Markdown table row.
fn table_cells(l: &str) -> Vec<&str> {
    l.trim().trim_start_matches('|').trim_end_matches('|').split('|').map(str::trim).collect()
}

/// `|---|:---:|---:|`: a table's separator row.
fn is_table_rule(l: &str) -> bool {
    let t = l.trim();
    t.starts_with('|') && t.contains('-') && table_cells(t).iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// The cell alignment a separator cell asks for, as a class attribute.
fn align(rule: &str) -> &'static str {
    match (rule.starts_with(':'), rule.ends_with(':')) {
        (true, true) => " class=\"c\"",
        (false, true) => " class=\"r\"",
        _ => "",
    }
}

/// A small Markdown renderer for answers: headings, fenced code, lists, tables, quotes, rules, paragraphs.
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
        } else if matches!(t.trim_end(), "---" | "***" | "___") {
            flush(&mut para, &mut out);
            out.push_str("<hr>\n");
        } else if t.starts_with('|') && lines.get(i + 1).is_some_and(|n| is_table_rule(n)) {
            flush(&mut para, &mut out);
            let aligns: Vec<&str> = table_cells(lines[i + 1]).into_iter().map(align).collect();
            let class = |j: usize| aligns.get(j).copied().unwrap_or("");
            out.push_str("<div class=\"table-wrap\"><table><thead><tr>");
            for (j, c) in table_cells(t).into_iter().enumerate() {
                out.push_str(&format!("<th{}>{}</th>", class(j), inline(c)));
            }
            out.push_str("</tr></thead><tbody>\n");
            i += 2;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                out.push_str("<tr>");
                for (j, c) in table_cells(lines[i]).into_iter().enumerate() {
                    out.push_str(&format!("<td{}>{}</td>", class(j), inline(c)));
                }
                out.push_str("</tr>\n");
                i += 1;
            }
            out.push_str("</tbody></table></div>\n");
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

/// Lines added and removed by a unified diff.
fn diff_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(a, d), l| {
        if l.starts_with("+++") || l.starts_with("---") {
            (a, d)
        } else if l.starts_with('+') {
            (a + 1, d)
        } else if l.starts_with('-') {
            (a, d + 1)
        } else {
            (a, d)
        }
    })
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

/// One user prompt and what followed it, up to the next prompt. `n` counts from 1.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub n: usize,
    pub prompt: String,
    pub created_ms: u64,
}

/// The turns to export, counted from 1 and inclusive. `None`: the first, or the last.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Range {
    pub from: Option<usize>,
    pub to: Option<usize>,
}

/// What a range covers in the task record.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub from: usize,
    pub to: usize,
    pub total: usize,
    /// Indexes into `rec.messages`.
    pub messages: std::ops::Range<usize>,
    /// Indexes into `rec.operations`.
    pub operations: std::ops::Range<usize>,
    /// Timeline snapshots taken in `[since_ms, until_ms)` belong to the range.
    pub since_ms: u64,
    pub until_ms: u64,
}

impl Span {
    pub fn is_partial(&self) -> bool {
        self.from > 1 || self.to < self.total
    }
}

/// Where each turn starts: (message index, operation index, time). Always at least one turn;
/// the first starts at the beginning, and the marks never go backwards (a compacted
/// conversation can leave checkpoints past its end).
fn marks(rec: &TaskRecord) -> Vec<(usize, usize, u64)> {
    let mut m: Vec<(usize, usize, u64)> = rec.checkpoints.iter().map(|c| (c.messages.min(rec.messages.len()), c.operations.min(rec.operations.len()), c.created_ms)).collect();
    if m.is_empty() {
        m.push((0, 0, 0));
    }
    m[0] = (0, 0, 0);
    for i in 1..m.len() {
        m[i] = (m[i].0.max(m[i - 1].0), m[i].1.max(m[i - 1].1), m[i].2.max(m[i - 1].2));
    }
    m
}

/// The task's turns, for the user to pick a range from.
pub fn turns(rec: &TaskRecord) -> Vec<Turn> {
    if rec.checkpoints.is_empty() {
        return vec![Turn { n: 1, prompt: rec.title.clone(), created_ms: rec.created_ms }];
    }
    rec.checkpoints
        .iter()
        .enumerate()
        .map(|(i, c)| Turn { n: i + 1, prompt: c.prompt.lines().next().unwrap_or("").to_owned(), created_ms: c.created_ms })
        .collect()
}

/// The part of the task a range covers, or why the range is wrong.
pub fn span(rec: &TaskRecord, r: Range) -> Result<Span, String> {
    let m = marks(rec);
    let total = m.len();
    let (from, to) = (r.from.unwrap_or(1), r.to.unwrap_or(total));
    if from < 1 || to > total || from > to {
        return Err(if total == 1 { format!("this task has one turn; got {from} to {to}") } else { format!("turns run from 1 to {total}; got {from} to {to}") });
    }
    let end = m.get(to);
    Ok(Span {
        from,
        to,
        total,
        messages: m[from - 1].0..end.map_or(rec.messages.len(), |e| e.0),
        operations: m[from - 1].1..end.map_or(rec.operations.len(), |e| e.1),
        since_ms: m[from - 1].2,
        until_ms: end.map_or(u64::MAX, |e| e.2),
    })
}

/// A message as HTML: `(is the user, html)`, or `None` when nothing in it is shown (a message
/// of tool results belongs to the calls above it).
fn message_html(m: &Message, results: &HashMap<&str, (&str, bool)>) -> Option<(bool, String)> {
    let mut html = String::new();
    match m.role {
        Role::User => {
            for p in &m.content {
                match p {
                    ContentPart::Text { text } => html.push_str(&user_text(text)),
                    ContentPart::Image { media_type, data } => {
                        html.push_str(&format!("<p><img alt=\"Attached image\" src=\"data:{};base64,{}\"></p>\n", esc(media_type), esc(data)))
                    }
                    _ => {}
                }
            }
        }
        Role::Assistant => {
            for p in &m.content {
                match p {
                    ContentPart::Text { text } if text != "(no response)" => html.push_str(&markdown(text)),
                    ContentPart::ToolUse { id, name, input } => {
                        let (result, failed) = results.get(id.as_str()).copied().unwrap_or(("", false));
                        let hint = call_hint(input);
                        html.push_str(&format!(
                            "<details class=\"tool{}\"><summary><span class=\"dot\"></span><span class=\"tool-name\">{}</span>{}{}</summary>\
                             <div class=\"label\">Input</div><pre>{}</pre><div class=\"label\">Result</div><pre>{}</pre></details>\n",
                            if failed { " failed" } else { "" },
                            esc(name),
                            if hint.is_empty() { String::new() } else { format!("<span class=\"hint\">{}</span>", esc(&hint)) },
                            if failed { "<span class=\"tag err\">failed</span>" } else { "" },
                            esc(&clip(&pretty(input), MAX_TOOL_CHARS)),
                            esc(&clip(&result_text(result), MAX_TOOL_CHARS)),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    (!html.trim().is_empty()).then_some((m.role == Role::User, html))
}

fn status_class(status: &str) -> &'static str {
    match status {
        "completed" => "ok",
        "failed" | "providerError" | "stuck" | "budgetExceeded" | "deadlineExceeded" | "maxIterations" => "err",
        "running" | "sleeping" => "info",
        _ => "muted",
    }
}

/// `maxIterations` → `max iterations`.
fn words(camel: &str) -> String {
    camel.chars().fold(String::new(), |mut s, c| {
        if c.is_ascii_uppercase() {
            s.push(' ');
        }
        s.push(c.to_ascii_lowercase());
        s
    })
}

const LIGHT: &str = "--bg: #f7f8fa; --card: #ffffff; --fg: #1d2330; --muted: #6a7282; --line: #e2e5ea; --line-strong: #cfd4dc; --soft: #f1f3f6; \
  --accent: #5b5bd6; --accent-soft: #ececfd; --user: #eef4ff; --user-line: #cfdcfb; --add: #e7f8ec; --del: #fdeceb; --addfg: #1a7f37; --delfg: #c9302c; \
  --ok: #1a7f37; --ok-soft: #e3f5e8; --err: #c9302c; --err-soft: #fbe7e6; --info: #0969da; --info-soft: #e2efff; --stripe: #fafbfc; --hover: #f3f4ff; \
  --shadow: 0 1px 2px rgba(16,24,40,.05), 0 1px 3px rgba(16,24,40,.06);";
const DARK: &str = "--bg: #0e1116; --card: #161a22; --fg: #e5e9f0; --muted: #8b93a3; --line: #262c37; --line-strong: #353d4b; --soft: #1c212b; \
  --accent: #8e8cf7; --accent-soft: #24244a; --user: #142338; --user-line: #23395a; --add: #11261a; --del: #2d1617; --addfg: #4fc66b; --delfg: #f47067; \
  --ok: #4fc66b; --ok-soft: #13291b; --err: #f47067; --err-soft: #321a1a; --info: #58a6ff; --info-soft: #132a45; --stripe: #191e27; --hover: #20223a; \
  --shadow: 0 1px 2px rgba(0,0,0,.4);";

const STYLE: &str = r#"
* { box-sizing: border-box; }
html { scroll-behavior: smooth; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, Helvetica, Arial, sans-serif; -webkit-font-smoothing: antialiased; }
.sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }
main { max-width: 960px; margin: 0 auto; padding: 36px 22px 72px; }
a { color: var(--accent); text-decoration: none; } a:hover { text-decoration: underline; }

/* Header */
.top { display: flex; align-items: flex-start; justify-content: space-between; gap: 16px; flex-wrap: wrap; }
.eyebrow { font-size: 12px; font-weight: 600; letter-spacing: .08em; text-transform: uppercase; color: var(--accent); margin-bottom: 6px; }
header h1 { font-size: 27px; line-height: 1.25; margin: 0 0 10px; letter-spacing: -.01em; }
.meta { color: var(--muted); font-size: 13px; display: flex; flex-wrap: wrap; align-items: center; gap: 6px 14px; }
.badge { display: inline-flex; align-items: center; gap: 6px; padding: 2px 10px; border-radius: 999px; font-size: 12px; font-weight: 600; text-transform: capitalize; border: 1px solid transparent; }
.badge::before { content: ""; width: 7px; height: 7px; border-radius: 50%; background: currentColor; }
.badge.ok { color: var(--ok); background: var(--ok-soft); } .badge.err { color: var(--err); background: var(--err-soft); }
.badge.info { color: var(--info); background: var(--info-soft); } .badge.muted { color: var(--muted); background: var(--soft); border-color: var(--line); }
.error-box { margin-top: 14px; padding: 10px 14px; border-radius: 10px; background: var(--err-soft); color: var(--err); border: 1px solid var(--err); font-size: 14px; }

/* Theme switch (CSS only) */
.theme { display: inline-flex; padding: 3px; border-radius: 10px; background: var(--soft); border: 1px solid var(--line); }
.theme label { cursor: pointer; padding: 3px 11px; border-radius: 7px; font-size: 12px; color: var(--muted); user-select: none; }
#theme-auto:checked ~ main label[for=theme-auto], #theme-light:checked ~ main label[for=theme-light], #theme-dark:checked ~ main label[for=theme-dark] { background: var(--card); color: var(--fg); box-shadow: var(--shadow); }

/* Stat cards */
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 12px; margin: 24px 0 8px; }
.stat { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 12px 14px; box-shadow: var(--shadow); min-width: 0; }
.stat .k { font-size: 11.5px; font-weight: 600; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); }
.stat .v { font-size: 17px; font-weight: 600; margin-top: 2px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.stat .s { font-size: 12px; color: var(--muted); }

/* Sections */
h2 { display: flex; align-items: center; gap: 10px; font-size: 18px; margin: 40px 0 14px; padding-bottom: 8px; border-bottom: 1px solid var(--line); }
.count { font-size: 12px; font-weight: 600; padding: 1px 9px; border-radius: 999px; background: var(--accent-soft); color: var(--accent); }
nav.toc { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 6px 4px; box-shadow: var(--shadow); }
nav.toc ol { margin: 0; padding: 0; list-style: none; counter-reset: none; }
nav.toc li a { display: flex; gap: 12px; align-items: baseline; padding: 6px 12px; border-radius: 8px; color: var(--fg); }
nav.toc li a:hover { background: var(--hover); text-decoration: none; }
nav.toc .n { flex: none; font: 600 12px ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--accent); min-width: 3.2em; }
nav.toc .p { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
nav.toc .d { flex: none; font-size: 12px; color: var(--muted); }

/* Turns and messages */
.turn { margin: 26px 0 0; scroll-margin-top: 16px; }
.turn-head { display: flex; align-items: center; gap: 10px; margin: 0 0 10px; color: var(--muted); font-size: 12.5px; }
.turn-head .pill { font-weight: 700; font-size: 11.5px; letter-spacing: .06em; text-transform: uppercase; color: var(--accent); background: var(--accent-soft); padding: 2px 10px; border-radius: 999px; }
.turn-head::after { content: ""; flex: 1; height: 1px; background: var(--line); }
.msg { position: relative; margin: 12px 0; padding: 14px 18px 14px 18px; border-radius: 14px; background: var(--card); border: 1px solid var(--line); box-shadow: var(--shadow); }
.msg.user { background: var(--user); border-color: var(--user-line); }
.who { display: flex; align-items: center; gap: 8px; font-size: 12px; font-weight: 700; color: var(--muted); text-transform: uppercase; letter-spacing: .06em; margin-bottom: 6px; }
.avatar { display: inline-grid; place-items: center; width: 22px; height: 22px; border-radius: 50%; font-size: 11px; color: #fff; background: var(--accent); letter-spacing: 0; }
.msg.user .avatar { background: var(--info); }
.msg p { margin: 8px 0; }
.msg h3, .msg h4, .msg h5, .msg h6 { margin: 16px 0 6px; line-height: 1.3; }
.msg ul, .msg ol { padding-left: 22px; margin: 8px 0; } .msg li { margin: 3px 0; }
hr { border: 0; border-top: 1px solid var(--line); margin: 16px 0; }

/* Code */
pre { position: relative; background: var(--soft); border: 1px solid var(--line); border-radius: 10px; padding: 12px 14px; overflow-x: auto; font: 12.5px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; white-space: pre-wrap; word-break: break-word; margin: 10px 0; }
pre[data-lang] { padding-top: 28px; }
pre[data-lang]::before { content: attr(data-lang); position: absolute; top: 0; right: 0; padding: 2px 10px; font-size: 11px; color: var(--muted); background: var(--card); border-left: 1px solid var(--line); border-bottom: 1px solid var(--line); border-bottom-left-radius: 8px; border-top-right-radius: 10px; }
code { font: .9em ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: var(--soft); border: 1px solid var(--line); padding: 1px 5px; border-radius: 5px; }
pre code { background: none; border: 0; padding: 0; }
kbd { display: inline-block; min-width: 1.6em; padding: 1px 6px; font: 600 .82em ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; text-align: center; color: var(--fg); background: var(--card); border: 1px solid var(--line-strong); border-bottom-width: 2px; border-radius: 6px; box-shadow: 0 1px 0 var(--line); vertical-align: .08em; white-space: nowrap; }
mark { background: #fff1a8; color: #1d2330; padding: 0 2px; border-radius: 3px; }
ins { color: var(--addfg); text-decoration-color: var(--addfg); } del { color: var(--delfg); }

/* Tables: rounded frame, header band, cell rules, zebra rows */
.table-wrap { overflow-x: auto; margin: 12px 0; border: 1px solid var(--line-strong); border-radius: 10px; box-shadow: var(--shadow); background: var(--card); }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 14px; }
th, td { padding: 8px 14px; text-align: left; vertical-align: top; border-bottom: 1px solid var(--line); border-right: 1px solid var(--line); }
th:last-child, td:last-child { border-right: 0; }
tbody tr:last-child td { border-bottom: 0; }
thead th { background: var(--soft); font-size: 12.5px; font-weight: 700; text-transform: uppercase; letter-spacing: .04em; color: var(--muted); border-bottom: 1px solid var(--line-strong); white-space: nowrap; }
tbody tr:nth-child(even) td { background: var(--stripe); }
tbody tr:hover td { background: var(--hover); }
th.c, td.c { text-align: center; } th.r, td.r, td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
td.num.add { color: var(--addfg); } td.num.del { color: var(--delfg); }

blockquote { margin: 10px 0; padding: 6px 14px; border-left: 3px solid var(--accent); background: var(--soft); border-radius: 0 8px 8px 0; color: var(--muted); }

/* Folded parts: tool calls, context, diffs */
details { margin: 8px 0; border: 1px solid var(--line); border-radius: 10px; background: var(--card); overflow: hidden; }
details > summary { list-style: none; cursor: pointer; display: flex; align-items: center; gap: 8px; padding: 7px 12px; font-size: 13px; }
details > summary::-webkit-details-marker { display: none; }
details > summary::after { content: "›"; margin-left: auto; color: var(--muted); font-size: 16px; transition: transform .15s; }
details[open] > summary::after { transform: rotate(90deg); }
details[open] > summary { border-bottom: 1px solid var(--line); background: var(--soft); }
details > summary:hover { background: var(--hover); }
details > pre, details > div { margin: 10px 12px; }
details.tool .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--ok); flex: none; }
details.tool.failed .dot { background: var(--err); }
.tool-name { font: 600 12.5px ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--accent); }
.hint { color: var(--muted); font: 12.5px ui-monospace, SFMono-Regular, Menlo, monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.tag { font-size: 11px; font-weight: 600; padding: 1px 8px; border-radius: 999px; }
.tag.err { color: var(--err); background: var(--err-soft); }
.tag.add { color: var(--addfg); background: var(--add); } .tag.del { color: var(--delfg); background: var(--del); } .tag.mod { color: var(--info); background: var(--info-soft); }
details.ctx > summary { color: var(--muted); text-transform: capitalize; }
.label { font-size: 11.5px; font-weight: 600; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); margin: 10px 12px 2px; }
pre.diff { padding: 8px 0; }
pre.diff .add { background: var(--add); color: var(--addfg); display: block; padding: 0 14px; }
pre.diff .del { background: var(--del); color: var(--delfg); display: block; padding: 0 14px; }
pre.diff .hunk { color: var(--accent); display: block; padding: 0 14px; }
pre.diff .h { color: var(--muted); display: block; padding: 0 14px; }

/* Timeline */
ol.timeline { list-style: none; padding: 0; margin: 0 0 0 6px; border-left: 2px solid var(--line); }
ol.timeline li { position: relative; padding: 2px 0 16px 20px; }
ol.timeline li::before { content: ""; position: absolute; left: -7px; top: 7px; width: 12px; height: 12px; border-radius: 50%; background: var(--card); border: 3px solid var(--accent); }
ol.timeline li.base::before { border-color: var(--muted); }
.files { color: var(--muted); font-size: 13px; }

img { max-width: 100%; border-radius: 8px; border: 1px solid var(--line); }
footer { margin-top: 48px; padding-top: 16px; border-top: 1px solid var(--line); color: var(--muted); font-size: 12px; display: flex; justify-content: space-between; flex-wrap: wrap; gap: 8px; }
@media (max-width: 640px) { main { padding: 22px 14px 48px; } header h1 { font-size: 22px; } .msg { padding: 12px 14px; } }
@media print { .theme { display: none; } body { background: #fff; } .msg, .stat, .table-wrap, nav.toc { box-shadow: none; } details { break-inside: avoid; } }
"#;

/// The theme variables: the system's choice unless the switch overrides it.
fn theme_css() -> String {
    format!(
        ":root {{ color-scheme: light; {LIGHT} }}\n\
         @media (prefers-color-scheme: dark) {{ :root:not(:has(#theme-light:checked)) {{ color-scheme: dark; {DARK} }} }}\n\
         :root:has(#theme-dark:checked) {{ color-scheme: dark; {DARK} }}\n"
    )
}

/// The task (the turns in `span`) as an HTML page. `ops` are the edit-log operations of those
/// turns; `timeline` their snapshots.
pub fn render(rec: &TaskRecord, ops: &[Operation], timeline: &[Snapshot], span: &Span) -> String {
    let results: HashMap<&str, (&str, bool)> = rec
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::ToolResult { tool_use_id, content, is_error, .. } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();
    let all_turns = turns(rec);
    let marks = marks(rec);
    let status = serde_json::to_value(rec.status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let range_text = if span.total == 1 {
        "1 turn".to_owned()
    } else if span.is_partial() && span.from == span.to {
        format!("Turn {} of {}", span.from, span.total)
    } else if span.is_partial() {
        format!("Turns {}–{} of {}", span.from, span.to, span.total)
    } else {
        format!("All {} turns", span.total)
    };
    let whole = if span.is_partial() { "whole task" } else { "" };

    let mut body = String::new();
    body.push_str(&format!(
        "<header><div class=\"top\"><div><div class=\"eyebrow\">task-x conversation</div><h1>{}</h1>\
         <div class=\"meta\"><span class=\"badge {}\">{}</span><span>{}</span><span>Task <code>{}</code></span></div></div>\
         <div class=\"theme\" role=\"radiogroup\" aria-label=\"Theme\"><label for=\"theme-auto\">Auto</label><label for=\"theme-light\">Light</label><label for=\"theme-dark\">Dark</label></div></div>{}</header>\n",
        esc(&rec.title),
        status_class(&status),
        esc(&words(&status)),
        esc(&date(rec.created_ms)),
        esc(&rec.id),
        rec.error.as_deref().map(|e| format!("<div class=\"error-box\"><b>Error:</b> {}</div>", esc(e))).unwrap_or_default(),
    ));
    let stat = |k: &str, v: String, s: &str| format!("<div class=\"stat\"><div class=\"k\">{}</div><div class=\"v\">{}</div><div class=\"s\">{}</div></div>", esc(k), v, esc(s));
    body.push_str("<section class=\"stats\">");
    body.push_str(&stat("Range", esc(&range_text), ""));
    body.push_str(&stat("Model", esc(&rec.model), &rec.provider_kind));
    body.push_str(&stat(
        "Tokens",
        format!("{} / {}", rec.usage.input_tokens.unwrap_or(0), rec.usage.output_tokens.unwrap_or(0)),
        if whole.is_empty() { "input / output" } else { "input / output · whole task" },
    ));
    body.push_str(&stat("Cost", format!("≈ ${:.4}", rec.metrics.cost_usd), whole));
    body.push_str(&stat("Activity", format!("{} tool calls", rec.tool_calls), &format!("{} model turns{}", rec.iterations, if whole.is_empty() { "" } else { " · whole task" })));
    body.push_str("</section>\n");

    if span.to > span.from {
        body.push_str("<h2 id=\"contents\">Contents</h2>\n<nav class=\"toc\"><ol>\n");
        for t in &all_turns[span.from - 1..span.to] {
            body.push_str(&format!(
                "<li><a href=\"#turn-{0}\"><span class=\"n\">#{0}</span><span class=\"p\">{1}</span><span class=\"d\">{2}</span></a></li>\n",
                t.n,
                esc(&t.prompt),
                if t.created_ms > 0 { esc(&date(t.created_ms)) } else { String::new() },
            ));
        }
        body.push_str("</ol></nav>\n");
    }

    body.push_str("<h2 id=\"conversation\">Conversation</h2>\n");
    for k in span.from..=span.to {
        let start = marks[k - 1].0;
        let end = marks.get(k).map_or(rec.messages.len(), |m| m.0);
        let when = all_turns.get(k - 1).map(|t| t.created_ms).filter(|ms| *ms > 0).map(|ms| format!("<span>{}</span>", esc(&date(ms)))).unwrap_or_default();
        body.push_str(&format!("<section class=\"turn\" id=\"turn-{k}\"><div class=\"turn-head\"><span class=\"pill\">Turn {k}</span>{when}</div>\n"));
        for m in &rec.messages[start..end] {
            if let Some((user, html)) = message_html(m, &results) {
                let (class, who, avatar) = if user { ("msg user", "You", "Y") } else { ("msg", "Agent", "A") };
                body.push_str(&format!("<section class=\"{class}\"><div class=\"who\"><span class=\"avatar\">{avatar}</span>{who}</div>{html}</section>\n"));
            }
        }
        body.push_str("</section>\n");
    }

    let changes: Vec<_> = ops.iter().filter(|o| !o.undone).flat_map(|o| &o.changes).collect();
    if !changes.is_empty() {
        body.push_str(&format!("<h2 id=\"files\">Changed files <span class=\"count\">{}</span></h2>\n", changes.len()));
        body.push_str("<div class=\"table-wrap\"><table><thead><tr><th>File</th><th>Change</th><th class=\"num\">Added</th><th class=\"num\">Removed</th></tr></thead><tbody>\n");
        for (i, c) in changes.iter().enumerate() {
            let (kind, tag) = match (&c.before, &c.after) {
                (None, Some(_)) => ("added", "add"),
                (Some(_), None) => ("deleted", "del"),
                _ => ("modified", "mod"),
            };
            let (a, d) = diff_counts(&c.diff);
            body.push_str(&format!(
                "<tr><td><a href=\"#file-{i}\"><code>{}</code></a></td><td><span class=\"tag {tag}\">{kind}</span></td><td class=\"num add\">+{a}</td><td class=\"num del\">−{d}</td></tr>\n",
                esc(&c.path)
            ));
        }
        body.push_str("</tbody></table></div>\n");
        for (i, c) in changes.iter().enumerate() {
            body.push_str(&format!("<details id=\"file-{i}\"><summary><code>{}</code></summary>{}</details>\n", esc(&c.path), diff_html(&c.diff)));
        }
    }

    if !timeline.is_empty() {
        body.push_str(&format!("<h2 id=\"timeline\">Timeline <span class=\"count\">{}</span></h2>\n<ol class=\"timeline\">\n", timeline.len()));
        for s in timeline {
            let files = if s.base {
                "The workspace before the task".to_owned()
            } else {
                s.files.iter().map(|f| format!("{} ({})", esc(&f.path), f.status)).collect::<Vec<_>>().join(", ")
            };
            body.push_str(&format!(
                "<li{}><b>Step {}</b> · {} · <span class=\"files\">{}</span><div class=\"files\">{files}</div><div class=\"files\"><code>{}</code></div></li>\n",
                if s.base { " class=\"base\"" } else { "" },
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
         <title>{}</title><style>{}{STYLE}</style></head><body>\
         <input class=\"sr\" type=\"radio\" name=\"theme\" id=\"theme-auto\" checked><input class=\"sr\" type=\"radio\" name=\"theme\" id=\"theme-light\"><input class=\"sr\" type=\"radio\" name=\"theme\" id=\"theme-dark\">\
         <main>\n{body}<footer><span>Exported by task-x on {}.</span><span>{}</span></footer></main></body></html>\n",
        esc(&rec.title),
        theme_css(),
        esc(&date(super::store::now_ms())),
        esc(&range_text),
    )
}

/// `<root>/.task-x/exports/2026-11-12-fix-the-login-bug-<id>.html`, with `-turns-2-4` for a range.
pub fn default_path(root: &Path, rec: &TaskRecord, span: Option<&Span>) -> PathBuf {
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
    let part = match span.filter(|s| s.is_partial()) {
        Some(s) if s.from == s.to => format!("-turn-{}", s.from),
        Some(s) => format!("-turns-{}-{}", s.from, s.to),
        None => String::new(),
    };
    root.join(EXPORT_DIR).join(format!("{day}-{slug}-{}{part}.html", rec.id))
}

/// Renders the turns of `rec` in `range` (with their edits from `log` and, in a workspace, their
/// timeline snapshots) and writes them to `path`, or to `default_path` in the workspace.
/// Returns the path, the size in bytes, and the span exported.
pub fn export_task(rec: &TaskRecord, log: &crate::tools::EditLog, root: Option<&Path>, path: Option<&str>, range: Range) -> Result<(PathBuf, usize, Span), String> {
    let span = span(rec, range)?;
    let path = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => root.ok_or("give an absolute path: no workspace is open")?.join(p),
        None => default_path(root.ok_or("give a path: no workspace is open")?, rec, Some(&span)),
    };
    let ops: Vec<Operation> = rec.operations[span.operations.clone()].iter().filter_map(|i| log.operations.get(*i).cloned()).collect();
    // Only an existing timeline is read: exporting never creates one.
    let snapshots: Vec<Snapshot> = root
        .filter(|r| r.join(super::timeline::DIR).join("HEAD").is_file())
        .and_then(|r| super::timeline::Timeline::open(r).ok()?.list(&rec.id).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !span.is_partial() || (s.time_ms >= span.since_ms && s.time_ms < span.until_ms))
        .collect();
    let html = render(rec, &ops, &snapshots, &span);
    write(&path, &html)?;
    Ok((path, html.len(), span))
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
    use crate::agent::store::tests::record;
    use crate::agent::store::{Checkpoint, Status};
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
                content: vec![ContentPart::Text { text: "## Done\n\n- fixed the check\n- added a test\n\n```rust\nif a <= b {}\n```\n\n| File | Change | Lines |\n|---|:---:|---:|\n| `login.rs` | fixed | 2 |".into() }],
            },
        ];
        r
    }

    /// `task()` plus two more prompts, with checkpoints for all three.
    fn three_turns() -> TaskRecord {
        let mut r = task();
        for (i, text) in ["Now add a test", "Then update the docs"].iter().enumerate() {
            r.messages.push(Message { role: Role::User, content: vec![ContentPart::Text { text: format!("<user_request>\n{text}\n</user_request>") }] });
            r.messages.push(Message { role: Role::Assistant, content: vec![ContentPart::Text { text: format!("Answer {}", i + 2) }] });
        }
        r.operations = vec![0, 1];
        r.checkpoints = vec![
            Checkpoint { messages: 0, operations: 0, prompt: "Fix the login bug".into(), created_ms: 1_000 },
            Checkpoint { messages: 4, operations: 1, prompt: "Now add a test".into(), created_ms: 2_000 },
            Checkpoint { messages: 6, operations: 2, prompt: "Then update the docs".into(), created_ms: 3_000 },
        ];
        r
    }

    fn whole(rec: &TaskRecord) -> Span {
        span(rec, Range::default()).unwrap()
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
        let rec = task();
        let html = render(&rec, &ops, &[snap], &whole(&rec));
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>Fix &lt;the&gt; login &amp; bug</title>"), "the title is escaped");
        assert!(!html.contains("<script"), "no scripts");
        assert!(html.contains("default-src 'none'"), "no network requests");
        assert!(html.contains("id=\"theme-dark\"") && html.contains(":root:has(#theme-dark:checked)"), "a CSS-only theme switch");
        assert!(html.contains("<p>Fix the <strong>login</strong> bug in <code>login.rs</code></p>"), "the request is the message");
        assert!(html.contains("<summary>repository context</summary>"), "context is folded away");
        assert!(html.contains("<span class=\"tool-name\">replace_text</span><span class=\"hint\">src/login.rs</span>"));
        assert!(html.contains("&quot;newText&quot;: &quot;a &lt;= b&quot;"), "tool input is escaped JSON");
        assert!(html.contains("&quot;changed&quot;"), "the result is attached to its call");
        assert!(html.contains("<h4>Done</h4>") && html.contains("<li>fixed the check</li>"));
        assert!(html.contains("<pre data-lang=\"rust\"><code>if a &lt;= b {}</code></pre>"));
        assert!(html.contains("<div class=\"table-wrap\"><table><thead><tr><th>File</th><th class=\"c\">Change</th><th class=\"r\">Lines</th>"), "tables are framed and aligned");
        assert!(html.contains("<td><code>login.rs</code></td><td class=\"c\">fixed</td><td class=\"r\">2</td>"));
        assert!(html.contains("<span class=\"add\">+a &lt;= b</span>"));
        assert!(html.contains("<td class=\"num add\">+1</td><td class=\"num del\">−1</td>"), "the changed-files table counts lines");
        assert!(html.contains("Changed files <span class=\"count\">1</span>") && html.contains("<h2 id=\"timeline\">") && html.contains("The workspace before the task"));
        assert_eq!(html.matches("class=\"msg user\"").count(), 1, "a message of tool results is not shown as the user");
        assert!(html.contains("<span class=\"pill\">Turn 1</span>") && !html.contains("id=\"contents\""), "one turn needs no contents list");
    }

    #[test]
    fn turns_and_ranges_follow_the_checkpoints() {
        let rec = three_turns();
        let t = turns(&rec);
        assert_eq!(t.iter().map(|t| t.prompt.as_str()).collect::<Vec<_>>(), ["Fix the login bug", "Now add a test", "Then update the docs"]);
        let s = span(&rec, Range { from: Some(2), to: Some(2) }).unwrap();
        assert_eq!((s.messages.clone(), s.operations.clone(), s.since_ms, s.until_ms), (4..6, 1..2, 2_000, 3_000));
        let s = span(&rec, Range { from: Some(2), to: None }).unwrap();
        assert_eq!((s.messages.clone(), s.until_ms, s.is_partial()), (4..8, u64::MAX, true));
        assert!(!whole(&rec).is_partial());
        assert!(span(&rec, Range { from: Some(3), to: Some(2) }).unwrap_err().contains("1 to 3"));
        assert!(span(&rec, Range { from: Some(0), to: None }).is_err());
        assert!(span(&rec, Range { from: None, to: Some(4) }).is_err());

        let html = render(&rec, &[], &[], &span(&rec, Range { from: Some(2), to: Some(3) }).unwrap());
        assert!(!html.contains("login.rs") && html.contains("Now add a test") && html.contains("Answer 3"), "only the chosen turns");
        assert!(html.contains("id=\"turn-2\"") && html.contains("id=\"turn-3\"") && !html.contains("id=\"turn-1\""));
        assert!(html.contains("Turns 2–3 of 3") && html.contains("href=\"#turn-3\""));

        // A record from before checkpoints, or with stale ones, is still one whole page.
        let mut old = task();
        old.checkpoints.clear();
        assert_eq!(turns(&old).len(), 1);
        assert_eq!(whole(&old).messages, 0..4);
        let mut stale = three_turns();
        stale.messages.truncate(3);
        assert_eq!(span(&stale, Range { from: Some(3), to: None }).unwrap().messages, 3..3);
    }

    #[test]
    fn markdown_never_lets_html_through() {
        let html = markdown("<img src=x onerror=alert(1)> **<b>x</b>** [a](javascript:alert(1)) [ok](https://example.com)");
        assert!(!html.contains("<img") && !html.contains("<b>x") && !html.contains("href=\"javascript"), "{html}");
        assert!(html.contains("<a href=\"https://example.com\" rel=\"noopener noreferrer\">ok</a>"), "{html}");
    }

    #[test]
    fn keys_and_a_few_inline_tags_are_html() {
        let html = markdown("Press <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd>, H<sub>2</sub>O<br>`<kbd>x</kbd>`");
        assert!(html.contains("Press <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd>, H<sub>2</sub>O<br>"), "{html}");
        assert!(html.contains("<code>&lt;kbd&gt;x&lt;/kbd&gt;</code>"), "code stays literal: {html}");
        let html = markdown(r#"<kbd onclick="x">a</kbd> <mark>open <kbd><img src=x></kbd>"#);
        assert!(!html.contains("<kbd onclick") && !html.contains("<mark>") && !html.contains("<img"), "attributes, unclosed tags, and other tags stay text: {html}");
        assert!(html.contains("<kbd>&lt;img src=x&gt;</kbd>"), "{html}");
    }

    #[test]
    fn dates_paths_and_the_html_extension() {
        assert_eq!(date(0), "1970-01-01 00:00 UTC");
        assert_eq!(date(1_762_958_580_000), "2025-11-12 14:43 UTC");
        let p = default_path(Path::new("/w"), &task(), None);
        assert_eq!(p, Path::new("/w/.task-x/exports/2025-11-12-fix-the-login-bug-abc123.html"));
        let rec = three_turns();
        let s = span(&rec, Range { from: Some(2), to: None }).unwrap();
        assert!(default_path(Path::new("/w"), &rec, Some(&s)).ends_with("2025-11-12-fix-the-login-bug-abc123-turns-2-3.html"));
        assert!(write(Path::new("/tmp/x.txt"), "").unwrap_err().contains(".html"));
    }
}
