//! Validation tools that run the project's detected commands (argv arrays, never shell strings).

use super::exec;
use super::schema::{int, object};
use super::{Handler, Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse};
use crate::context::project::Command;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;

const MAX_EXTRA_ARGS: usize = 16;
const MAX_ARG_LEN: usize = 200;
const MAX_DIAGNOSTICS: usize = 200;

#[derive(Clone, Copy)]
enum Kind {
    Format,
    Lint,
    Test,
    Build,
    Typecheck,
}

fn def(name: &'static str, description: &'static str, secs: u64, schema: fn() -> Value, handler: Handler) -> ToolDef {
    ToolDef { name, description, schema, permission: Permission::Execute, timeout: Duration::from_secs(secs), handler }
}

fn choose_schema() -> Value {
    object(json!({ "index": int("Which detected command to run when there are several (default 0)", 0) }), &[])
}

const MAX_COMMAND_ARGS: usize = 64;
const MAX_COMMAND_ARG_LEN: usize = 4_096;
const DEFAULT_COMMAND_SECS: u64 = 120;
const MAX_COMMAND_SECS: u64 = 600;

pub fn tools() -> Vec<ToolDef> {
    vec![
        def(
            "run_command",
            "Run a command in the workspace, like a terminal, and return its exit code and output. Pass the program and its arguments as an array (no shell: pipes, redirects, globs, and `&&` are not interpreted; run `sh -c` only when the user asks for shell features). Prefer the project tools (run_tests, check_build) when they fit. Set background: true for slow commands (installs, builds, long test runs): the call returns at once, the output comes to you as a message when the command ends, and you keep working meanwhile.",
            MAX_COMMAND_SECS + 5,
            || {
                object(
                    json!({
                        "command": {
                            "type": ["array", "string"],
                            "items": { "type": "string" },
                            "minItems": 1,
                            "maxItems": MAX_COMMAND_ARGS,
                            "description": "Program and arguments, e.g. [\"npm\", \"run\", \"lint\"]. A plain line (\"npm run lint\") is split the same way; quotes group words."
                        },
                        "cwd": { "type": "string", "description": "Working directory relative to the workspace root (default '.')" },
                        "timeoutSecs": int("Timeout in seconds (default 120, max 600)", 1),
                        "background": { "type": "boolean", "description": "Run in the background; do not wait or poll for it. Builds, tests, and installs default to true" }
                    }),
                    &["command"],
                )
            },
            run_command,
        ),
        def("format_code", "Run the project's formatter (detected from manifests).", 120, choose_schema, |c, i| {
            run_kind(c, i, Kind::Format)
        }),
        def("run_linter", "Run the project's linter.", 300, choose_schema, |c, i| run_kind(c, i, Kind::Lint)),
        def(
            "run_tests",
            "Run the project's tests. `args` are appended to the test command (e.g. a test name filter).",
            600,
            || {
                object(
                    json!({ "index": int("Which detected test command (default 0)", 0), "args": { "type": "array", "items": { "type": "string" }, "maxItems": MAX_EXTRA_ARGS } }),
                    &[],
                )
            },
            |c, i| run_kind(c, i, Kind::Test),
        ),
        def("check_build", "Build or compile the project to check for errors.", 600, choose_schema, |c, i| run_kind(c, i, Kind::Build)),
        def(
            "get_diagnostics",
            "Type-check or build the project and return parsed errors and warnings with file and line.",
            600,
            choose_schema,
            get_diagnostics,
        ),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct In {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    args: Vec<String>,
}

fn pick(c: &ToolCtx, kind: Kind, index: usize) -> Result<(Command, Vec<Command>), ToolError> {
    let all = c.with_workspace(|w| {
        let p = &w.project.commands;
        match kind {
            Kind::Format => p.format.clone(),
            Kind::Lint => p.lint.clone(),
            Kind::Test => p.test.clone(),
            Kind::Build => p.build.clone(),
            Kind::Typecheck => {
                if p.typecheck.is_empty() {
                    p.build.clone()
                } else {
                    p.typecheck.clone()
                }
            }
        }
    })?;
    let cmd = all.get(index).cloned().ok_or_else(|| {
        ToolError::new(
            ToolErrorKind::NotFound,
            if all.is_empty() {
                "no command of this kind was detected for the project".into()
            } else {
                format!("index {index} out of range ({} commands)", all.len())
            },
        )
    })?;
    Ok((cmd, all))
}

fn execute(c: &ToolCtx, cmd: &Command, extra: &[String], timeout: Duration) -> Result<exec::Output, ToolError> {
    let (abs, label) = c.resolve(if cmd.cwd.is_empty() { "." } else { &cmd.cwd })?;
    let mut argv = cmd.argv.clone();
    if !extra.is_empty() {
        // npm-style runners need `--` before arguments meant for the script.
        if matches!(argv.first().map(String::as_str), Some("npm" | "pnpm" | "yarn" | "bun")) {
            argv.push("--".into());
        }
        argv.extend(extra.iter().cloned());
    }
    exec::run_sandboxed(&argv, &abs, &label, timeout, &c.cancel, &exec::Sandbox { writable_root: &c.root })
}

fn validate_args(args: &[String]) -> Result<(), ToolError> {
    if args.len() > MAX_EXTRA_ARGS || args.iter().any(|a| a.is_empty() || a.len() > MAX_ARG_LEN || a.contains('\0')) {
        return Err(ToolError::invalid(format!("args: at most {MAX_EXTRA_ARGS} non-empty values of up to {MAX_ARG_LEN} characters")));
    }
    Ok(())
}

fn timeout_for(kind: Kind) -> Duration {
    Duration::from_secs(match kind {
        Kind::Format => 115,
        Kind::Lint => 295,
        _ => 595,
    })
}

fn run_kind(c: &ToolCtx, input: Value, kind: Kind) -> ToolResult {
    let i: In = parse(input)?;
    if !matches!(kind, Kind::Test) && !i.args.is_empty() {
        return Err(ToolError::invalid("args are only accepted by run_tests"));
    }
    validate_args(&i.args)?;
    let (cmd, all) = pick(c, kind, i.index.unwrap_or(0))?;
    let out = execute(c, &cmd, &i.args, timeout_for(kind))?;
    if matches!(kind, Kind::Format) && out.success {
        // Formatters rewrite files; refresh the index so later reads and edits see the new content.
        if let Some(w) = c.workspace.write().expect("workspace lock").as_mut() {
            w.refresh();
        }
        c.seen.lock().expect("seen lock").clear();
    }
    Ok(json!({ "result": out, "available": all.iter().map(|c| c.argv.join(" ")).collect::<Vec<_>>() }))
}

/// A command as a model sends it: the schema's array, a JSON array inside a string
/// (`"[\"cargo\",\"clippy\"]"`), or a plain command line (`cargo clippy --all-targets`). The strict
/// `Vec<String>` reader answered the last two with `invalid type: string …, expected a sequence`,
/// which says nothing about what to send.
#[derive(Deserialize)]
#[serde(untagged)]
enum Argv {
    List(Vec<String>),
    Text(String),
}

impl Argv {
    fn into_vec(self) -> Result<Vec<String>, ToolError> {
        match self {
            Argv::List(v) => Ok(v),
            Argv::Text(s) => {
                let s = s.trim();
                if s.is_empty() {
                    return Err(ToolError::invalid("command is empty"));
                }
                // A JSON array inside a string: what a model sends when it quotes the schema back.
                if s.starts_with('[') {
                    if let Ok(v) = serde_json::from_str::<Vec<String>>(s) {
                        return Ok(v);
                    }
                }
                Ok(split_args(s))
            }
        }
    }
}

/// A command line split into argv. Whitespace separates words, `'…'` and `"…"` group them, and a
/// backslash escapes the next character (inside `"…"` only `$`, `` ` ``, `"`, and `\`, as in sh, so
/// `"a\|b"` keeps its backslash). Nothing is interpreted: no pipes, globs, or variables, so
/// `a | b` becomes three arguments exactly as the tool documents.
pub(super) fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('\'') => cur.push(c),
            Some(_) if c == '\\' => {
                if let Some(n) = chars.next() {
                    if !matches!(n, '$' | '`' | '"' | '\\' | '\n') {
                        cur.push('\\');
                    }
                    cur.push(n);
                }
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    started = true;
                }
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Input of `run_command`; at module scope so the shapes it accepts can be tested directly.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunIn {
    command: Argv,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    background: bool,
}

fn run_command(c: &ToolCtx, input: Value) -> ToolResult {
    let i: RunIn = parse(input)?;
    if i.background {
        // The agent loop runs background commands itself; other callers (MCP) cannot be told later.
        return Err(ToolError::invalid("background is not available here; run the command without it"));
    }
    // A plain line keeps its pipes for the search reader; a JSON array in a string is argv.
    let line = match &i.command {
        Argv::Text(s) if !s.trim_start().starts_with('[') => Some(s.clone()),
        _ => None,
    };
    let command = i.command.into_vec()?;
    if command.is_empty() || command.len() > MAX_COMMAND_ARGS || command.iter().any(|a| a.len() > MAX_COMMAND_ARG_LEN || a.contains('\0')) {
        return Err(ToolError::invalid(format!(
            "command must have 1..={MAX_COMMAND_ARGS} arguments of up to {MAX_COMMAND_ARG_LEN} characters"
        )));
    }
    if command[0].trim().is_empty() {
        return Err(ToolError::invalid("the program name is empty"));
    }
    let (abs, label) = c.resolve(i.cwd.as_deref().unwrap_or("."))?;
    if !abs.is_dir() {
        return Err(ToolError::invalid(format!("{label} is not a directory")));
    }
    // grep, rg, and find are answered from the index, with the search tool to call next time.
    if let Some(r) = super::shell_search::answer(c, &command, line.as_deref(), &label) {
        return r;
    }
    let timeout = Duration::from_secs(i.timeout_secs.unwrap_or(DEFAULT_COMMAND_SECS).clamp(1, MAX_COMMAND_SECS));
    let out = exec::run_sandboxed(&command, &abs, &label, timeout, &c.cancel, &exec::Sandbox { writable_root: &c.root })?;
    // Commands can change files: refresh the index and require re-reads before edits.
    if let Some(w) = c.workspace.write().expect("workspace lock").as_mut() {
        w.refresh();
    }
    c.seen.lock().expect("seen lock").clear();
    Ok(json!({ "result": out }))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: String,
    pub line: u32,
    pub column: Option<u32>,
    pub severity: String,
    pub message: String,
}

/// Parses common compiler output formats: rustc (`--> file:l:c`), tsc (`file(l,c): error`),
/// and the generic `file:l:c: severity: message` used by gcc, eslint-unix, go, python tools.
pub fn parse_diagnostics(output: &str) -> Vec<Diagnostic> {
    static RUSTC: OnceLock<Regex> = OnceLock::new();
    static TSC: OnceLock<Regex> = OnceLock::new();
    static GENERIC: OnceLock<Regex> = OnceLock::new();
    let rustc = RUSTC.get_or_init(|| Regex::new(r"^\s*-->\s+([^:\s][^:]*):(\d+):(\d+)").expect("regex"));
    let tsc = TSC.get_or_init(|| Regex::new(r"^([^\s(][^(]*)\((\d+),(\d+)\):\s+(error|warning)\s+\w*:?\s*(.*)$").expect("regex"));
    let generic = GENERIC
        .get_or_init(|| Regex::new(r"^([^\s:][^:]*\.[A-Za-z0-9]+):(\d+):(?:(\d+):)?\s*(error|warning|note)?:?\s*(.+)$").expect("regex"));
    let mut out = Vec::new();
    let mut last_rust_msg: Option<(String, String)> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("error").or_else(|| line.strip_prefix("warning")) {
            if let Some(msg) = rest.split_once(": ").map(|(_, m)| m) {
                let sev = if line.starts_with("error") { "error" } else { "warning" };
                last_rust_msg = Some((sev.into(), msg.trim().into()));
                continue;
            }
        }
        if let Some(m) = rustc.captures(line) {
            let (severity, message) = last_rust_msg.take().unwrap_or(("error".into(), String::new()));
            out.push(Diagnostic {
                path: m[1].trim().into(),
                line: m[2].parse().unwrap_or(0),
                column: m[3].parse().ok(),
                severity,
                message,
            });
        } else if let Some(m) = tsc.captures(line) {
            out.push(Diagnostic {
                path: m[1].trim().into(),
                line: m[2].parse().unwrap_or(0),
                column: m[3].parse().ok(),
                severity: m[4].into(),
                message: m[5].trim().into(),
            });
        } else if let Some(m) = generic.captures(line) {
            out.push(Diagnostic {
                path: m[1].trim().into(),
                line: m[2].parse().unwrap_or(0),
                column: m.get(3).and_then(|c| c.as_str().parse().ok()),
                severity: m.get(4).map_or("error", |s| s.as_str()).into(),
                message: m[5].trim().into(),
            });
        }
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
    out
}

fn get_diagnostics(c: &ToolCtx, input: Value) -> ToolResult {
    let i: In = parse(input)?;
    let (cmd, _) = pick(c, Kind::Typecheck, i.index.unwrap_or(0))?;
    let out = execute(c, &cmd, &[], timeout_for(Kind::Build))?;
    let diags: Vec<Value> = parse_diagnostics(&format!("{}\n{}", out.stdout, out.stderr))
        .into_iter()
        .map(|d| json!({ "path": d.path, "line": d.line, "column": d.column, "severity": d.severity, "message": d.message }))
        .collect();
    Ok(
        json!({ "command": out.argv, "success": out.success, "timedOut": out.timed_out, "diagnostics": diags, "outputTail": if diags.is_empty() { out.stderr } else { String::new() } }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_tsc_and_generic_diagnostics() {
        let out = "error[E0425]: cannot find value `x` in this scope\n  --> src/main.rs:3:5\n   |\nsrc/app.ts(10,7): error TS2322: Type 'string' is not assignable to type 'number'.\npkg/x.go:4:2: undefined: foo\n";
        let d = parse_diagnostics(out);
        assert_eq!(
            d[0],
            Diagnostic {
                path: "src/main.rs".into(),
                line: 3,
                column: Some(5),
                severity: "error".into(),
                message: "cannot find value `x` in this scope".into()
            }
        );
        assert_eq!((d[1].path.as_str(), d[1].line, d[1].column), ("src/app.ts", 10, Some(7)));
        assert!(d[1].message.starts_with("Type 'string'"));
        assert_eq!((d[2].path.as_str(), d[2].line, d[2].message.as_str()), ("pkg/x.go", 4, "undefined: foo"));
    }

    /// The three shapes a model sends for `command`. A string used to fail with
    /// `invalid type: string "…", expected a sequence`, which never said what to send.
    #[test]
    fn run_command_takes_an_array_a_json_array_string_or_a_plain_line() {
        let argv = |v: Value| -> Vec<String> {
            let parsed: RunIn = serde_json::from_value(v).unwrap();
            parsed.command.into_vec().unwrap()
        };
        // The schema's array.
        assert_eq!(argv(json!({ "command": ["cargo", "clippy"] })), ["cargo", "clippy"]);
        // A JSON array inside a string — the reported case.
        assert_eq!(
            argv(json!({ "command": r#"["cargo", "clippy", "--manifest-path", "runtime/Cargo.toml", "--all-targets"]"# })),
            ["cargo", "clippy", "--manifest-path", "runtime/Cargo.toml", "--all-targets"]
        );
        // A plain line, with quotes grouping words that contain spaces.
        assert_eq!(argv(json!({ "command": "cargo clippy --all-targets" })), ["cargo", "clippy", "--all-targets"]);
        assert_eq!(argv(json!({ "command": "sh -c 'echo a | b'" })), ["sh", "-c", "echo a | b"]);
        assert_eq!(argv(json!({ "command": "git commit -m \"two words\"" })), ["git", "commit", "-m", "two words"]);
        // Nothing is interpreted: a pipe stays one argument.
        assert_eq!(argv(json!({ "command": "ls | head" })), ["ls", "|", "head"]);
        // Inside double quotes a backslash stays unless it escapes $ ` " or \, as in sh.
        assert_eq!(argv(json!({ "command": r#"grep "a\|b" "say \"hi\"" x\ y"# })), ["grep", r"a\|b", "say \"hi\"", "x y"]);
        // Unusable values still fail clearly.
        let empty: RunIn = serde_json::from_value(json!({ "command": "   " })).unwrap();
        assert_eq!(empty.command.into_vec().unwrap_err().kind, ToolErrorKind::InvalidInput);
        let wrong: Result<RunIn, _> = serde_json::from_value(json!({ "command": 5 }));
        assert!(wrong.is_err(), "a number is not a command");
    }

    #[cfg(unix)]
    #[test]
    fn run_command_runs_argv_in_workspace_without_a_shell() {
        let c = super::super::tests::ctx(&[("sub/a.txt", "hello\n")]);
        let r = super::super::tests::run(&c, "run_command", json!({ "command": ["cat", "a.txt"], "cwd": "sub" })).unwrap();
        assert_eq!((r["result"]["stdout"].as_str(), r["result"]["exitCode"].as_i64()), (Some("hello\n"), Some(0)));
        let r = super::super::tests::run(&c, "run_command", json!({ "command": ["echo", "a | b && c"] })).unwrap();
        assert_eq!(r["result"]["stdout"], "a | b && c\n", "no shell interpretation");
        assert!(super::super::tests::run(&c, "run_command", json!({ "command": ["ls"], "cwd": "../" })).is_err());
        assert!(super::super::tests::run(&c, "run_command", json!({ "command": [] })).is_err());
    }

    #[test]
    fn rejects_bad_args() {
        assert!(validate_args(&["".into()]).is_err());
        assert!(validate_args(&vec!["a".into(); 17]).is_err());
        assert!(validate_args(&["my_test".into()]).is_ok());
    }
}
