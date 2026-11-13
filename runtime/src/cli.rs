//! The terminal front end: `task-x-runtime exec "<prompt>"` runs one task with no one watching (for
//! scripts and CI) and exits with its status, and `task-x-runtime chat` is an interactive session.
//! Both drive the same agent as the VS Code extension, in the current folder (or `--cwd`).
//!
//! - The model: `--provider`, `--model`, `--base-url`, with the key in `ANTHROPIC_API_KEY`,
//!   `OPENAI_API_KEY`, or `TASK_X_API_KEY` (or the variable `--api-key-env` names). Defaults come
//!   from `TASK_X_PROVIDER`, `TASK_X_MODEL`, and `TASK_X_BASE_URL`.
//! - Permissions: `--edit safe` (default), `auto`, or `ask`; `--read-only`. Calls that need approval
//!   ask on the terminal, or are denied when there is no terminal (unless `--yes`).
//! - Tasks, the edit log, the project graph, and the workspace memory are saved in `~/.task-x/cli`.
//!   The workspace's `.task-x/hooks.json`, `.task-x/mcp.json`, skills, commands, agents, and plugins
//!   apply, as in a trusted VS Code workspace.

use crate::agent::policy::{EditMode, Mode, PolicyConfig};
use crate::agent::store::{Status, Store, TaskRecord};
use crate::agent::{self, ApprovalDecision, ApprovalRequest, AskRequest, Answers, ChatMode, Host, Limits, RunParams};
use crate::context::Workspace;
use crate::provider::{ProviderConfig, ProviderKind, Providers};
use crate::tools::{self, EditLog};
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub const USAGE: &str = "task-x: a coding agent for your terminal

Usage:
  task-x-runtime exec [options] <prompt...>   Run one task and exit (0 when it completes). Use - to read the prompt from stdin.
  task-x-runtime chat [options]               Start an interactive session.

Options:
  --provider <anthropic|openai|openaiCompatible|ollama>   Model provider (env TASK_X_PROVIDER)
  --model <id>              Model id (env TASK_X_MODEL)
  --base-url <url>          Endpoint for openaiCompatible or a non-default host (env TASK_X_BASE_URL)
  --api-key-env <NAME>      Environment variable that holds the API key
  --mode <ask|plan|agent|team>   Chat mode (default agent)
  --edit <safe|auto|ask>    What runs without asking (default safe)
  --read-only               Only read and search
  --yes                     Approve every call that would ask (exec)
  --allow <tool>, --deny <tool>   Never ask for, or always refuse, a tool (repeatable)
  --cwd <dir>               Work in this folder (default: the current folder)
  --continue                Continue the newest task in this folder
  --resume <task id>        Continue a task
  --max-turns <n>           Model turns before the task stops (default 60)
  --json                    Print the summary as JSON on stdout (exec)
  --quiet                   Print only the final answer
  --watch                   Index file changes while the session runs (chat)
  --no-mcp-import           Start only .task-x/mcp.json servers, not the ones set up for Claude, VS Code, Copilot CLI, or Cursor
  --trust                   Trust this folder: its hooks, MCP servers, skills, and agents may run (asked once otherwise)

In chat: /exit, /new, /cost, /compact, /checkpoints, /rewind <n>, /fork [n], /timeline, /restore <step>, /export [turns] [file.html] (asks for the start and end turn; or give 2-5), /agents, /mcp, /approvals, /allow|/ask|/deny <tool or server>, and every skill and command (/init, /review, ...).";

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub command: String,
    pub prompt: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub mode: ChatMode,
    pub edit: EditMode,
    pub read_only: bool,
    pub yes: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub continue_last: bool,
    pub resume: Option<String>,
    pub max_turns: Option<u32>,
    pub json: bool,
    pub quiet: bool,
    pub watch: bool,
    pub no_mcp_import: bool,
    pub trust: bool,
}

/// Reads the command line (without the program name).
pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut it = args.iter();
    let command = it.next().cloned().ok_or("give a command: exec or chat")?;
    if !matches!(command.as_str(), "exec" | "chat") {
        return Err(format!("unknown command '{command}': use exec or chat"));
    }
    let mut o = Options {
        command,
        prompt: String::new(),
        provider: None,
        model: None,
        base_url: None,
        api_key_env: None,
        mode: ChatMode::Agent,
        edit: EditMode::Safe,
        read_only: false,
        yes: false,
        allow: Vec::new(),
        deny: Vec::new(),
        cwd: None,
        continue_last: false,
        resume: None,
        max_turns: None,
        json: false,
        quiet: false,
        watch: false,
        no_mcp_import: false,
        trust: false,
    };
    let mut words: Vec<String> = Vec::new();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--provider" => o.provider = Some(value(a)?),
            "--model" => o.model = Some(value(a)?),
            "--base-url" => o.base_url = Some(value(a)?),
            "--api-key-env" => o.api_key_env = Some(value(a)?),
            "--mode" => {
                let v = value(a)?;
                o.mode = serde_json::from_value(json!(v)).map_err(|_| format!("--mode is ask, plan, agent, or team, not '{v}'"))?;
            }
            "--edit" => {
                let v = value(a)?;
                o.edit = serde_json::from_value(json!(v)).map_err(|_| format!("--edit is safe, auto, or ask, not '{v}'"))?;
            }
            "--read-only" => o.read_only = true,
            "--yes" | "-y" => o.yes = true,
            "--allow" => o.allow.push(value(a)?),
            "--deny" => o.deny.push(value(a)?),
            "--cwd" => o.cwd = Some(PathBuf::from(value(a)?)),
            "--continue" | "-c" => o.continue_last = true,
            "--resume" => o.resume = Some(value(a)?),
            "--max-turns" => o.max_turns = Some(value(a)?.parse().map_err(|_| "--max-turns needs a number")?),
            "--json" => o.json = true,
            "--quiet" | "-q" => o.quiet = true,
            "--watch" => o.watch = true,
            "--no-mcp-import" => o.no_mcp_import = true,
            "--trust" => o.trust = true,
            "--" => words.extend(it.by_ref().cloned()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => words.push(a.clone()),
        }
    }
    o.prompt = words.join(" ");
    if o.command == "exec" && o.prompt.trim().is_empty() {
        return Err("exec needs a prompt (or - to read it from stdin)".into());
    }
    Ok(o)
}

/// The provider from the options and the environment. With no provider named: Anthropic when its
/// key is set, else OpenAI when its key is set, else a local Ollama.
pub fn provider(o: &Options, env: &dyn Fn(&str) -> Option<String>) -> Result<ProviderConfig, String> {
    let name = o.provider.clone().or_else(|| env("TASK_X_PROVIDER")).unwrap_or_else(|| {
        if env("ANTHROPIC_API_KEY").is_some() {
            "anthropic".into()
        } else if env("OPENAI_API_KEY").is_some() {
            "openai".into()
        } else {
            "ollama".into()
        }
    });
    let kind: ProviderKind = serde_json::from_value(json!(name)).map_err(|_| format!("unknown provider '{name}'"))?;
    let key_var = o.api_key_env.clone().unwrap_or_else(|| {
        match kind {
            ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
            ProviderKind::OpenAi => "OPENAI_API_KEY",
            _ => "TASK_X_API_KEY",
        }
        .into()
    });
    let model = o.model.clone().or_else(|| env("TASK_X_MODEL")).unwrap_or_else(|| {
        match kind {
            ProviderKind::Anthropic => "claude-sonnet-5-5",
            ProviderKind::OpenAi => "gpt-5",
            ProviderKind::OpenAiCompatible => "",
            ProviderKind::Ollama => "qwen3",
        }
        .into()
    });
    if model.is_empty() {
        return Err("give a model with --model (or TASK_X_MODEL)".into());
    }
    let cfg = ProviderConfig { kind, model, base_url: o.base_url.clone().or_else(|| env("TASK_X_BASE_URL")), api_key: env(&key_var), timeout_ms: None };
    cfg.resolve().map_err(|e| format!("{}{}", e.message, if kind.requires_api_key() { format!(" (set {key_var})") } else { String::new() }))?;
    // The key the agent runs on stays out of the commands and MCP servers it starts.
    if cfg.api_key.is_some() {
        tools::exec::hide_env(&key_var);
    }
    Ok(cfg)
}

/// Colours of the wake-up lines: sleeping is magenta (☾), waking and firing yellow (⏰).
const SLEEP_COLOR: &str = "35";
const WAKE_COLOR: &str = "33";
const DIM: &str = "2";

/// Colour on a terminal, unless `NO_COLOR` is set.
fn paint(code: &str, text: &str) -> String {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()));
    if *ON { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
}

/// The moon's phases, one per second while the task sleeps.
const MOON: [&str; 8] = ["🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘"];

/// A countdown to the second: `1h 05m 03s`, `29m 58s`, `7s`.
fn countdown(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, s) => format!("{h}h {m:02}m {s:02}s"),
    }
}

/// What follows "next wake-up in …" on the sleeping line: `: <reason> (<n> pending)`.
fn sleeping_tail(event: &Value) -> String {
    let n = event["pending"].as_array().map_or(0, Vec::len);
    let reason = event["pending"][0]["reason"].as_str().map(|r| r.replace('\n', " ")).unwrap_or_default();
    let reason = if reason.chars().count() > 60 { format!("{}…", reason.chars().take(59).collect::<String>()) } else { reason };
    format!("{}{} ({n} pending)", if reason.is_empty() { "" } else { ": " }, reason)
}

/// One frame of the animated sleeping line: the moon of second `tick` and the time left.
fn sleep_frame(tick: usize, until_ms: Option<u64>, now_ms: u64, tail: &str) -> String {
    let when = match until_ms {
        Some(u) if u > now_ms => format!("next wake-up in {}", countdown(u - now_ms)),
        Some(_) => "waking up…".to_owned(),
        None => "waiting".to_owned(),
    };
    format!("{} Sleeping · {when}{tail}", MOON[tick % MOON.len()])
}

/// The live sleeping line on a terminal: one magenta line redrawn in place every second. Any
/// other output stops it and clears the line first, so nothing is printed into it.
#[derive(Default)]
struct SleepLine {
    /// (generation, running): a new animation, or `stop`, ends the drawing thread of the last one.
    state: Mutex<(u64, bool)>,
}

impl SleepLine {
    fn start(self: &Arc<Self>, until_ms: Option<u64>, tail: String) {
        let generation = {
            let mut s = self.state.lock().expect("sleep line lock");
            *s = (s.0 + 1, true);
            s.0
        };
        eprintln!();
        let me = self.clone();
        std::thread::spawn(move || {
            let live = |s: &(u64, bool)| s.0 == generation && s.1;
            for tick in 0.. {
                {
                    // Drawn under the lock, so `stop` never clears the line halfway through a frame.
                    let s = me.state.lock().expect("sleep line lock");
                    if !live(&s) {
                        return;
                    }
                    eprint!("\r\x1b[2K  {}", paint(SLEEP_COLOR, &sleep_frame(tick, until_ms, agent::store::now_ms(), &tail)));
                    let _ = std::io::stderr().flush();
                }
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if !live(&me.state.lock().expect("sleep line lock")) {
                        return;
                    }
                }
            }
        });
    }

    /// Stops the animation and clears its line. Nothing happens when none runs.
    fn stop(&self) {
        let mut s = self.state.lock().expect("sleep line lock");
        if std::mem::replace(&mut s.1, false) {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

/// Every agent of this session as an indented board: what each one does now. Shown when a
/// subagent starts or ends, and by `/agents`.
fn agents_board() -> String {
    let list = agent::activity::snapshot();
    if list.is_empty() {
        return "  No agent has run in this session yet.".into();
    }
    let running = list.iter().filter(|(_, s)| s.running).count();
    let mut out = format!("  Agents: {running} running, {} done\n", list.len() - running);
    let now = agent::store::now_ms();
    for (depth, s) in &list {
        let (mark, color) = match s.status.as_str() {
            _ if s.running && s.status == "waitingApproval" => ("?", "33"),
            _ if s.running && s.status == "waitingAnswer" => ("?", "33"),
            _ if s.running && s.status == "sleeping" => ("☾", SLEEP_COLOR),
            _ if s.running => ("●", "36"),
            "completed" => ("✓", "32"),
            _ => ("✗", "31"),
        };
        let what = match (&s.current_tool, s.running) {
            (Some(t), true) if s.status == "waitingApproval" => format!("waiting for your approval: {t}"),
            (Some(t), true) => t.clone(),
            (None, true) if s.status == "waitingAnswer" => "waiting for your answer".into(),
            (None, true) => s.status.clone(),
            _ => s.status.clone(),
        };
        let secs = (s.ended_ms.unwrap_or(now).saturating_sub(s.started_ms)) / 1000;
        let role = s.agent.as_deref().or(s.role.as_deref()).map(|r| format!(" ({r})")).unwrap_or_default();
        let title: String = s.title.chars().take(50).collect();
        out.push_str(&format!(
            "  {}{} {}{} · {} {}\n",
            "  ".repeat(*depth),
            paint(color, mark),
            s.name,
            role,
            what,
            paint(DIM, &format!("· turn {} · {} tools · {} tokens · {}s · {title}", s.iteration, s.tool_calls, s.tokens, secs))
        ));
    }
    out.trim_end().to_owned()
}

/// Longest `<kbd>…` held back while waiting for its `</kbd>`; past it the text goes out as it is.
const KEY_HOLD: usize = 48;

/// Shows `<kbd>Cmd</kbd>` in the streamed answer as a key (reverse video on a terminal, `[Cmd]`
/// in a pipe) and `<br>` as a line break. A tag cut between two pieces of the stream waits for
/// the next piece.
#[derive(Default)]
struct InlineKeys {
    carry: String,
}

impl InlineKeys {
    fn push(&mut self, piece: &str) -> String {
        let mut text = std::mem::take(&mut self.carry);
        text.push_str(piece);
        self.carry = text.split_off(held_from(&text));
        keys(&text, color_out())
    }

    fn flush(&mut self) -> String {
        keys(&std::mem::take(&mut self.carry), color_out())
    }
}

/// Where an unfinished tag at the end of `text` starts (`text.len()` when there is none).
fn held_from(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    if let Some(i) = lower.rfind("<kbd>").filter(|i| !lower[*i..].contains("</kbd>") && text.len() - i <= KEY_HOLD) {
        return i;
    }
    match text.rfind('<') {
        // `<`, `<k`, `</kb`: the start of a tag; `a < b` is just text.
        Some(i) if text.len() - i <= 7 && text[i + 1..].chars().all(|c| c.is_ascii_alphabetic() || c == '/') => i,
        _ => text.len(),
    }
}

/// Colour on standard output (where the answer goes), unless `NO_COLOR` is set.
fn color_out() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()));
    *ON
}

fn keys(text: &str, color: bool) -> String {
    static KBD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)<kbd>([^<\n]{1,40})</kbd>").expect("kbd regex"));
    static BR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)<br\s*/?>").expect("br regex"));
    if !text.contains('<') {
        return text.to_owned();
    }
    let text = BR.replace_all(text, "\n");
    KBD.replace_all(&text, |c: &regex::Captures| if color { format!("\x1b[7m {} \x1b[0m", &c[1]) } else { format!("[{}]", &c[1]) }).into_owned()
}

/// `30m` from now until `ms` (epoch milliseconds).
fn until(ms: &Value) -> String {
    let left = ms.as_u64().unwrap_or(0).saturating_sub(agent::store::now_ms());
    agent::wakeup::human(std::time::Duration::from_millis(left.div_ceil(1000) * 1000))
}

/// One line for the terminal from an agent event: (to stderr, text). `None` for events not shown.
pub fn render(event: &Value, quiet: bool) -> Option<(bool, String)> {
    let t = event["type"].as_str()?;
    if quiet {
        return None;
    }
    let short = |v: &Value| {
        let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
        let s = s.replace('\n', " ");
        if s.chars().count() > 100 { format!("{}…", s.chars().take(99).collect::<String>()) } else { s }
    };
    Some(match t {
        "textDelta" => (false, event["text"].as_str()?.to_owned()),
        "toolStarted" => {
            let input = &event["input"];
            let what = ["path", "command", "pattern", "query", "objective", "name"].iter().find_map(|k| input.get(*k).filter(|v| !v.is_null())).map(short).unwrap_or_default();
            (true, format!("\n  > {} {what}\n", event["tool"].as_str()?))
        }
        "toolFinished" if event["ok"] == false => (true, format!("  x {} failed: {}\n", event["tool"].as_str()?, short(&event["output"]))),
        "subagentStarted" => (true, format!("\n  + subagent {} ({}) started: {}\n", event["name"].as_str()?, event["agent"].as_str().or(event["role"].as_str()).unwrap_or("agent"), short(&event["objective"]))),
        "subagentFinished" => (true, format!("  + subagent {} {}\n", event["name"].as_str()?, event["status"].as_str().unwrap_or(""))),
        "subagent" => {
            let inner = &event["event"];
            if inner["type"] == "toolStarted" { (true, format!("    [{}] {}\n", event["name"].as_str()?, inner["tool"].as_str()?)) } else { return None }
        }
        "warning" => (true, format!("\n  ! {}\n", event["message"].as_str()?)),
        "error" => (true, format!("\n  ! {}\n", event["error"]["message"].as_str().unwrap_or("model error"))),
        "hook" if event["blocked"] == true => (true, format!("  ! a {} hook blocked: {}\n", event["event"].as_str().unwrap_or(""), short(&event["message"]))),
        "compacted" => (true, "  (history compressed)\n".into()),
        "retry" => (
            true,
            format!(
                "\n  ↻ {}retrying in {:.1}s (attempt {}): {}\n",
                if event["discardText"] == true { "the partial reply above is dropped; " } else { "" },
                event["waitMs"].as_u64().unwrap_or(0) as f64 / 1000.0,
                event["attempt"],
                short(&event["reason"])
            ),
        ),
        // The static line, for logs and pipes; a terminal animates it instead (`SleepLine`).
        "sleeping" => {
            (true, format!("\n  {}\n", paint(SLEEP_COLOR, &format!("☾ Sleeping · next wake-up in {}{}", until(&event["untilMs"]), sleeping_tail(event)))))
        }
        "awake" => {
            let by = match event["by"].as_str() {
                Some("wakeup") => "a wake-up is due",
                Some("message") => "a message arrived",
                _ => "nothing is pending",
            };
            (true, format!("  {}\n", paint(WAKE_COLOR, &format!("⏰ Awake: {by}"))))
        }
        "wakeupScheduled" => {
            let w = &event["wakeup"];
            let what = if w["kind"] == "watch" { "check" } else { "wake-up" };
            (true, format!("  {}\n", paint(DIM, &format!("⏰ {what} {} in {}: {}", w["id"].as_str()?, until(&w["dueMs"]), short(&w["reason"])))))
        }
        "wakeupFired" => {
            let note = match event["met"].as_bool() {
                Some(true) => " (passed)",
                Some(false) => " (did not pass)",
                None => "",
            };
            (true, format!("  {}\n", paint(WAKE_COLOR, &format!("⏰ {} fired{note}: {}", event["id"].as_str()?, short(&event["reason"])))))
        }
        "wakeupCancelled" if event["by"] == "model" => (true, format!("  {}\n", paint(DIM, &format!("⏰ {} cancelled", event["id"].as_str()?)))),
        _ => return None,
    })
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Asks a yes/no question on the terminal. No terminal: the default.
fn confirm(question: &str, default: bool) -> ApprovalDecision {
    if !std::io::stdin().is_terminal() {
        return if default { ApprovalDecision::Allow } else { ApprovalDecision::Deny };
    }
    eprint!("{question} [y]es / [a]lways / [N]o: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        "a" | "always" => ApprovalDecision::AllowSession,
        _ => ApprovalDecision::Deny,
    }
}

/// Puts the agent's questions on the terminal; no terminal dismisses them.
fn ask_terminal(req: &AskRequest) -> Option<Answers> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let mut a = Answers::default();
    let read = || {
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        line
    };
    // Every question has a recommendation: one key takes them all.
    if req.questions.iter().all(|q| !q.recommended.is_empty()) {
        eprintln!("\nThe agent asks {} question{}. Its recommendation:", req.questions.len(), if req.questions.len() == 1 { "" } else { "s" });
        for q in &req.questions {
            eprintln!("  {} → {}", q.text, paint("32", &q.recommended.join(", ")));
        }
        eprint!("Proceed with recommendation? [Y]es / [n]o, answer each: ");
        if matches!(read().trim().to_ascii_lowercase().as_str(), "" | "y" | "yes") {
            for q in &req.questions {
                a.selected.insert(q.id.clone(), q.recommended.clone());
            }
            return Some(a);
        }
    }
    for q in &req.questions {
        eprintln!("\n? {}", q.text);
        for (i, opt) in q.options.iter().enumerate() {
            let star = if q.recommended.contains(opt) { paint("32", "  ★ recommended") } else { String::new() };
            eprintln!("  {}) {opt}{star}", i + 1);
        }
        eprint!("answer (numbers, or your own text{}): ", if q.recommended.is_empty() { "" } else { "; Enter takes the recommendation" });
        let line = read();
        if line.trim().is_empty() && !q.recommended.is_empty() {
            a.selected.insert(q.id.clone(), q.recommended.clone());
            continue;
        }
        let picks: Vec<String> = line.split([',', ' ']).filter_map(|s| s.trim().parse::<usize>().ok()).filter_map(|n| q.options.get(n.wrapping_sub(1)).cloned()).collect();
        if picks.is_empty() && !line.trim().is_empty() {
            a.other.insert(q.id.clone(), line.trim().to_owned());
        }
        a.selected.insert(q.id.clone(), picks);
    }
    Some(a)
}

/// `/export [range] [file.html]`: a range is `3`, `2-5`, `2-` (to the end), or `-4` (from the
/// start); `all` is every turn. Either part may come first.
fn export_args(arg: &str) -> Result<(Option<agent::export::Range>, Option<String>), String> {
    let (mut range, mut path) = (None, None);
    for word in arg.split_whitespace() {
        let num = |s: &str| if s.is_empty() { Ok(None) } else { s.parse::<usize>().map(Some).map_err(|_| format!("'{word}' is not a turn range (use 3, 2-5, 2-, or -4)")) };
        if word.eq_ignore_ascii_case("all") {
            range = Some(agent::export::Range::default());
        } else if word.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '–') && word.chars().any(|c| c.is_ascii_digit()) {
            let r = match word.split_once(['-', '–']) {
                Some((a, b)) => agent::export::Range { from: num(a)?, to: num(b)? },
                None => agent::export::Range { from: num(word)?, to: num(word)? },
            };
            range = Some(r);
        } else if path.is_none() {
            path = Some(word.to_owned());
        } else {
            return Err("use /export [range] [file.html]".into());
        }
    }
    Ok((range, path))
}

/// Lists the turns and asks where the export starts and ends; Enter keeps the first and the last.
fn pick_turns(turns: &[agent::export::Turn]) -> Result<agent::export::Range, String> {
    let width = turns.iter().map(|t| t.prompt.chars().count()).max().unwrap_or(0).clamp(10, 60);
    eprintln!("Turns in this task:");
    for t in turns {
        let p: String = if t.prompt.chars().count() > width { format!("{}…", t.prompt.chars().take(width - 1).collect::<String>()) } else { t.prompt.clone() };
        eprintln!("  {:>3}  {p:<width$}  {}", t.n, paint(DIM, &agent::export::date(t.created_ms)));
    }
    let last = turns.len();
    let ask = |q: &str, default: usize| -> Result<usize, String> {
        eprint!("{q} [{default}]: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let line = line.trim();
        if line.is_empty() {
            return Ok(default);
        }
        line.parse::<usize>().ok().filter(|n| (1..=last).contains(n)).ok_or_else(|| format!("give a turn from 1 to {last}"))
    };
    let from = ask("Start at turn", 1)?;
    let to = ask("End at turn", last)?;
    if from > to {
        return Err(format!("the end ({to}) comes before the start ({from})"));
    }
    Ok(agent::export::Range { from: Some(from), to: Some(to) })
}

/// Whether this folder's own hooks, MCP servers, skills, and agents may run. Trusted folders are
/// remembered in `<data>/trusted.json`. `--trust` trusts it (and remembers that); otherwise a
/// terminal asks once, and with no terminal the folder stays untrusted. The home folder is the
/// user's own and always trusted.
fn workspace_trust(data: &std::path::Path, root: &std::path::Path, flag: bool, quiet: bool) -> bool {
    let file = data.join("trusted.json");
    let mut list: Vec<String> = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let key = root.to_string_lossy().into_owned();
    if list.contains(&key) || home().is_some_and(|h| h == root) {
        return true;
    }
    let yes = flag || {
        let has = |p: &str| root.join(p).exists();
        let ships = [".task-x", ".mcp.json", ".vscode/mcp.json", ".cursor/mcp.json", ".claude", ".github/prompts"].iter().any(|p| has(p));
        if !ships {
            // Nothing in it would run: trusting it changes nothing yet, so it is not asked about.
            return false;
        }
        if !std::io::stdin().is_terminal() {
            if !quiet {
                eprintln!("task-x: {} is not trusted, so its hooks, MCP servers, skills, and agents are off (use --trust).", root.display());
            }
            return false;
        }
        eprint!(
            "task-x: {} has its own hooks, MCP servers, skills, or agents, which can run commands.\nTrust this folder? [y]es / [N]o: ",
            root.display()
        );
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    if yes {
        list.push(key);
        let _ = std::fs::create_dir_all(data);
        let _ = std::fs::write(&file, serde_json::to_string_pretty(&list).unwrap_or_default());
    }
    yes
}

/// Ctrl-C while a task runs cancels the task (its record is saved as cancelled, and running
/// commands are stopped); a second Ctrl-C during the same task, or one at the prompt, quits.
fn watch_ctrl_c(current: Arc<Mutex<Option<CancellationToken>>>) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            let token = current.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            match token {
                Some(t) if !t.is_cancelled() => {
                    t.cancel();
                    eprintln!("\n  ^C Cancelling the task... (press Ctrl-C again to quit)");
                }
                _ => {
                    eprintln!();
                    std::process::exit(130);
                }
            }
        }
    });
}

struct Session {
    host: Host,
    options: Options,
    provider: ProviderConfig,
    task_id: Option<String>,
    /// The running task's token: Ctrl-C cancels it (see `watch_ctrl_c`).
    current: Arc<Mutex<Option<CancellationToken>>>,
    /// Where each MCP server came from, and what the import found or skipped.
    mcp_origins: std::collections::BTreeMap<String, String>,
    mcp_notes: Vec<String>,
}

impl Session {
    async fn open(o: Options) -> Result<Self, String> {
        let provider = provider(&o, &|k| std::env::var(k).ok().filter(|v| !v.is_empty()))?;
        let root = o.cwd.clone().map_or_else(std::env::current_dir, Ok).map_err(|e| e.to_string())?;
        let data = home().ok_or("no home folder")?.join(".task-x").join("cli");
        let quiet = o.quiet || o.json;
        if !quiet {
            eprintln!("task-x: indexing {} ...", root.display());
        }
        let mut ws = tokio::task::spawn_blocking({
            let root = root.clone();
            move || Workspace::open(&root, &[])
        })
        .await
        .map_err(|e| e.to_string())??;
        let key = format!("{:016x}", tools::fingerprint(&ws.index.root().to_string_lossy()));
        ws.graph_file = Some(data.join("graphs").join(format!("{key}.json")));
        let root = ws.index.root().to_path_buf();
        let store = Store::new(Some(&data));
        store.fail_interrupted();
        let edits = Arc::new(Mutex::new(EditLog::open(data.join("checkpoints").join(format!("{key}.json")))));
        // A folder's own hooks, MCP servers, skills, and agents run only once the user trusts it: a
        // freshly cloned repository could ship any of them.
        let trusted = workspace_trust(&data, &root, o.trust, quiet);
        let mut hook_list = if trusted { agent::hooks::load_workspace(&root).unwrap_or_default() } else { Vec::new() };
        if let Some(h) = home() {
            hook_list.extend(agent::hooks::load_workspace(&h).unwrap_or_default().into_iter().map(|h| agent::hooks::Hook { trusted: true, ..h }));
        }
        let mcp = Arc::new(tokio::sync::RwLock::new(crate::mcp::Manager::default()));
        // The servers set up for Claude, VS Code, Copilot CLI, and Cursor, then .task-x/mcp.json over them.
        let mut imported = crate::mcp_import::Imported::default();
        if !o.no_mcp_import {
            if let Some(h) = home() {
                let sources: Vec<String> = crate::mcp_import::SOURCES.iter().map(|s| s.to_string()).collect();
                imported = crate::mcp_import::import(&sources, &crate::mcp_import::Env::current(h, Some(root.clone()), trusted), &crate::mcp_import::read_file);
            }
        }
        let mut servers = imported.servers.clone();
        let mut origins = imported.origins.clone();
        for (name, cfg) in if trusted { crate::mcp::load_workspace(&root).unwrap_or_default() } else { Default::default() } {
            origins.insert(name.clone(), "task-x (.task-x/mcp.json)".into());
            servers.insert(name, cfg);
        }
        if !servers.is_empty() {
            if !quiet {
                eprintln!("task-x: starting {} MCP server{} ...", servers.len(), if servers.len() == 1 { "" } else { "s" });
            }
            mcp.write().await.configure(servers, Some(root.clone())).await;
        }
        let emit_quiet = quiet;
        let yes = o.yes;
        // The sleeping line animates on a real terminal only; logs and pipes get one static line.
        let animate = !quiet && std::io::stderr().is_terminal() && std::env::var("TERM").map_or(true, |t| t != "dumb");
        let sleep_line = Arc::new(SleepLine::default());
        let keys = Mutex::new(InlineKeys::default());
        let host = Host {
            providers: Providers::new().map_err(|e| e.message)?,
            workspace: Arc::new(RwLock::new(Some(ws))),
            seen: Arc::default(),
            edits,
            grants: Arc::default(),
            store,
            emit: Arc::new(move |_, ev| {
                if animate && ev["type"] == "sleeping" {
                    sleep_line.start(ev["untilMs"].as_u64(), sleeping_tail(&ev));
                    return;
                }
                if ev["type"] == "done" {
                    sleep_line.stop();
                }
                let delta = ev["type"] == "textDelta";
                if !delta {
                    // Any other event ends the answer text: what waited for a closing tag goes out first.
                    let rest = keys.lock().expect("keys lock").flush();
                    if !rest.is_empty() {
                        print!("{rest}");
                        let _ = std::io::stdout().flush();
                    }
                }
                // With several agents at work, each start and end shows the whole board.
                let board = !emit_quiet && matches!(ev["type"].as_str(), Some("subagentStarted" | "subagentFinished"));
                if let Some((err, text)) = render(&ev, emit_quiet) {
                    sleep_line.stop();
                    if board {
                        eprintln!("{text}{}", agents_board());
                        return;
                    }
                    if err {
                        eprint!("{text}");
                    } else {
                        let text = if delta { keys.lock().expect("keys lock").push(&text) } else { text };
                        print!("{text}");
                        let _ = std::io::stdout().flush();
                    }
                }
            }),
            approve: Arc::new(move |req: ApprovalRequest, _| {
                Box::pin(async move {
                    if yes {
                        return ApprovalDecision::Allow;
                    }
                    let what = req.preview.clone().unwrap_or_else(|| req.input.to_string());
                    tokio::task::spawn_blocking(move || confirm(&format!("\nAllow {} ({:?})?\n{what}\n", req.tool, req.permission), false)).await.unwrap_or(ApprovalDecision::Deny)
                })
            }),
            ask: Arc::new(|req: AskRequest, _| Box::pin(async move { tokio::task::spawn_blocking(move || ask_terminal(&req)).await.ok().flatten() })),
            mcp,
            hooks: Arc::new(RwLock::new(hook_list)),
            subagents: Arc::default(),
            steers: Arc::default(),
            tool_cancels: Arc::default(),
            plugins: Arc::new(RwLock::new(agent::plugins::Settings { workspace_trusted: trusted, ..Default::default() })),
        };
        let mut task_id = o.resume.clone();
        if o.continue_last && task_id.is_none() {
            task_id = host.store.list(1).first().and_then(|t| t["id"].as_str().map(str::to_owned));
        }
        let current: Arc<Mutex<Option<CancellationToken>>> = Arc::default();
        watch_ctrl_c(current.clone());
        Ok(Self { host, options: o, provider, task_id, current, mcp_origins: origins, mcp_notes: imported.notes })
    }

    async fn run(&mut self, prompt: &str) -> Result<TaskRecord, String> {
        let o = &self.options;
        let p = RunParams {
            prompt: prompt.to_owned(),
            images: Vec::new(),
            provider: self.provider.clone(),
            chat_mode: o.mode,
            policy: self.policy(),
            editor: Default::default(),
            limits: Limits { max_iterations: o.max_turns.unwrap_or(60).clamp(1, 500), lazy_tools: true, ..Default::default() },
            task_id: self.task_id.clone(),
            routes: Vec::new(),
            fallbacks: Vec::new(),
            prices: Vec::new(),
            role_providers: Vec::new(),
            helper: None,
            embeddings: None,
            depth: 0,
            agent: None,
        };
        let cancel = CancellationToken::new();
        *self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel.clone());
        let res = agent::run(self.host.clone(), p, cancel).await;
        *self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let rec = res?;
        self.task_id = Some(rec.id.clone());
        Ok(rec)
    }

    fn policy(&self) -> PolicyConfig {
        let o = &self.options;
        PolicyConfig {
            mode: if o.read_only { Mode::ReadOnly } else { Mode::Autonomous },
            allow: o.allow.clone(),
            deny: o.deny.clone(),
            allow_all: false,
            edit_mode: (!o.read_only).then_some(o.edit),
            sandboxed: None,
        }
    }

    /// `/approvals`: the MCP servers with each tool's decision, and task-x's tools that ask or are refused.
    async fn approvals(&self) -> String {
        use agent::approvals::Why;
        use agent::policy::Decision;
        let mcp = self.host.mcp.read().await.tools();
        let grants = self.host.grants.lock().expect("grants lock").clone();
        let a = agent::approvals::list(&self.policy(), &grants, &mcp);
        let word = |d: Decision| match d {
            Decision::Allow => paint("32", "allow"),
            Decision::Ask => paint("33", "ask  "),
            Decision::Deny => paint("31", "deny "),
        };
        let line = |t: &agent::approvals::ToolApproval| {
            let why = match (t.why, &t.rule) {
                (Why::Denied, Some(r)) => format!("  (deny list: {r})"),
                (Why::Allowed, Some(r)) => format!("  (allow list: {r})"),
                (Why::Session, _) => "  (allowed for this session)".into(),
                _ => String::new(),
            };
            format!("    {} {}{}", word(t.decision), t.tool, paint(DIM, &why))
        };
        let own: Vec<_> = a.tools.iter().filter(|t| t.server.is_none()).collect();
        let notable: Vec<_> = own.iter().filter(|t| t.decision != Decision::Allow || t.why != Why::Mode).collect();
        let mut out = format!("task-x tools: {} of {} run without asking.\n", own.iter().filter(|t| t.decision == Decision::Allow).count(), own.len());
        out.extend(notable.iter().map(|t| line(t) + "\n"));
        if a.servers.is_empty() {
            out.push_str("No MCP server is running.\n");
        }
        for s in &a.servers {
            out.push_str(&format!(
                "\n{} ({}): {} allowed, {} ask, {} refused\n",
                s.name,
                s.pattern,
                s.allowed,
                s.asks,
                s.denied
            ));
            out.extend(a.tools.iter().filter(|t| t.server.as_deref() == Some(s.name.as_str())).map(|t| line(t) + "\n"));
        }
        out + "\n/allow, /ask, or /deny <tool or server> changes it for this session (use --allow and --deny to start with them)."
    }

    /// `/mcp`: each server, where it came from, and whether it runs; then what the import skipped.
    async fn mcp_list(&self) -> String {
        let status = self.host.mcp.read().await.status();
        let mut out = String::new();
        for s in status["servers"].as_array().into_iter().flatten() {
            let name = s["name"].as_str().unwrap_or_default();
            let tools = s["tools"].as_array().map_or(0, Vec::len);
            let state = if s["disabled"] == true {
                paint(DIM, "off")
            } else if s["running"] == true {
                paint("32", &format!("running, {tools} tools"))
            } else {
                paint("31", &format!("not running: {}", s["error"].as_str().unwrap_or("no answer")))
            };
            let from = self.mcp_origins.get(name).map_or(String::new(), |o| format!("  {}", paint(DIM, &format!("from {o}"))));
            out.push_str(&format!("  {name}: {state}{from}\n"));
        }
        if out.is_empty() {
            out.push_str("  No MCP server is set up. task-x reads Claude, VS Code, Copilot CLI, and Cursor settings, and .task-x/mcp.json.\n");
        }
        let skipped: Vec<&String> = self.mcp_notes.iter().filter(|n| n.contains("skipped") || n.contains("could not") || n.contains("is named")).collect();
        if !skipped.is_empty() {
            out.push_str(&format!("Import notes:\n{}\n", skipped.iter().map(|n| format!("  {n}")).collect::<Vec<_>>().join("\n")));
        }
        format!("MCP servers:\n{out}See each tool's permission with /approvals; /allow, /ask, or /deny <server> sets a whole server.")
    }

    /// `/allow`, `/ask`, `/deny <name>`: a server name stands for all its tools, and a tool's short
    /// name for its full one when only one server has it.
    async fn set_approval(&mut self, choice: agent::approvals::Choice, arg: &str) -> Result<String, String> {
        let arg = arg.trim();
        if arg.is_empty() {
            return Err("give a tool or a server name (see /approvals)".into());
        }
        let mcp = self.host.mcp.read().await.tools();
        let short: Vec<_> = mcp.iter().filter(|t| t.name == arg).collect();
        let name = if mcp.iter().any(|t| t.server == arg) {
            agent::approvals::server_pattern(arg)
        } else if let [one] = short.as_slice() {
            one.qualified.clone()
        } else {
            arg.to_owned()
        };
        let e = agent::approvals::set(&self.policy(), &name, choice)?;
        (self.options.allow, self.options.deny) = (e.allow, e.deny);
        let did = match choice {
            agent::approvals::Choice::Allow => "runs without asking",
            agent::approvals::Choice::Ask => "follows the edit mode again",
            agent::approvals::Choice::Deny => "is refused",
        };
        let note = e.covered_by.map(|c| format!("\nNote: '{c}' still decides for it; change that entry too.")).unwrap_or_default();
        Ok(format!("{name} {did} for the rest of this session.{note}"))
    }

    fn root(&self) -> Result<PathBuf, String> {
        self.host.workspace.read().expect("workspace lock").as_ref().map(|w| w.index.root().to_path_buf()).ok_or_else(|| "no workspace is open".into())
    }

    /// The session commands of `chat`; `None` when the line is a prompt for the agent.
    async fn command(&mut self, line: &str) -> Option<Result<String, String>> {
        let (cmd, arg) = line.split_once(' ').map_or((line, ""), |(c, a)| (c, a.trim()));
        let store = self.host.store.clone();
        let need = |id: &Option<String>| id.clone().ok_or_else(|| "no task yet in this session".to_owned());
        Some(match cmd {
            "/timeline" => need(&self.task_id).and_then(|id| {
                let list = agent::timeline::Timeline::open(&self.root()?)?.list(&id)?;
                if list.is_empty() {
                    return Ok("This task has no timeline yet.".into());
                }
                Ok(list
                    .iter()
                    .map(|s| {
                        let files = if s.base { "before the task".to_owned() } else { format!("{} files", s.files.len()) };
                        format!("{:>3}  {}  ({files})", s.step, s.label)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n/restore <step> puts the files back to that step.")
            }),
            "/restore" => {
                let res = need(&self.task_id).and_then(|id| {
                    let step: usize = arg.parse().map_err(|_| "give the step number (see /timeline)")?;
                    let t = agent::timeline::Timeline::open(&self.root()?)?;
                    let snap = t.list(&id)?.into_iter().find(|s| s.step == step).ok_or("no such step (see /timeline)")?;
                    t.restore(&id, &snap.id)
                });
                res.map(|r| {
                    self.host.seen.lock().expect("seen lock").clear();
                    if let Some(w) = self.host.workspace.write().expect("workspace lock").as_mut() {
                        w.refresh();
                    }
                    let n = r["files"].as_array().map_or(0, Vec::len);
                    format!("Restored step {} ({n} files). The state before it is saved as the newest step, so /restore can undo this.", r["step"])
                })
            }
            "/export" => need(&self.task_id).and_then(|id| {
                let r = store.load(&id).ok_or("unknown task")?;
                let (range, path) = export_args(arg)?;
                let turns = agent::export::turns(&r);
                // No range given: on a terminal, ask where to start and where to end.
                let range = match range {
                    Some(range) => range,
                    None if turns.len() > 1 && std::io::stdin().is_terminal() => pick_turns(&turns)?,
                    None => agent::export::Range::default(),
                };
                let root = self.root().ok();
                let log = self.host.edits.lock().expect("edit log lock");
                let (path, bytes, span) = agent::export::export_task(&r, &log, root.as_deref(), path.as_deref(), range)?;
                let what = if span.is_partial() { format!("turns {}–{} of {}", span.from, span.to, span.total) } else { format!("all {} turns", span.total) };
                Ok(format!("Exported {what} to {} ({} KB).", path.display(), bytes.div_ceil(1024)))
            }),
            "/approvals" => Ok(self.approvals().await),
            "/mcp" => Ok(self.mcp_list().await),
            "/agents" => Ok(agents_board()),
            "/allow" => self.set_approval(agent::approvals::Choice::Allow, arg).await,
            "/ask" => self.set_approval(agent::approvals::Choice::Ask, arg).await,
            "/deny" => self.set_approval(agent::approvals::Choice::Deny, arg).await,
            "/new" => {
                self.task_id = None;
                Ok("Started a new conversation.".into())
            }
            "/cost" => need(&self.task_id).and_then(|id| store.load(&id).map(|r| agent::session::cost(&r)).ok_or("unknown task".into())).map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()),
            "/checkpoints" => need(&self.task_id).and_then(|id| store.load(&id).ok_or("unknown task".into())).map(|r| {
                r.checkpoints.iter().enumerate().map(|(i, c)| format!("{i}: {}", c.prompt.lines().next().unwrap_or(""))).collect::<Vec<_>>().join("\n")
            }),
            "/compact" => need(&self.task_id).and_then(|id| {
                let mut r = store.load(&id).ok_or("unknown task")?;
                let out = agent::session::compact_record(&mut r);
                store.save(&r).map_err(|e| e.to_string())?;
                Ok(format!("Compressed: about {} tokens before, {} after.", out["beforeTokens"], out["afterTokens"]))
            }),
            "/fork" => need(&self.task_id).and_then(|id| {
                let at = arg.parse::<usize>().ok();
                let f = agent::session::fork(&store, &id, at)?;
                self.task_id = Some(f.id.clone());
                Ok(format!("Forked into task {}; you now continue the fork.", f.id))
            }),
            "/rewind" => {
                let res = (|| {
                    let id = need(&self.task_id)?;
                    let at: usize = arg.parse().map_err(|_| "give the checkpoint number (see /checkpoints)")?;
                    let mut r = store.load(&id).ok_or("unknown task")?;
                    let ops = agent::session::cut(&mut r, at)?;
                    store.save(&r).map_err(|e| e.to_string())?;
                    Ok::<_, String>(ops)
                })();
                match res {
                    Ok(ops) => {
                        let ws = self.host.workspace.clone();
                        let root = ws.read().expect("workspace lock").as_ref().map(|w| w.index.root().to_path_buf()).unwrap_or_default();
                        let ctx = tools::ToolCtx {
                            root,
                            workspace: ws,
                            cancel: CancellationToken::new(),
                            seen: self.host.seen.clone(),
                            edits: self.host.edits.clone(),
                            task_id: self.task_id.clone(),
                        };
                        let undone = ops.iter().filter(|op| tools::undo(&ctx, **op).is_ok()).count();
                        Ok(format!("Rewound the conversation; undid {undone} of {} edits.", ops.len()))
                    }
                    Err(e) => Err(e),
                }
            }
            _ => return None,
        })
    }
}

fn status_code(s: Status) -> ExitCode {
    if s == Status::Completed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

pub async fn main(args: &[String]) -> ExitCode {
    let mut o = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("task-x: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if o.prompt.trim() == "-" {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
        o.prompt = s;
    }
    let mut session = match Session::open(o.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("task-x: {e}");
            return ExitCode::from(2);
        }
    };
    if o.command == "exec" {
        return match session.run(&o.prompt).await {
            Ok(rec) => {
                if o.json {
                    println!("{}", agent::summary(&rec));
                } else if o.quiet {
                    println!("{}", rec.final_text);
                }
                // A task that did not complete always says why, even with --quiet or --json.
                if !o.json && (!o.quiet || rec.status != Status::Completed) {
                    eprintln!("\n\ntask {}: {:?}{}", rec.id, rec.status, rec.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default());
                }
                status_code(rec.status)
            }
            Err(e) => {
                eprintln!("task-x: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if o.watch {
        let ws = session.host.workspace.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let w = ws.clone();
                let _ = tokio::task::spawn_blocking(move || w.write().expect("workspace lock").as_mut().map(Workspace::rescan)).await;
            }
        });
    }
    eprintln!("task-x chat ({} {}). Type /exit to leave.", serde_json::to_value(session.provider.kind).unwrap_or_default().as_str().unwrap_or(""), session.provider.model);
    let stdin = std::io::stdin();
    loop {
        eprint!("\n> ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/exit" | "/quit") {
            break;
        }
        if let Some(r) = session.command(line).await {
            match r {
                Ok(t) => eprintln!("{t}"),
                Err(e) => eprintln!("task-x: {e}"),
            }
            continue;
        }
        match session.run(line).await {
            Ok(rec) if rec.status != Status::Completed => eprintln!("\n(task {}: {:?}{})", rec.id, rec.status, rec.error.map(|e| format!(": {e}")).unwrap_or_default()),
            Ok(_) => println!(),
            Err(e) => eprintln!("task-x: {e}"),
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn keys_show_as_keys_even_when_the_stream_cuts_them() {
        assert_eq!(keys("Press <kbd>Cmd</kbd>+<KBD>P</KBD><br>then", false), "Press [Cmd]+[P]\nthen");
        assert_eq!(keys("<kbd>Cmd</kbd>", true), "\x1b[7m Cmd \x1b[0m");
        assert_eq!(keys("a < b and <b>x</b>", false), "a < b and <b>x</b>", "other text and tags are left alone");
        let mut k = InlineKeys::default();
        let out: String = ["Press <k", "bd>Cm", "d</kb", "d>+<", "kbd>P</kbd> now"].iter().map(|p| k.push(p)).collect::<String>() + &k.flush();
        assert_eq!(out.replace("\x1b[7m ", "[").replace(" \x1b[0m", "]"), "Press [Cmd]+[P] now");
        let mut k = InlineKeys::default();
        assert_eq!(k.push("x < y"), "x < y", "a lone < with text after it is not held");
        assert_eq!(k.push("open <kbd>"), "open ");
        assert_eq!(k.flush(), "<kbd>", "an unclosed tag comes out as it is in the end");
    }

    #[test]
    fn reads_export_ranges_and_paths() {
        use agent::export::Range;
        let r = |from, to| Some(Range { from, to });
        assert_eq!(export_args("").unwrap(), (None, None));
        assert_eq!(export_args("2-5").unwrap(), (r(Some(2), Some(5)), None));
        assert_eq!(export_args("3 out.html").unwrap(), (r(Some(3), Some(3)), Some("out.html".into())));
        assert_eq!(export_args("out.html 2-").unwrap(), (r(Some(2), None), Some("out.html".into())));
        assert_eq!(export_args("-4").unwrap(), (r(None, Some(4)), None));
        assert_eq!(export_args("all").unwrap(), (r(None, None), None));
        assert_eq!(export_args("2024-report.html").unwrap(), (None, Some("2024-report.html".into())));
        assert!(export_args("2-3-4").is_err());
        assert!(export_args("a.html b.html").is_err());
    }

    #[test]
    fn reads_the_command_line() {
        let o = parse_args(&args("exec --mode plan --edit auto --deny run_command --max-turns 5 --json fix the login bug")).unwrap();
        assert_eq!((o.command.as_str(), o.prompt.as_str()), ("exec", "fix the login bug"));
        assert_eq!((o.mode, o.edit, o.max_turns, o.json), (ChatMode::Plan, EditMode::Auto, Some(5), true));
        assert_eq!(o.deny, ["run_command"]);
        assert!(parse_args(&args("exec")).is_err(), "exec needs a prompt");
        assert!(parse_args(&args("chat --frobnicate")).is_err());
        assert!(parse_args(&args("exec --mode wild x")).is_err());
        assert!(parse_args(&args("serve")).is_err());
        assert_eq!(parse_args(&args("exec -- --not-an-option")).unwrap().prompt, "--not-an-option");
        assert!(parse_args(&args("chat")).unwrap().prompt.is_empty());
    }

    #[test]
    fn picks_the_provider_from_options_and_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned());
        let o = parse_args(&args("chat")).unwrap();
        let a = provider(&o, &env(&[("ANTHROPIC_API_KEY", "k")])).unwrap();
        assert_eq!((a.kind, a.model.as_str()), (ProviderKind::Anthropic, "claude-sonnet-5-5"));
        let l = provider(&o, &env(&[])).unwrap();
        assert_eq!(l.kind, ProviderKind::Ollama, "no key: a local model");
        let missing = parse_args(&args("chat --provider openai")).unwrap();
        assert!(provider(&missing, &env(&[])).unwrap_err().contains("OPENAI_API_KEY"));
        let custom = parse_args(&args("chat --provider openaiCompatible --model m --base-url http://127.0.0.1:9/v1 --api-key-env MY_KEY")).unwrap();
        let c = provider(&custom, &env(&[("MY_KEY", "secret")])).unwrap();
        assert_eq!((c.api_key.as_deref(), c.model.as_str()), (Some("secret"), "m"));
        assert!(provider(&parse_args(&args("chat --provider nope")).unwrap(), &env(&[])).is_err());
    }

    #[test]
    fn renders_events_for_the_terminal() {
        assert_eq!(render(&json!({ "type": "textDelta", "text": "hi" }), false), Some((false, "hi".into())));
        let tool = render(&json!({ "type": "toolStarted", "tool": "read_file", "input": { "path": "src/a.rs" } }), false).unwrap();
        assert!(tool.0 && tool.1.contains("read_file src/a.rs"));
        assert!(render(&json!({ "type": "toolFinished", "tool": "x", "ok": true }), false).is_none(), "a success is quiet");
        assert!(render(&json!({ "type": "toolFinished", "tool": "x", "ok": false, "output": "boom" }), false).unwrap().1.contains("boom"));
        assert!(render(&json!({ "type": "textDelta", "text": "hi" }), true).is_none(), "quiet prints only the answer");
    }

    #[test]
    fn renders_sleeping_and_wakeups() {
        let due = agent::store::now_ms() + 1_800_000;
        let sleeping = json!({ "type": "sleeping", "untilMs": due, "pending": [{ "id": "w1", "reason": "check the deploy" }] });
        let line = render(&sleeping, false).unwrap().1;
        assert!(line.contains("☾ Sleeping · next wake-up in 30m: check the deploy (1 pending)"), "{line}");
        // The animated line: the moon moves one phase a second, and the countdown ticks down.
        let tail = sleeping_tail(&sleeping);
        assert_eq!(sleep_frame(0, Some(1_801_000), 1_000, &tail), "🌑 Sleeping · next wake-up in 30m 00s: check the deploy (1 pending)");
        assert_eq!(sleep_frame(1, Some(1_801_000), 3_000, &tail), "🌒 Sleeping · next wake-up in 29m 58s: check the deploy (1 pending)");
        assert!(sleep_frame(9, Some(10), 20, "").starts_with("🌒 Sleeping · waking up"), "the phases repeat; a due wake-up says so");
        assert_eq!([7_000, 65_000, 3_903_000].map(countdown), ["7s", "1m 05s", "1h 05m 03s"]);
        let scheduled = json!({ "type": "wakeupScheduled", "wakeup": { "id": "w2", "kind": "watch", "reason": "server up", "dueMs": due } });
        assert!(render(&scheduled, false).unwrap().1.contains("⏰ check w2 in 30m: server up"));
        assert!(render(&json!({ "type": "wakeupFired", "id": "w2", "reason": "server up", "met": true }), false).unwrap().1.contains("w2 fired (passed)"));
        assert!(render(&json!({ "type": "awake", "by": "message" }), false).unwrap().1.contains("⏰ Awake: a message arrived"));
        assert!(render(&json!({ "type": "wakeupCancelled", "id": "w1", "by": "taskEnded" }), false).is_none(), "the task's end is not news");
    }
}
