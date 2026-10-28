//! Long command output, shortened for the model. A test or build run can print tens of thousands of
//! characters, and every later turn sends them again. The model gets the error lines (with a little
//! context) and the last lines; the full output is saved as a log that `read_log` reads on demand.

use super::schema::{int, object};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

/// Output up to this size goes to the model whole.
pub const KEEP_CHARS: usize = 8_000;
const HEAD_LINES: usize = 15;
const TAIL_LINES: usize = 40;
const CONTEXT_AFTER: usize = 2;
const MAX_IMPORTANT_CHARS: usize = 4_000;
const MAX_LINE_CHARS: usize = 400;
const MAX_READ_LINES: usize = 400;
const MAX_GREP_LINES: usize = 200;
const LOG_TTL: Duration = Duration::from_secs(24 * 3600);

static IMPORTANT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(error|errors|failed|failure|fail|panicked|panic|exception|traceback|assert\w*|warning|not ok|undefined|cannot|unresolved)\b|✗|✖|FAIL\b|E\d{4}")
        .expect("important line regex")
});
static LOG_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]{1,120}\.log$").expect("log name regex"));

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "read_log",
        "Read the full output of an earlier command whose result was shortened (the result names the log). Give start/end for a line range, or grep for the matching lines only.",
        Permission::Read,
        || {
            object(
                json!({
                    "log": { "type": "string", "description": "Log name from the shortened result" },
                    "start": int("First line, 1-based", 1),
                    "end": int("Last line", 1),
                    "grep": { "type": "string", "description": "Regular expression; returns matching lines with their numbers" }
                }),
                &["log"],
            )
        },
        read_log,
    )]
}

fn log_dir() -> PathBuf {
    std::env::temp_dir().join("task-x-logs")
}

/// A safe file name for a log, from task and call ids.
pub fn log_name(hint: &str) -> String {
    let mut s: String = hint.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(100).collect();
    if s.is_empty() {
        s = "output".into();
    }
    format!("{s}.log")
}

fn save(name: &str, text: &str) -> Option<()> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    // Old logs go away by themselves; a failed cleanup is harmless.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > LOG_TTL);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    std::fs::write(dir.join(name), text).ok()
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}…")
}

/// The first lines, the error lines with a little context, and the last lines, with markers where
/// lines were left out. Text within `KEEP_CHARS` is returned unchanged.
pub fn shorten_text(text: &str) -> Option<String> {
    if text.len() <= KEEP_CHARS {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut keep = vec![false; n];
    for k in keep.iter_mut().take(HEAD_LINES.min(n)) {
        *k = true;
    }
    for k in keep.iter_mut().skip(n.saturating_sub(TAIL_LINES)) {
        *k = true;
    }
    let mut budget = MAX_IMPORTANT_CHARS;
    for (i, l) in lines.iter().enumerate() {
        if budget == 0 {
            break;
        }
        if IMPORTANT.is_match(l) {
            for k in keep.iter_mut().skip(i).take(CONTEXT_AFTER + 1) {
                *k = true;
            }
            budget = budget.saturating_sub(l.len().min(MAX_LINE_CHARS));
        }
    }
    let mut out = String::new();
    let mut gap = 0;
    for (i, l) in lines.iter().enumerate() {
        if keep[i] {
            if gap > 0 {
                out.push_str(&format!("… {gap} lines left out …\n"));
                gap = 0;
            }
            out.push_str(&clip(l));
            out.push('\n');
        } else {
            gap += 1;
        }
    }
    if gap > 0 {
        out.push_str(&format!("… {gap} lines left out …\n"));
    }
    Some(out)
}

/// Shortens a command result (`{stdout, stderr}`, or one inside `result`) in place and saves the
/// full output as a log. Returns whether anything was shortened.
pub fn shorten(v: &mut Value, hint: &str) -> bool {
    let target = if v["result"]["stdout"].is_string() || v["result"]["stderr"].is_string() {
        &mut v["result"]
    } else if v["stdout"].is_string() || v["stderr"].is_string() {
        v
    } else {
        return false;
    };
    let (out, err) = (target["stdout"].as_str().unwrap_or("").to_owned(), target["stderr"].as_str().unwrap_or("").to_owned());
    let (short_out, short_err) = (shorten_text(&out), shorten_text(&err));
    if short_out.is_none() && short_err.is_none() {
        return false;
    }
    let name = log_name(hint);
    let saved = save(&name, &format!("{out}{}{err}", if err.is_empty() { "" } else { "\n--- stderr ---\n" })).is_some();
    if let Some(s) = short_out {
        target["stdout"] = json!(s);
    }
    if let Some(s) = short_err {
        target["stderr"] = json!(s);
    }
    target["shortened"] = json!(true);
    if saved {
        let total = out.lines().count() + err.lines().count();
        target["log"] = json!(name);
        target["note"] = json!(format!(
            "Output shortened to the error lines and the last lines ({total} lines in all). Call read_log with log \"{name}\" and a line range or grep for the rest."
        ));
    }
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadIn {
    log: String,
    #[serde(default, alias = "offset", alias = "startLine")]
    start: Option<usize>,
    #[serde(default, alias = "endLine")]
    end: Option<usize>,
    #[serde(default, alias = "pattern")]
    grep: Option<String>,
}

fn read_log(_: &ToolCtx, input: Value) -> ToolResult {
    let i: ReadIn = parse(input)?;
    if !LOG_NAME.is_match(&i.log) {
        return Err(ToolError::invalid("log must be a log name from a shortened result, such as \"t1-c2.log\""));
    }
    let text = std::fs::read_to_string(log_dir().join(&i.log))
        .map_err(|_| ToolError::new(ToolErrorKind::NotFound, format!("no log named {} (logs are kept for a day)", i.log)))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if let Some(p) = i.grep.filter(|g| !g.is_empty()) {
        let re = regex::RegexBuilder::new(&p).size_limit(1 << 20).build().map_err(|e| ToolError::invalid(format!("invalid grep: {e}")))?;
        let hits: Vec<Value> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| re.is_match(l))
            .take(MAX_GREP_LINES)
            .map(|(n, l)| json!({ "line": n + 1, "text": clip(l) }))
            .collect();
        return Ok(json!({ "log": i.log, "totalLines": total, "matches": hits }));
    }
    let start = i.start.unwrap_or(1).max(1);
    let end = i.end.unwrap_or(start + MAX_READ_LINES - 1).min(start + MAX_READ_LINES - 1).min(total);
    if start > total.max(1) {
        return Err(ToolError::invalid(format!("start {start} is past the end of {} ({total} lines)", i.log)));
    }
    let content: String = lines[start - 1..end].iter().enumerate().map(|(k, l)| format!("{:>6}\t{}\n", start + k, clip(l))).collect();
    Ok(json!({ "log": i.log, "startLine": start, "endLine": end, "totalLines": total, "content": content, "more": end < total }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn long_output_keeps_errors_and_the_tail_and_the_rest_is_readable() {
        let mut lines: Vec<String> =
            (1..=2_000).map(|n| format!("compiling crate_{n} v1.0.0 (some/long/path/that/adds/characters)")).collect();
        lines[1_200] = "error[E0308]: mismatched types in src/lib.rs:42".into();
        lines.push("test result: FAILED. 3 passed; 1 failed".into());
        let out = lines.join("\n");
        let mut v = json!({ "result": { "stdout": out, "stderr": "", "exitCode": 1 } });
        assert!(shorten(&mut v, "task-1:call 2"));
        let s = v["result"]["stdout"].as_str().unwrap();
        assert!(s.len() < KEEP_CHARS + MAX_IMPORTANT_CHARS + 6_000, "{}", s.len());
        assert!(s.contains("error[E0308]") && s.contains("test result: FAILED") && s.contains("lines left out"), "{s}");
        assert_eq!(v["result"]["exitCode"], 1, "the rest of the result is kept");
        let name = v["result"]["log"].as_str().unwrap().to_owned();
        assert_eq!(name, "task-1_call_2.log");
        let c = ctx(&[("a.txt", "a\n")]);
        let hit = run(&c, "read_log", json!({ "log": name, "grep": "E0308" })).unwrap();
        assert_eq!(hit["matches"][0]["line"], 1_201);
        let range = run(&c, "read_log", json!({ "log": name, "start": 1_199, "end": 1_201 })).unwrap();
        assert!(range["content"].as_str().unwrap().contains("mismatched types"));
        assert_eq!(run(&c, "read_log", json!({ "log": "../etc/passwd" })).unwrap_err().kind, ToolErrorKind::InvalidInput);
        let mut small = json!({ "stdout": "ok\n", "stderr": "" });
        assert!(!shorten(&mut small, "x"), "short output stays whole");
    }
}
