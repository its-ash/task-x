//! Shell searches (`grep`, `egrep`, `fgrep`, `rg`, `find`) read as the index search tools.
//! `run_command` answers them from the index, and the Claude Code hook names the tool call to make
//! instead. A command that uses a feature the tools do not have (context lines, inverted matches,
//! redirects, `-exec`) is not read and runs as before.

use super::search::{MAX_LIMIT, build_regex, grep_where};
use super::validate::split_args;
use super::{ToolCtx, ToolError, ToolResult, exec};
use crate::context::index::globset;
use serde_json::{Value, json};
use std::path::Path;

/// A shell search, read into what the index tools need.
#[derive(Debug, PartialEq)]
pub enum Search {
    /// `search_regex`: a Rust regex over the files that match `globs` (all files when empty).
    Grep { pattern: String, case_sensitive: bool, globs: Vec<String>, files_only: bool, count: bool },
    /// `find_files`: the files that match `globs`.
    Find { globs: Vec<String> },
}

/// What a pipe does with the result: `sort [-r] [-u]`, `uniq`, `head -n N`, `wc -l`.
#[derive(Debug, PartialEq, Default)]
pub struct Post {
    sort: bool,
    reverse: bool,
    unique: bool,
    head: Option<usize>,
    count_lines: bool,
}

impl Search {
    /// The tool call that does the same search.
    pub fn call(&self) -> (&'static str, Value) {
        let glob = |g: &[String]| match g {
            [] => None,
            [one] => Some(one.clone()),
            many => Some(format!("{{{}}}", many.join(","))),
        };
        match self {
            Search::Grep { pattern, case_sensitive, globs, files_only, .. } => {
                let mut input = json!({ "pattern": pattern, "caseSensitive": case_sensitive });
                if let Some(g) = glob(globs) {
                    input["glob"] = json!(g);
                }
                (if *files_only { "search_files_by_content" } else { "search_regex" }, if *files_only { files_input(input) } else { input })
            }
            Search::Find { globs } => ("find_files", json!({ "glob": glob(globs).unwrap_or_else(|| "**".into()) })),
        }
    }
}

/// `search_files_by_content` takes a literal `query`; a regex goes to `search_regex` instead.
fn files_input(mut input: Value) -> Value {
    let p = input["pattern"].as_str().unwrap_or_default().to_owned();
    if regex::escape(&p) != p {
        return input;
    }
    input.as_object_mut().map(|o| o.remove("pattern"));
    input["query"] = json!(p);
    input
}

/// Reads a command (argv, or `sh -c "line"`) as a search. `root` is the workspace and `cwd` the
/// directory the command runs in, relative to it ("" for the root).
pub fn read(argv: &[String], root: &Path, cwd: &str) -> Option<(Search, Post)> {
    let prog = argv.first().map(|p| base(p))?;
    if matches!(prog, "sh" | "bash" | "zsh") {
        return match argv.get(1).map(String::as_str) {
            Some("-c" | "-lc") if argv.len() == 3 => read_line(&argv[2], root, cwd),
            _ => None,
        };
    }
    Some((read_one(argv, root, cwd)?, Post::default()))
}

/// Reads a shell line: an optional `cd DIR &&`, a search, then pipes to sort, uniq, head, or wc.
pub fn read_line(line: &str, root: &Path, cwd: &str) -> Option<(Search, Post)> {
    let parts = split_line(line)?;
    let (mut cwd, mut rest) = (cwd.to_owned(), &parts[..]);
    if let [(first, Some(Sep::And)), tail @ ..] = rest {
        let a = split_args(first);
        let [cd, dir] = a.as_slice() else { return None };
        if cd != "cd" {
            return None;
        }
        cwd = join(&cwd, dir.trim_end_matches('/'))?;
        rest = tail;
    }
    let [(search, _), pipes @ ..] = rest else { return None };
    if rest.iter().skip(1).any(|(_, s)| *s == Some(Sep::And)) || rest[..rest.len() - 1].iter().any(|(_, s)| *s != Some(Sep::Pipe)) {
        return None;
    }
    let found = read_one(&split_args(search), root, &cwd)?;
    let mut post = Post::default();
    for (seg, _) in pipes {
        read_post(&split_args(seg), &mut post)?;
    }
    Some((found, post))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sep {
    Pipe,
    And,
}

/// Splits a line at `|` and `&&` outside quotes. Anything else the shell would interpret
/// (`;`, `||`, `&`, redirects, `$(...)`, backticks) makes the line unreadable.
fn split_line(line: &str) -> Option<Vec<(String, Option<Sep>)>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                continue;
            }
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                    continue;
                }
                '|' if chars.peek() != Some(&'|') => {
                    out.push((std::mem::take(&mut cur), Some(Sep::Pipe)));
                    continue;
                }
                '&' if chars.peek() == Some(&'&') => {
                    chars.next();
                    out.push((std::mem::take(&mut cur), Some(Sep::And)));
                    continue;
                }
                ';' | '|' | '&' | '>' | '<' | '`' | '\n' => return None,
                '$' if chars.peek() == Some(&'(') => return None,
                _ => {}
            },
        }
        cur.push(c);
    }
    if quote.is_some() || cur.trim().is_empty() {
        return None;
    }
    out.push((cur, None));
    out.iter().all(|(s, _)| !s.trim().is_empty()).then_some(out)
}

fn read_post(a: &[String], post: &mut Post) -> Option<()> {
    let (prog, args) = a.split_first()?;
    match base(prog) {
        "sort" => {
            post.sort = true;
            for f in args {
                match f.as_str() {
                    "-r" => post.reverse = true,
                    "-u" => post.unique = true,
                    "-ru" | "-ur" => (post.reverse, post.unique) = (true, true),
                    "-n" | "-V" | "-f" => {}
                    _ => return None,
                }
            }
        }
        "uniq" if args.is_empty() => post.unique = true,
        "head" => {
            let n = match args {
                [] => 10,
                [f] if f.starts_with("--lines=") => f["--lines=".len()..].parse().ok()?,
                [f] if f.starts_with("-n") && f.len() > 2 => f[2..].parse().ok()?,
                [f] if f.starts_with('-') => f[1..].parse().ok()?,
                [n, v] if n == "-n" => v.parse().ok()?,
                _ => return None,
            };
            post.head = Some(post.head.map_or(n, |h: usize| h.min(n)));
        }
        "wc" if args == ["-l"] => post.count_lines = true,
        _ => return None,
    }
    Some(())
}

fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// `cwd/p` inside the workspace, without `./`; None for a path that leaves it.
fn join(cwd: &str, p: &str) -> Option<String> {
    let mut parts: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    if p.starts_with('/') {
        return None;
    }
    for s in p.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn read_one(a: &[String], root: &Path, cwd: &str) -> Option<Search> {
    let (prog, args) = a.split_first()?;
    match base(prog) {
        p @ ("grep" | "egrep" | "fgrep" | "rg") => read_grep(p, args, root, cwd),
        "find" => read_find(args, cwd),
        _ => None,
    }
}

fn is_glob(p: &str) -> bool {
    p.contains(['*', '?', '[', '{'])
}

/// The glob for one path argument: a folder means everything under it.
fn path_glob(p: &str, root: &Path, cwd: &str) -> Option<String> {
    let rel = join(cwd, p)?;
    if rel.is_empty() {
        return Some(String::new());
    }
    if is_glob(&rel) {
        return Some(rel);
    }
    let dir = matches!(p.trim_end_matches('/'), "." | ".." | "") || p.ends_with('/') || root.join(&rel).is_dir();
    Some(if dir { format!("{rel}/**") } else { rel })
}

fn read_grep(prog: &str, args: &[String], root: &Path, cwd: &str) -> Option<Search> {
    let rg = prog == "rg";
    let (mut extended, mut fixed) = (prog == "egrep" || rg, prog == "fgrep");
    let (mut icase, mut smart, mut word, mut files_only, mut count, mut recursive) = (false, false, false, false, false, rg);
    let (mut patterns, mut include, mut positional) = (Vec::<String>::new(), Vec::<String>::new(), Vec::<String>::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            positional.extend(it.by_ref().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, val) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v.to_owned())));
            let mut value = || val.clone().or_else(|| it.next().cloned());
            match name {
                "recursive" | "dereference-recursive" => recursive = true,
                "line-number" | "with-filename" | "no-filename" | "binary-files" | "color" | "colour" | "no-messages" | "no-heading"
                | "heading" | "hidden" => {}
                "ignore-case" => icase = true,
                "smart-case" => smart = true,
                "extended-regexp" => extended = true,
                "fixed-strings" => fixed = true,
                "basic-regexp" => extended = false,
                "word-regexp" => word = true,
                "files-with-matches" => files_only = true,
                "count" => count = true,
                "include" | "glob" | "iglob" => include.push(value()?),
                "regexp" => patterns.push(value()?),
                _ => return None,
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let flags: Vec<char> = a[1..].chars().collect();
            for (k, ch) in flags.iter().enumerate() {
                match ch {
                    'r' | 'R' => recursive = true,
                    'n' | 'H' | 'h' | 's' | 'I' | 'N' | '.' => {}
                    'i' => icase = true,
                    'S' if rg => smart = true,
                    'E' => extended = true,
                    'F' => fixed = true,
                    'G' if !rg => extended = false,
                    'w' => word = true,
                    'l' => files_only = true,
                    'c' => count = true,
                    'e' | 'g' => {
                        let rest: String = flags[k + 1..].iter().collect();
                        let v = if rest.is_empty() { it.next().cloned()? } else { rest };
                        if *ch == 'e' {
                            patterns.push(v)
                        } else if rg {
                            include.push(v)
                        } else {
                            return None;
                        }
                        break;
                    }
                    _ => return None,
                }
            }
            continue;
        }
        positional.push(a.clone());
    }
    if patterns.is_empty() {
        if positional.is_empty() {
            return None;
        }
        patterns.push(positional.remove(0));
    }
    // A plain grep with no file reads stdin; that is not a code search.
    if positional.is_empty() && !recursive {
        return None;
    }
    let parts: Vec<String> = patterns
        .iter()
        .map(|p| match (fixed, extended) {
            (true, _) => regex::escape(p),
            (false, true) => p.replace("\\<", "\\b").replace("\\>", "\\b"),
            (false, false) => from_basic(p),
        })
        .collect();
    let mut pattern =
        if parts.len() == 1 { parts[0].clone() } else { parts.iter().map(|p| format!("(?:{p})")).collect::<Vec<_>>().join("|") };
    if word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    let case_sensitive = !(icase || smart && !patterns.iter().any(|p| p.chars().any(char::is_uppercase)));
    let paths: Vec<String> = positional.iter().map(|p| path_glob(p, root, cwd)).collect::<Option<_>>()?;
    let paths = if paths.is_empty() { vec![join(cwd, "")?] } else { paths };
    Some(Search::Grep { pattern, case_sensitive, globs: combine(&paths, &include), files_only, count })
}

/// Path globs narrowed by `--include` globs (which grep applies to file names).
fn combine(paths: &[String], include: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        let under = if p.is_empty() { Some(String::new()) } else { p.strip_suffix("/**").map(|d| format!("{d}/")) };
        match under {
            Some(dir) if !include.is_empty() => out.extend(include.iter().map(|i| format!("{dir}**/{}", i.trim_start_matches("**/")))),
            _ if p.is_empty() => {}
            _ => out.push(p.clone()),
        }
    }
    out
}

/// A POSIX basic regex as a Rust regex: `\|`, `\(`, `\)`, `\{`, `\}`, `\+`, `\?` are operators,
/// and the same characters without a backslash are literal. `\<` and `\>` are word edges.
fn from_basic(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n @ ('|' | '(' | ')' | '{' | '}' | '+' | '?')) => out.push(n),
                Some('<' | '>') => out.push_str(r"\b"),
                Some(n) => {
                    out.push('\\');
                    out.push(n);
                }
                None => out.push_str(r"\\"),
            },
            '|' | '(' | ')' | '{' | '}' | '+' | '?' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

fn read_find(args: &[String], cwd: &str) -> Option<Search> {
    let start = args.iter().position(|a| a.starts_with('-') || a == "(" || a == "!").unwrap_or(args.len());
    let dirs: Vec<String> = args[..start].iter().map(|d| join(cwd, d.trim_end_matches('/'))).collect::<Option<_>>()?;
    let dirs = if dirs.is_empty() { vec![join(cwd, "")?] } else { dirs };
    let (mut names, mut paths) = (Vec::new(), Vec::new());
    let mut it = args[start..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-name" | "-iname" => names.push(it.next()?.clone()),
            "-path" | "-ipath" | "-wholename" => paths.push(it.next()?.trim_start_matches("./").to_owned()),
            "-type" if it.next()? == "f" => {}
            "-maxdepth" | "-mindepth" => {
                it.next()?;
            }
            "-o" | "-or" | "-print" => {}
            _ => return None,
        }
    }
    let under = |d: &str| if d.is_empty() { String::new() } else { format!("{d}/") };
    let mut globs: Vec<String> = Vec::new();
    for d in &dirs {
        if names.is_empty() && paths.is_empty() {
            globs.push(format!("{}**", under(d)));
        }
        globs.extend(names.iter().map(|n| format!("{}**/{n}", under(d))));
    }
    globs.extend(paths.iter().map(|p| join(cwd, p)).collect::<Option<Vec<_>>>()?);
    Some(Search::Find { globs })
}

/// Answers a shell search from the index, in the shape of a command result, with the tool call to
/// use next time. None when the command is not a search this module reads.
pub fn answer(c: &ToolCtx, argv: &[String], line: Option<&str>, cwd: &str) -> Option<ToolResult> {
    let rel = if cwd == "." { "" } else { cwd };
    let (search, post) = match line {
        Some(l) => read_line(l, &c.root, rel)?,
        None => read(argv, &c.root, rel)?,
    };
    let started = std::time::Instant::now();
    Some(run(c, &search).map(|(lines, cut)| {
        let lines = post.apply(lines);
        let (name, input) = search.call();
        let out = exec::Output {
            argv: argv.to_vec(),
            cwd: cwd.to_owned(),
            exit_code: Some(if lines.is_empty() { 1 } else { 0 }),
            success: !lines.is_empty(),
            timed_out: false,
            duration_ms: started.elapsed().as_millis() as u64,
            stdout: lines.iter().map(|l| format!("{l}\n")).collect(),
            stderr: String::new(),
            truncated: cut,
            sandboxed: false,
        };
        json!({
            "result": out,
            "answeredBy": { "tool": name, "input": input },
            "note": format!("task-x ran this search on the workspace index (it skips .gitignore'd files). Next time call {name} directly with this input instead of a shell search."),
        })
    }))
}

impl Post {
    fn apply(&self, mut lines: Vec<String>) -> Vec<String> {
        if self.sort {
            lines.sort();
            if self.reverse {
                lines.reverse();
            }
        }
        if self.unique {
            lines.dedup();
        }
        if let Some(n) = self.head {
            lines.truncate(n);
        }
        if self.count_lines { vec![lines.len().to_string()] } else { lines }
    }
}

/// The output lines of a search, grep style, and whether the match limit cut them.
fn run(c: &ToolCtx, s: &Search) -> Result<(Vec<String>, bool), ToolError> {
    match s {
        Search::Grep { pattern, case_sensitive, globs, files_only, count } => {
            let re = build_regex(pattern, false, *case_sensitive)?;
            let set = if globs.is_empty() { None } else { Some(globset(globs).map_err(ToolError::invalid)?) };
            let r = grep_where(c, &re, |rel| set.as_ref().is_none_or(|g| g.is_match(rel)), MAX_LIMIT, 0)?;
            let matches = r["matches"].as_array().cloned().unwrap_or_default();
            let path = |m: &Value| m["path"].as_str().unwrap_or_default().to_owned();
            let lines = if *files_only || *count {
                let mut files: Vec<(String, usize)> = Vec::new();
                for m in &matches {
                    match files.last_mut() {
                        Some((p, n)) if *p == path(m) => *n += 1,
                        _ => files.push((path(m), 1)),
                    }
                }
                files.into_iter().map(|(p, n)| if *count { format!("{p}:{n}") } else { p }).collect()
            } else {
                matches.iter().map(|m| format!("{}:{}:{}", path(m), m["line"], m["excerpt"].as_str().unwrap_or_default())).collect()
            };
            Ok((lines, r["more"].as_bool().unwrap_or(false)))
        }
        Search::Find { globs } => {
            let set = globset(globs).map_err(ToolError::invalid)?;
            c.with_workspace(|w| w.index.files().map(|f| f.rel.clone()).filter(|r| set.is_match(r)).collect::<Vec<_>>()).map(|v| (v, false))
        }
    }
}

/// For the Claude Code hook: the tool call that replaces a shell search, as a message. None when
/// the command is not one.
pub fn suggestion(line: &str, root: &Path) -> Option<String> {
    let (search, _) = read_line(line, root, "")?;
    let (name, input) = search.call();
    Some(format!(
        "Blocked: search the code with the task-x tools, not with grep, rg, or find in the shell. Call mcp__task-x__{name} with {input}. \
         Use search_regex for a pattern, search_text for literal text, find_files for file names, and find_symbol or find_references for code names."
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, run as call};
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        split_args(s)
    }

    #[test]
    fn reads_grep_find_and_pipes() {
        let root = std::env::temp_dir();
        let line = r#"grep -rn "ROAD_END\|createRoad\b\|journey" scripts/*.js scripts/*.mjs | sort"#;
        let (s, post) = read_line(line, &root, "").unwrap();
        assert_eq!(
            s,
            Search::Grep {
                pattern: r"ROAD_END|createRoad\b|journey".into(),
                case_sensitive: true,
                globs: vec!["scripts/*.js".into(), "scripts/*.mjs".into()],
                files_only: false,
                count: false
            }
        );
        assert!(post.sort);
        assert_eq!(
            s.call(),
            (
                "search_regex",
                json!({ "pattern": r"ROAD_END|createRoad\b|journey", "caseSensitive": true, "glob": "{scripts/*.js,scripts/*.mjs}" })
            )
        );
        // Basic regex: bare ( + ? are literal; -i, -w, --include, and `cd dir &&`.
        let (s, _) = read_line("cd src && grep -riw --include=*.ts 'a(b)+' .", &root, "").unwrap();
        assert_eq!(
            s,
            Search::Grep {
                pattern: r"\b(?:a\(b\)\+)\b".into(),
                case_sensitive: false,
                globs: vec!["src/**/*.ts".into()],
                files_only: false,
                count: false
            }
        );
        let (s, _) = read(&argv("rg -l -g '*.rs' fooBar"), &root, "").unwrap();
        assert_eq!(s.call(), ("search_files_by_content", json!({ "query": "fooBar", "caseSensitive": true, "glob": "**/*.rs" })));
        let (s, post) = read(&argv("sh -c \"find . -name '*.vue' -type f | head -n 5\""), &root, "").unwrap();
        assert_eq!((s, post.head), (Search::Find { globs: vec!["**/*.vue".into()] }, Some(5)));
        // Not searches the tools can do: context lines, inverted matches, stdin, redirects, -exec, output filters.
        for not in [
            "grep -A2 x src",
            "grep -v x a.js",
            "grep x",
            "grep x a > out",
            "find . -name '*.tmp' -delete",
            "git log | grep fix",
            "grep x a; rm b",
            "grep x ../other",
        ] {
            assert!(read_line(not, &root, "").is_none(), "{not}");
        }
    }

    #[test]
    fn run_command_answers_searches_from_the_index() {
        let c = ctx(&[("scripts/a.js", "const ROAD_END = 1;\nfoo();\n"), ("scripts/b.mjs", "createRoad();\n"), ("lib/c.js", "ROAD_END\n")]);
        let r = call(
            &c,
            "run_command",
            json!({ "command": ["sh", "-c", r#"grep -rn "ROAD_END\|createRoad\b" scripts/*.js scripts/*.mjs | sort"#] }),
        )
        .unwrap();
        assert_eq!(r["result"]["stdout"], "scripts/a.js:1:const ROAD_END = 1;\nscripts/b.mjs:1:createRoad();\n", "{r}");
        assert_eq!((r["result"]["exitCode"].as_i64(), r["answeredBy"]["tool"].as_str()), (Some(0), Some("search_regex")));
        let n = call(&c, "run_command", json!({ "command": "grep -rl ROAD_END . | wc -l" })).unwrap();
        assert_eq!(n["result"]["stdout"], "2\n");
        let f = call(&c, "run_command", json!({ "command": "find scripts -name '*.mjs'" })).unwrap();
        assert_eq!(f["result"]["stdout"], "scripts/b.mjs\n");
        let none = call(&c, "run_command", json!({ "command": "grep -rn missing_name ." })).unwrap();
        assert_eq!((none["result"]["exitCode"].as_i64(), none["result"]["stdout"].as_str()), (Some(1), Some("")));
    }

    #[test]
    fn the_hook_names_the_tool_call() {
        let msg = suggestion("grep -rn 'createRoad' scripts", Path::new("/nonexistent")).unwrap();
        assert!(msg.starts_with("Blocked") && msg.contains("mcp__task-x__search_regex") && msg.contains("createRoad"), "{msg}");
        assert!(suggestion("cargo test", Path::new(".")).is_none());
    }
}
