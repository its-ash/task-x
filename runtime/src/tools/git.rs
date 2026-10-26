//! Read-only git tools. Mutating git operations are not exposed.

use super::exec;
use super::schema::{int, object, path};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LOG: usize = 100;
const LOG_FORMAT: &str = "--format=%H%x1f%an%x1f%aI%x1f%s";

pub fn tools() -> Vec<ToolDef> {
    use Permission::Read;
    vec![
        tool("git_status", "Current branch and changed files (git status).", Read, || object(json!({}), &[]), git_status),
        tool(
            "git_diff",
            "Show the diff of the working tree (or staged changes, or against a revision).",
            Read,
            || {
                object(
                    json!({ "staged": { "type": "boolean" }, "rev": { "type": "string", "description": "Compare against this revision" }, "path": path("Limit to this path") }),
                    &[],
                )
            },
            git_diff,
        ),
        tool(
            "git_log",
            "Recent commits (hash, author, date, subject).",
            Read,
            || object(json!({ "limit": int("Max commits (default 20, max 100)", 1), "path": path("Limit to this path") }), &[]),
            git_log,
        ),
        tool(
            "git_show",
            "Show one commit with its stat and patch.",
            Read,
            || object(json!({ "rev": { "type": "string" } }), &["rev"]),
            git_show,
        ),
        tool(
            "get_changed_files",
            "Paths changed relative to HEAD, including untracked files.",
            Read,
            || object(json!({}), &[]),
            get_changed_files,
        ),
        tool(
            "get_file_history",
            "Commits that changed a file (follows renames).",
            Read,
            || object(json!({ "path": path("File path"), "limit": int("Max commits (default 20, max 100)", 1) }), &["path"]),
            get_file_history,
        ),
    ]
}

fn git(c: &ToolCtx, args: &[&str]) -> Result<String, ToolError> {
    let mut argv: Vec<String> =
        ["git", "--no-pager", "-c", "core.quotepath=off", "-c", "color.ui=false"].iter().map(|s| s.to_string()).collect();
    argv.extend(args.iter().map(|s| s.to_string()));
    let o = exec::run(&argv, &c.root, ".", GIT_TIMEOUT, &c.cancel)?;
    if o.timed_out {
        return Err(ToolError::new(ToolErrorKind::Timeout, "git timed out"));
    }
    if !o.success {
        return Err(ToolError::new(ToolErrorKind::Failed, format!("git {}: {}", args.first().unwrap_or(&""), o.stderr.trim())));
    }
    Ok(o.stdout)
}

/// Revisions may be hashes, refs, or relative refs (`HEAD~2`); never options.
pub fn valid_rev(rev: &str) -> Result<&str, ToolError> {
    let ok = !rev.is_empty()
        && rev.len() <= 200
        && !rev.starts_with('-')
        && !rev.contains("..")
        && rev.chars().all(|c| c.is_ascii_alphanumeric() || "._/~^@{}-".contains(c));
    if ok { Ok(rev) } else { Err(ToolError::invalid(format!("invalid revision '{rev}'"))) }
}

fn parse_log(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('\x1f').collect();
            (p.len() == 4).then(|| json!({ "hash": p[0], "author": p[1], "date": p[2], "subject": p[3] }))
        })
        .collect()
}

fn git_status(c: &ToolCtx, _: Value) -> ToolResult {
    let out = git(c, &["status", "--porcelain=v1", "-b", "-uall"])?;
    let mut branch = None;
    let mut files = Vec::new();
    for l in out.lines() {
        if let Some(b) = l.strip_prefix("## ") {
            branch = Some(b.to_owned());
        } else if l.len() > 3 {
            files.push(json!({ "status": l[..2].trim(), "path": &l[3..] }));
        }
    }
    Ok(json!({ "branch": branch, "files": files, "clean": files.is_empty() }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffIn {
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

fn git_diff(c: &ToolCtx, input: Value) -> ToolResult {
    let i: DiffIn = parse(input)?;
    let mut args = vec!["diff", "--no-ext-diff"];
    if i.staged {
        args.push("--cached");
    }
    if let Some(r) = &i.rev {
        args.push(valid_rev(r)?);
    }
    let rel = i.path.as_deref().map(|p| c.resolve(p).map(|r| r.1)).transpose()?;
    args.push("--");
    if let Some(r) = &rel {
        args.push(r);
    }
    let diff = git(c, &args)?;
    Ok(json!({ "diff": diff, "empty": diff.is_empty() }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogIn {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    path: Option<String>,
}

fn log(c: &ToolCtx, limit: Option<usize>, path: Option<&str>, follow: bool) -> ToolResult {
    let n = format!("-n{}", limit.unwrap_or(20).clamp(1, MAX_LOG));
    let rel = path.map(|p| c.resolve(p).map(|r| r.1)).transpose()?;
    let mut args = vec!["log", n.as_str(), LOG_FORMAT];
    if follow {
        args.push("--follow");
    }
    args.push("--");
    if let Some(r) = &rel {
        args.push(r);
    }
    Ok(json!({ "commits": parse_log(&git(c, &args)?) }))
}

fn git_log(c: &ToolCtx, input: Value) -> ToolResult {
    let i: LogIn = parse(input)?;
    log(c, i.limit, i.path.as_deref(), false)
}

fn get_file_history(c: &ToolCtx, input: Value) -> ToolResult {
    let i: LogIn = parse(input)?;
    let p = i.path.ok_or_else(|| ToolError::invalid("path is required"))?;
    log(c, i.limit, Some(&p), true)
}

fn git_show(c: &ToolCtx, input: Value) -> ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct In {
        rev: String,
    }
    let i: In = parse(input)?;
    let out = git(c, &["show", "--stat", "--patch", "--no-ext-diff", valid_rev(&i.rev)?, "--"])?;
    Ok(json!({ "rev": i.rev, "show": out }))
}

fn get_changed_files(c: &ToolCtx, _: Value) -> ToolResult {
    let out = git(c, &["status", "--porcelain=v1", "-uall"])?;
    let files: Vec<Value> = out.lines().filter(|l| l.len() > 3).map(|l| json!({ "status": l[..2].trim(), "path": &l[3..] })).collect();
    Ok(json!({ "files": files }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{ctx, run};
    use super::*;

    #[test]
    fn reads_status_diff_log_and_rejects_option_injection() {
        let c = ctx(&[("a.txt", "one\n")]);
        let g = |a: &[&str]| std::process::Command::new("git").args(a).current_dir(&c.root).output().unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "first"]);
        std::fs::write(c.root.join("a.txt"), "two\n").unwrap();
        let s = run(&c, "git_status", json!({})).unwrap();
        assert_eq!(s["files"], json!([{ "status": "M", "path": "a.txt" }]));
        assert!(run(&c, "git_diff", json!({})).unwrap()["diff"].as_str().unwrap().contains("+two"));
        assert_eq!(run(&c, "git_log", json!({})).unwrap()["commits"][0]["subject"], "first");
        assert_eq!(run(&c, "get_file_history", json!({ "path": "a.txt" })).unwrap()["commits"].as_array().unwrap().len(), 1);
        assert!(run(&c, "git_show", json!({ "rev": "HEAD" })).unwrap()["show"].as_str().unwrap().contains("first"));
        for bad in ["--output=/tmp/x", "a..b", "x;rm"] {
            assert!(run(&c, "git_show", json!({ "rev": bad })).is_err(), "{bad}");
        }
        assert_eq!(run(&c, "get_changed_files", json!({})).unwrap()["files"][0]["path"], "a.txt");
    }
}
