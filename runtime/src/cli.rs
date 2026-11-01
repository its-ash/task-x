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

In chat: /exit, /new, /cost, /compact, /checkpoints, /rewind <n>, /fork [n], and every skill and command (/init, /review, ...).";

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
    Ok(cfg)
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
    for q in &req.questions {
        eprintln!("\n? {}", q.text);
        for (i, opt) in q.options.iter().enumerate() {
            eprintln!("  {}) {opt}", i + 1);
        }
        eprint!("answer (numbers, or your own text): ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let picks: Vec<String> = line.split([',', ' ']).filter_map(|s| s.trim().parse::<usize>().ok()).filter_map(|n| q.options.get(n.wrapping_sub(1)).cloned()).collect();
        if picks.is_empty() && !line.trim().is_empty() {
            a.other.insert(q.id.clone(), line.trim().to_owned());
        }
        a.selected.insert(q.id.clone(), picks);
    }
    Some(a)
}

struct Session {
    host: Host,
    options: Options,
    provider: ProviderConfig,
    task_id: Option<String>,
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
        // The user runs the agent in this folder, so its hooks and MCP servers count as trusted.
        let mut hook_list = agent::hooks::load_workspace(&root).unwrap_or_default();
        if let Some(h) = home() {
            hook_list.extend(agent::hooks::load_workspace(&h).unwrap_or_default().into_iter().map(|h| agent::hooks::Hook { trusted: true, ..h }));
        }
        let mcp = Arc::new(tokio::sync::RwLock::new(crate::mcp::Manager::default()));
        let servers = crate::mcp::load_workspace(&root).unwrap_or_default();
        if !servers.is_empty() {
            mcp.write().await.configure(servers, Some(root.clone())).await;
        }
        let emit_quiet = quiet;
        let yes = o.yes;
        let host = Host {
            providers: Providers::new().map_err(|e| e.message)?,
            workspace: Arc::new(RwLock::new(Some(ws))),
            seen: Arc::default(),
            edits,
            grants: Arc::default(),
            store,
            emit: Arc::new(move |_, ev| {
                if let Some((err, text)) = render(&ev, emit_quiet) {
                    if err {
                        eprint!("{text}");
                    } else {
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
            plugins: Arc::new(RwLock::new(agent::plugins::Settings { workspace_trusted: true, ..Default::default() })),
        };
        let mut task_id = o.resume.clone();
        if o.continue_last && task_id.is_none() {
            task_id = host.store.list(1).first().and_then(|t| t["id"].as_str().map(str::to_owned));
        }
        Ok(Self { host, options: o, provider, task_id })
    }

    async fn run(&mut self, prompt: &str) -> Result<TaskRecord, String> {
        let o = &self.options;
        let p = RunParams {
            prompt: prompt.to_owned(),
            images: Vec::new(),
            provider: self.provider.clone(),
            chat_mode: o.mode,
            policy: PolicyConfig {
                mode: if o.read_only { Mode::ReadOnly } else { Mode::Autonomous },
                allow: o.allow.clone(),
                deny: o.deny.clone(),
                allow_all: false,
                edit_mode: (!o.read_only).then_some(o.edit),
                sandboxed: None,
            },
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
        let rec = agent::run(self.host.clone(), p, CancellationToken::new()).await?;
        self.task_id = Some(rec.id.clone());
        Ok(rec)
    }

    /// The session commands of `chat`; `None` when the line is a prompt for the agent.
    async fn command(&mut self, line: &str) -> Option<Result<String, String>> {
        let (cmd, arg) = line.split_once(' ').map_or((line, ""), |(c, a)| (c, a.trim()));
        let store = self.host.store.clone();
        let need = |id: &Option<String>| id.clone().ok_or_else(|| "no task yet in this session".to_owned());
        Some(match cmd {
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
}
