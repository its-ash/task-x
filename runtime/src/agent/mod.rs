//! The agent loop: enriched request → model → tool calls (policy-checked, approved, executed) →
//! results back to the model → … until the model finishes or a limit stops it.

pub mod compact;
pub mod cost;
pub mod hooks;
pub mod policy;
pub mod prompt;
pub mod skills;
pub mod store;
pub mod team;

use crate::context::Workspace;
use crate::errors;
use crate::mcp;
use crate::provider::{
    ChatRequest, ContentPart, ErrorKind, Message, ProviderConfig, ProviderError, Providers, Role, StreamEvent, ToolCall, Usage,
    repair_history,
};
use crate::tools::{self, EditLog, Permission, ToolCtx, ToolErrorKind};
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, join_all};
use futures_util::stream::FuturesUnordered;
use policy::{Decision, Mode, Policy, PolicyConfig};
use prompt::{EditorContext, Enriched, Inputs};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use store::{Status, Store, TaskRecord, ValidationRun};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

const MAX_HISTORY_RESULT_CHARS: usize = 16_000;
/// Tool output sent to the client for display (the model gets `MAX_HISTORY_RESULT_CHARS`).
const MAX_EVENT_OUTPUT_CHARS: usize = 120_000;
const PROVIDER_RETRIES: u32 = 3;
const MAX_IDENTICAL_FAILURES: u32 = 2;
const STUCK_ROUNDS: u32 = 3;
const VALIDATION_TOOLS: &[&str] = &["run_tests", "check_build", "run_linter", "format_code", "get_diagnostics"];
/// A command still running after this long stops holding the loop: it continues in the background
/// and its output arrives as a message. Long typechecks, builds, and installs would otherwise block
/// every other tool call for minutes while the chat shows only "Running…".
const COMMAND_HANDOFF: Duration = Duration::from_secs(30);
/// How often a reply with no text and no tool call is answered with `EMPTY_ANSWER_NUDGE`.
const MAX_EMPTY_ANSWERS: u32 = 2;
const EMPTY_ANSWER_NUDGE: &str = "Your last reply had no text. Write your answer to the request now, as plain text in your reply (not only in your reasoning). If you need more information, call a tool.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatMode {
    /// Answer questions with read-only tools.
    Ask,
    /// Investigate with read-only tools and produce an implementation plan.
    Plan,
    /// Make changes: edit files and run commands, as the permission policy allows.
    #[default]
    Agent,
    /// Lead a team: plan the work as a dependency graph and run subagents in parallel.
    Team,
}

impl ChatMode {
    fn guidance(self) -> &'static str {
        match self {
            Self::Ask => {
                "Mode: ASK. Answer the user's question. Use read-only tools to look at the code and cite files with line numbers. You cannot edit files in this mode; if changes are needed, describe them and suggest switching to Agent mode."
            }
            Self::Plan => {
                "Mode: PLAN. Investigate with read-only tools, then write an implementation plan: goal, files to change (paths), ordered steps with the specific edits, risks, and how to validate. Do not edit files; the user will run the plan in Agent mode."
            }
            Self::Agent => {
                "Mode: AGENT. Carry out the request: inspect the code, make the edits with the edit tools, then validate with the project's commands. Keep changes minimal and focused."
            }
            Self::Team => {
                "Mode: TEAM. You lead subagents that work at the same time, each writer in its own git worktree. Look at the code briefly with read-only tools, then call run_plan once with the steps. Steps that do not depend on each other run in parallel; a step lists in dependsOn the steps whose results it needs. Give each step a precise objective and the facts it needs. run_plan merges the finished work into the working tree one step at a time. Then check the results, resolve merge conflicts yourself (the conflicting worktree is kept: fix the files, then worktree_discard it), run the project's checks, and summarize. For a small task, do the work yourself without run_plan."
            }
        }
    }

    fn writes(self) -> bool {
        matches!(self, Self::Agent | Self::Team)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "d_iter")]
    pub max_iterations: u32,
    /// The only budget: the task stops when its tokens reach it. Unset: no token budget. Tool calls are not limited.
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    #[serde(default = "d_deadline")]
    pub deadline_secs: u64,
    #[serde(default = "d_ctx")]
    pub context_budget_tokens: usize,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// The model's context window. Unset: the provider's known window for the model.
    #[serde(default)]
    pub context_window_tokens: Option<usize>,
    /// Compression starts when the history passes this share of the window (30..=95).
    #[serde(default = "d_compact_percent")]
    pub compact_percent: u32,
    /// Subagents that run at the same time in Team mode (1..=8).
    #[serde(default = "d_parallel")]
    pub max_parallel: u32,
    /// Offer a core set of tools and load the others on first use (`find_tool`, or calling one by
    /// name). The full list costs tokens on every turn.
    #[serde(default)]
    pub lazy_tools: bool,
    /// Reasoning effort for every model call of the task. Unset: each model's default.
    #[serde(default)]
    pub effort: Option<crate::provider::types::Effort>,
}
fn d_parallel() -> u32 {
    3
}
fn d_compact_percent() -> u32 {
    70
}
const MIN_WINDOW: usize = 4_000;
fn d_iter() -> u32 {
    60
}
fn d_deadline() -> u64 {
    1_800
}
fn d_ctx() -> usize {
    8_000
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_iterations: d_iter(),
            max_total_tokens: None,
            deadline_secs: d_deadline(),
            context_budget_tokens: d_ctx(),
            max_output_tokens: None,
            context_window_tokens: None,
            compact_percent: d_compact_percent(),
            max_parallel: d_parallel(),
            lazy_tools: false,
            effort: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunParams {
    pub prompt: String,
    /// Images the user attached to the prompt (pasted or dropped into the chat).
    #[serde(default)]
    pub images: Vec<crate::provider::types::ImageInput>,
    pub provider: ProviderConfig,
    #[serde(default)]
    pub chat_mode: ChatMode,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub editor: EditorContext,
    #[serde(default)]
    pub limits: Limits,
    /// Continue an earlier task (conversation) with this new prompt.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Use a different provider for some task kinds (e.g. a small local model for questions).
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Providers to try, in order, when the active one keeps failing with retryable errors.
    /// Only providers the user configured are used; every switch is reported.
    #[serde(default)]
    pub fallbacks: Vec<ProviderConfig>,
    /// Price rules for cost estimates; they take precedence over the built-in table.
    #[serde(default)]
    pub prices: Vec<cost::PriceRule>,
    /// A provider for subagents of one role (for example a local model for research).
    #[serde(default)]
    pub role_providers: Vec<RoleProvider>,
    /// A small, cheap model for `summarize_file`. Unset: the task's own model.
    #[serde(default)]
    pub helper: Option<ProviderConfig>,
    /// An embedding model for `semantic_search` and docs search (Ollama or OpenAI-compatible).
    #[serde(default)]
    pub embeddings: Option<ProviderConfig>,
    /// Nesting level; subagents run at depth 1 and cannot start more subagents.
    #[serde(default, skip_deserializing)]
    pub depth: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleProvider {
    pub role: team::Role,
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Route {
    pub kinds: Vec<prompt::TaskKind>,
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub model_calls: u32,
    pub model_ms: u64,
    pub tool_ms: u64,
    pub retries: u32,
    pub fallbacks: u32,
    pub compactions: u32,
    /// Tokens served by local models (Ollama or loopback endpoints); not billed.
    pub local_tokens: u64,
    pub billed_tokens: u64,
    /// Estimated cost in USD, subagents included.
    #[serde(default)]
    pub cost_usd: f64,
    /// Billed tokens of models with no known price (not in `cost_usd`).
    #[serde(default)]
    pub unpriced_tokens: u64,
    #[serde(default)]
    pub subagents: u32,
    #[serde(default)]
    pub subagent_cost_usd: f64,
    #[serde(default)]
    pub subagent_tokens: u64,
}

/// Usage of finished subagents, added to the lead's metrics when the task ends.
#[derive(Default)]
struct SubTotals {
    count: u32,
    cost_usd: f64,
    tokens: u64,
    unpriced: u64,
}

pub fn is_local(p: &ProviderConfig) -> bool {
    p.kind == crate::provider::ProviderKind::Ollama
        || p.resolve().is_ok_and(|r| matches!(r.base_url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1")))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub task_id: String,
    pub call_id: String,
    pub tool: String,
    pub permission: Permission,
    pub input: Value,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Allow,
    AllowSession,
    Deny,
}

pub type ApproveFn = Arc<dyn Fn(ApprovalRequest, CancellationToken) -> BoxFuture<'static, ApprovalDecision> + Send + Sync>;

const MAX_QUESTIONS: usize = 4;
const MAX_OPTIONS: usize = 8;
const MAX_QUESTION_CHARS: usize = 300;
const MAX_OPTION_CHARS: usize = 120;
/// Longest free-text answer kept per question.
pub const MAX_OTHER_CHARS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuestionKind {
    /// Exactly one option.
    Radio,
    /// Any number of options.
    Checkbox,
}

/// A question for the user, with the options they can pick from.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub text: String,
    pub kind: QuestionKind,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskRequest {
    pub task_id: String,
    pub call_id: String,
    pub questions: Vec<Question>,
}

/// The user's answers: chosen option texts and typed text, both by question id.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Answers {
    #[serde(rename = "answers")]
    pub selected: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub other: HashMap<String, String>,
}

impl Answers {
    /// The offered options picked for `q` (one for radio questions).
    pub fn picked(&self, q: &Question) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for a in self.selected.get(&q.id).into_iter().flatten() {
            if q.options.contains(a) && !out.contains(a) {
                out.push(a.clone());
            }
        }
        if q.kind == QuestionKind::Radio {
            out.truncate(1);
        }
        out
    }

    /// The trimmed text typed for `q`, cut to `MAX_OTHER_CHARS`, if any.
    pub fn other(&self, q: &Question) -> Option<String> {
        let t = self.other.get(&q.id)?.trim();
        (!t.is_empty()).then(|| t.chars().take(MAX_OTHER_CHARS).collect())
    }
}

/// The answers, or `None` when the user dismissed the questions.
pub type AskFn = Arc<dyn Fn(AskRequest, CancellationToken) -> BoxFuture<'static, Option<Answers>> + Send + Sync>;
pub type EmitFn = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Messages waiting for each running top-level task, by task id (`agent/steer`).
pub type Steers = Arc<Mutex<HashMap<String, Vec<String>>>>;
/// Steering messages waiting for one task, and their size.
const MAX_STEERS: usize = 10;
const MAX_STEER_CHARS: usize = 100_000;

#[derive(Clone)]
pub struct Host {
    pub providers: Providers,
    pub workspace: Arc<RwLock<Option<Workspace>>>,
    pub seen: Arc<Mutex<HashMap<String, u64>>>,
    pub edits: Arc<Mutex<EditLog>>,
    pub grants: Arc<Mutex<HashSet<String>>>,
    pub store: Store,
    pub emit: EmitFn,
    pub approve: ApproveFn,
    pub ask: AskFn,
    pub mcp: Arc<tokio::sync::RwLock<mcp::Manager>>,
    pub hooks: Arc<RwLock<Vec<hooks::Hook>>>,
    /// Running subagents by task id, so the client can stop one (`agent/cancelSubagent`).
    pub subagents: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// Messages sent to a running task, taken into its conversation at the next step.
    pub steers: Steers,
    /// Running tool calls by `taskId:callId`, so the user can stop one and the task goes on (`agent/cancelTool`).
    pub tool_cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more characters truncated]", &s[..cut], s.len() - cut)
}

fn mode_note(chat: ChatMode, p: &PolicyConfig) -> String {
    match (chat, p.mode) {
        (ChatMode::Team, Mode::AskBeforeEdit) => {
            "subagents edit their own worktrees; the user approves the plan, edits in the working tree, and commands".into()
        }
        (ChatMode::Ask | ChatMode::Plan, _) | (_, Mode::ReadOnly) => "read-only: file edits and commands are not available".into(),
        (_, Mode::AskBeforeEdit) => "the user approves each edit and command".into(),
        (_, Mode::AskBeforeExecution) => "edits apply directly; the user approves commands and deletions".into(),
        (_, Mode::Autonomous) => "edits and project commands run without asking; deletions need approval".into(),
    }
}

/// Short human-readable preview shown in approval prompts.
fn preview(name: &str, input: &Value) -> Option<String> {
    match name {
        "replace_text" | "replace_all" => Some(format!(
            "{}:\n- {}\n+ {}",
            input["path"].as_str()?,
            truncate(input["oldText"].as_str()?, 2_000),
            truncate(input["newText"].as_str()?, 2_000)
        )),
        // A batch of edits: which files, and how many changes.
        "edit_files" => {
            let edits = input["edits"].as_array()?;
            let mut files: Vec<&str> = edits.iter().filter_map(|e| e["path"].as_str()).collect();
            files.sort_unstable();
            files.dedup();
            Some(format!("{} change(s) in {} file(s):\n{}", edits.len(), files.len(), files.join("\n")))
        }
        "insert_at_line" => Some(format!(
            "{}: insert before line {}\n+ {}",
            input["path"].as_str()?,
            input["line"].as_u64().unwrap_or(1),
            truncate(input["text"].as_str()?, 2_000)
        )),
        "delete_range" => Some(format!(
            "{}: delete lines {}–{}",
            input["path"].as_str()?,
            input["start"].as_u64().unwrap_or(1),
            input["end"].as_u64().unwrap_or(1)
        )),
        "apply_patch" => input["patch"].as_str().map(|p| truncate(p, 6_000)),
        "create_file" => Some(format!("create {} ({} bytes)", input["path"].as_str()?, input["content"].as_str()?.len())),
        "delete_file" => Some(format!("delete {}", input["path"].as_str()?)),
        "run_command" => {
            // The command may arrive as an array or as a plain line; show either as one string.
            Some(format!("$ {}\n(in {})", command_text(&input["command"]), input["cwd"].as_str().unwrap_or(".")))
        }
        "web_fetch" => Some(format!("GET {}", input["url"].as_str()?)),
        "web_search" => Some(format!("Search the web for: {}", input["query"].as_str()?)),
        "copy_file" | "move_file" => {
            Some(format!("{} {} → {}", if name == "move_file" { "move" } else { "copy" }, input["from"].as_str()?, input["to"].as_str()?))
        }
        "rename_file" => Some(format!("rename {} → {}", input["from"].as_str()?, input["to"].as_str()?)),
        _ => None,
    }
}

/// A `run_command` command as one line, whether it arrived as an array or as text.
fn command_text(command: &Value) -> String {
    match command {
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        Value::String(s) => s.trim().to_owned(),
        _ => String::new(),
    }
}

/// One-letter type names keep the guide short; the model only needs the shape.
fn short_type(schema: &Value) -> &'static str {
    match schema["type"].as_str() {
        Some("string") => "s",
        Some("integer" | "number") => "n",
        Some("boolean") => "b",
        Some("array") => "[]",
        Some("object") => "{}",
        _ => "?",
    }
}

/// A tool list as `name(required:s, optional?:n)`, the shortest form that still shows every name
/// and its parameters. Provider schemas are the authority, but several of them drop or ignore
/// `tools`, and models then invent argument names; a few lines of plain text in the system prompt
/// cost almost nothing and prevent most of those mistakes.
fn tool_guide(specs: &[crate::provider::ToolSpec]) -> String {
    let mut lines = Vec::with_capacity(specs.len());
    for t in specs {
        let required: Vec<&str> = t.input_schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let props = t.input_schema["properties"].as_object();
        let mut args: Vec<String> = Vec::new();
        if let Some(props) = props {
            // Required fields first, then optional ones; alphabetical within each group, so the
            // guide is stable and `path` (required) is never hidden behind optional fields.
            let mut ordered: Vec<(&String, &Value)> = props.iter().collect();
            ordered.sort_by_key(|(name, _)| (!required.contains(&name.as_str()), name.as_str()));
            for (name, schema) in ordered {
                let ty = short_type(schema);
                args.push(if required.contains(&name.as_str()) { format!("{name}:{ty}") } else { format!("{name}?:{ty}") });
            }
        }
        lines.push(if args.is_empty() { t.name.clone() } else { format!("{}({})", t.name, args.join(", ")) });
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "<tools>\nCall a tool by the exact name below. Arguments are one JSON object; `?` marks an optional field. \
s = string, n = number, b = boolean, [] = array, {{}} = object.\n{}\n</tools>",
        lines.join("\n")
    )
}

struct Run {
    host: Host,
    rec: TaskRecord,
    policy: Policy,
    cancel: CancellationToken,
    limits: Limits,
    provider: ProviderConfig,
    tool_specs: Vec<crate::provider::ToolSpec>,
    failures: HashMap<String, u32>,
    started: Instant,
    mcp: HashMap<String, (Arc<mcp::Client>, mcp::McpTool)>,
    hooks: Vec<hooks::Hook>,
    cwd: std::path::PathBuf,
    fallbacks: Vec<ProviderConfig>,
    skills: HashMap<String, String>,
    depth: u32,
    policy_cfg: PolicyConfig,
    /// Subagent worktrees created by this task → whether their patch was applied.
    worktrees: Mutex<HashMap<String, bool>>,
    sub_count: std::sync::atomic::AtomicU32,
    sub_totals: Mutex<SubTotals>,
    /// Real prompt tokens ÷ character estimate, from the last response (calibrates estimates).
    token_ratio: f64,
    prices: Vec<cost::PriceRule>,
    role_providers: Vec<RoleProvider>,
    background: Arc<Background>,
    /// Read calls by name and input → (fingerprint of the result, id of the call that returned it).
    reads: HashMap<String, (u64, String)>,
    /// Tools not offered yet (lazy tool list); calling one by name, or `find_tool`, loads it.
    deferred: Vec<crate::provider::ToolSpec>,
    helper: Option<ProviderConfig>,
    embeddings: Option<ProviderConfig>,
}

/// Tools offered from the start when the tool list is lazy: what nearly every coding task uses.
const CORE_TOOLS: &[&str] = &[
    "read_file",
    "read_range",
    "read_symbol",
    "get_file_outline",
    "search_text",
    "search_regex",
    "multi_search",
    "find_files",
    "find_symbol",
    "find_references",
    "list_files",
    "replace_text",
    "edit_files",
    "create_file",
    "apply_patch",
    "run_command",
    "run_tests",
    "check_build",
    "git_diff",
    "read_log",
    "remember",
    "todo_write",
];
const FIND_TOOL: &str = "find_tool";
const SUMMARIZE: &str = "summarize_file";
const SEMANTIC: &str = "semantic_search";
const DOCS_SEARCH: &str = "docs_search";
/// One embedding update of a workspace at a time; the store is shared by every task.
static SEMANTIC_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
/// Characters of one search hit given to the model.
const MAX_HIT_CHARS: usize = 1_500;

fn semantic_spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: SEMANTIC.into(),
        description: "Search the project by meaning, not exact words: \"where is the retry logic\", \"code that parses CLI flags\". Returns the closest code chunks with file and lines. Use search_regex for exact names.".into(),
        input_schema: json!({ "type": "object", "properties": {
            "query": { "type": "string", "description": "What the code does, in plain words" },
            "limit": { "type": "integer", "minimum": 1, "maximum": 20 },
            "glob": { "type": "string", "description": "Only paths that match, e.g. src/**/*.rs" }
        }, "required": ["query"], "additionalProperties": false }),
    }
}

fn docs_spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: DOCS_SEARCH.into(),
        description: "Search library documentation the user indexed (@docs). Returns the best passages with their page URL. Give source to search one docs set.".into(),
        input_schema: json!({ "type": "object", "properties": {
            "query": { "type": "string" },
            "source": { "type": "string", "description": "The docs name, as in @docs:<name>" },
            "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
        }, "required": ["query"], "additionalProperties": false }),
    }
}
/// Read calls whose repeat, with the same result still in the conversation, is answered with a
/// short note instead of the same content again.
const REREAD_TOOLS: &[&str] = &["read_file", "read_range", "read_symbol", "get_file_outline", "read_multiple_files", "read_log"];
/// File text sent to the helper model for one summary.
const MAX_SUMMARY_SOURCE_CHARS: usize = 120_000;
const SUMMARY_TOKENS: u32 = 900;

fn find_tool_spec(deferred: &[crate::provider::ToolSpec]) -> crate::provider::ToolSpec {
    let names: Vec<&str> = deferred.iter().map(|t| t.name.as_str()).collect();
    crate::provider::ToolSpec {
        name: FIND_TOOL.into(),
        description: format!(
            "Load more tools. Give names to load them, or a query to find them by name and description. Loaded tools can be called from the next step. Not loaded yet: {}.",
            names.join(", ")
        ),
        input_schema: json!({ "type": "object", "properties": {
            "names": { "type": "array", "items": { "type": "string" } },
            "query": { "type": "string" }
        }, "additionalProperties": false }),
    }
}

fn summarize_spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: SUMMARIZE.into(),
        description: "Get a short summary of a large file from a helper model instead of reading it all: its purpose, its main parts with line numbers, and the answer to question. Use it for files you need to understand, not for code you will edit (read that range).".into(),
        input_schema: json!({ "type": "object", "properties": {
            "path": { "type": "string" },
            "question": { "type": "string", "description": "What you need to know from the file" }
        }, "required": ["path"], "additionalProperties": false }),
    }
}

/// The deferred tools that match: exact names first, else every word of the query in the name or description.
fn match_tools(deferred: &[crate::provider::ToolSpec], input: &Value) -> Vec<String> {
    let names: Vec<&str> = input["names"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let words: Vec<String> = input["query"].as_str().unwrap_or_default().to_lowercase().split_whitespace().map(str::to_owned).collect();
    deferred
        .iter()
        .filter(|t| {
            names.contains(&t.name.as_str()) || {
                let hay = format!("{} {}", t.name, t.description).to_lowercase();
                !words.is_empty() && words.iter().all(|w| hay.contains(w.as_str()))
            }
        })
        .take(10)
        .map(|t| t.name.clone())
        .collect()
}

/// `run_command` calls with `background: true`: how many still run, and the reports of the finished ones.
#[derive(Default)]
struct Background {
    running: std::sync::atomic::AtomicUsize,
    done: Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
}

/// Output of a finished background command given to the model.
const MAX_BACKGROUND_REPORT_CHARS: usize = 12_000;

const READ_SKILL: &str = "read_skill";
const ASK_USER: &str = "ask_user";
const SUBAGENT_ITERATIONS: u32 = 15;
const SUBAGENT_DEADLINE_SECS: u64 = 900;
const MAX_PATCH_CHARS: usize = 20_000;
const MAX_PLAN_STEPS: usize = 12;
const MAX_PARALLEL: u32 = 8;
const MAX_STEP_ID: usize = 24;
/// Report text of a dependency passed to the steps that need it.
const MAX_DEP_REPORT_CHARS: usize = 4_000;
/// A candidate's patch shown to the client in best-of-N.
const MAX_CANDIDATE_PATCH_CHARS: usize = 60_000;

enum Target {
    Builtin(&'static tools::ToolDef),
    Mcp(Arc<mcp::Client>, mcp::McpTool),
    Skill,
    Subagent(team::Role),
    Worktree(&'static str),
    /// `run_plan`; true when a step writes.
    Plan(bool),
    /// `ask_user`: the questions to put to the user.
    Ask(Vec<Question>),
    /// `find_tool`: loads deferred tools (in `record`, which may change the tool list).
    FindTool,
    /// `summarize_file`: a helper model reads the file and returns a summary.
    Summarize,
    /// `semantic_search` over the project, and `docs_search` over indexed docs.
    Semantic,
    Docs,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanStep {
    id: String,
    role: team::Role,
    objective: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanInput {
    steps: Vec<PlanStep>,
    #[serde(default = "yes")]
    merge: bool,
}
fn yes() -> bool {
    true
}

/// Checks a plan (ids, dependencies, no cycles) and returns step indexes in dependency order.
fn plan_order(steps: &[PlanStep]) -> Result<Vec<usize>, String> {
    if steps.is_empty() || steps.len() > MAX_PLAN_STEPS {
        return Err(format!("a plan needs 1 to {MAX_PLAN_STEPS} steps"));
    }
    let mut seen = HashSet::new();
    for s in steps {
        if s.id.len() > MAX_STEP_ID || !team::valid_name(&s.id) {
            return Err(format!("step id '{}' must be 1 to {MAX_STEP_ID} letters, digits, - or _", s.id));
        }
        if !seen.insert(s.id.as_str()) {
            return Err(format!("step id '{}' is used twice", s.id));
        }
        if s.objective.trim().is_empty() {
            return Err(format!("step '{}' has no objective", s.id));
        }
    }
    for s in steps {
        if let Some(d) = s.depends_on.iter().find(|d| !seen.contains(d.as_str()) || **d == s.id) {
            return Err(format!("step '{}' depends on unknown step '{d}'", s.id));
        }
    }
    let mut order: Vec<usize> = Vec::new();
    while order.len() < steps.len() {
        let next =
            (0..steps.len()).find(|i| !order.contains(i) && steps[*i].depends_on.iter().all(|d| order.iter().any(|o| steps[*o].id == *d)));
        match next {
            Some(i) => order.push(i),
            None => return Err("the steps have a dependency cycle".into()),
        }
    }
    Ok(order)
}

/// What one round of tool calls amounted to.
struct Round {
    parts: Vec<ContentPart>,
    any_ok: bool,
    /// Some failure was the model's to fix (bad input, wrong text, stale file), so the round counts
    /// towards the stuck check. A round of pure policy denials does not: retrying cannot help.
    any_fixable: bool,
}

/// One finished (or skipped) plan step.
struct StepResult {
    ok: bool,
    out: Value,
    patch: Option<String>,
    worktree: Option<String>,
}

fn team_specs() -> Vec<crate::provider::ToolSpec> {
    let spec = |name: &str, description: &str, schema: Value| crate::provider::ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    };
    vec![
        spec(
            "run_subagent",
            "Delegate one bounded, independent piece of work to a subagent. Use only for substantial work that can run on its own (several can run in parallel); do the work yourself for small tasks. role is one of research, implementation, testing, review, documentation (writing roles get an isolated git worktree and return a patch; apply it with worktree_apply after reviewing it).",
            json!({ "type": "object", "properties": {
                "role": { "type": "string", "enum": ["research", "implementation", "testing", "review", "documentation"], "description": "research and review read only; implementation, testing, and documentation edit in a worktree" },
                "objective": { "type": "string", "description": "What to achieve and how to know it is done" },
                "context": { "type": "string", "description": "Relevant facts, files, and constraints" },
                "name": { "type": "string", "description": "Worktree name (letters, digits, - and _)" }
            }, "required": ["role", "objective"], "additionalProperties": false }),
        ),
        spec(
            "worktree_apply",
            "Apply a subagent worktree's patch to the main working tree (refuses on conflicts).",
            json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"], "additionalProperties": false }),
        ),
        spec(
            "worktree_discard",
            "Delete a subagent worktree and its branch without applying it.",
            json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"], "additionalProperties": false }),
        ),
    ]
}

/// Validates `ask_user` input: 1 to 4 questions, each with 2 to 8 distinct options.
fn parse_questions(input: &Value) -> Result<Vec<Question>, String> {
    let raw = input["questions"]
        .as_array()
        .filter(|a| (1..=MAX_QUESTIONS).contains(&a.len()))
        .ok_or("questions must be a list of 1 to 4 questions")?;
    raw.iter()
        .enumerate()
        .map(|(i, q)| {
            let text = q["question"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.chars().count() <= MAX_QUESTION_CHARS);
            let text = text.ok_or(format!("questions[{i}].question must be 1 to {MAX_QUESTION_CHARS} characters"))?;
            let kind = serde_json::from_value::<QuestionKind>(q["type"].clone())
                .map_err(|_| format!("questions[{i}].type must be radio or checkbox"))?;
            let options = q["options"]
                .as_array()
                .and_then(|o| {
                    o.iter()
                        .map(|x| {
                            x.as_str().map(str::trim).filter(|s| !s.is_empty() && s.chars().count() <= MAX_OPTION_CHARS).map(String::from)
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or(format!("questions[{i}].options must be non-empty text of up to {MAX_OPTION_CHARS} characters"))?;
            let distinct: HashSet<&String> = options.iter().collect();
            if !(2..=MAX_OPTIONS).contains(&options.len()) || distinct.len() != options.len() {
                return Err(format!("questions[{i}] needs 2 to {MAX_OPTIONS} different options"));
            }
            Ok(Question { id: format!("q{}", i + 1), text: text.into(), kind, options })
        })
        .collect()
}

/// The tool result: the chosen options per question, with only the offered options kept, and any typed text.
fn answer_json(qs: &[Question], answers: Option<Answers>) -> Value {
    let Some(answers) = answers else {
        return json!({ "answered": false, "message": "The user dismissed the questions. Continue without them, choose a reasonable default and say so, or ask again in a different way." });
    };
    let list: Vec<Value> = qs
        .iter()
        .map(|q| {
            let mut a = json!({ "question": q.text, "selected": answers.picked(q) });
            if let Some(t) = answers.other(q) {
                a["other"] = json!(t);
            }
            a
        })
        .collect();
    json!({ "answered": true, "answers": list })
}

fn ask_spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: ASK_USER.into(),
        description: "Ask the user one to four questions and wait for the answers. Use it only when a choice blocks progress and only the user can make it. Use radio when exactly one option applies, checkbox when several can. Each question has 2 to 8 options. The user can also type their own answer. Returns the chosen option texts per question, and `other` when the user typed an answer.".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "questions": { "type": "array", "minItems": 1, "maxItems": MAX_QUESTIONS, "items": {
                "type": "object",
                "properties": {
                    "question": { "type": "string" },
                    "type": { "type": "string", "enum": ["radio", "checkbox"] },
                    "options": { "type": "array", "minItems": 2, "maxItems": MAX_OPTIONS, "items": { "type": "string" } }
                },
                "required": ["question", "type", "options"],
                "additionalProperties": false
            } } },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

fn plan_spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "run_plan".into(),
        description: "Run a plan as a team of subagents. Steps whose dependencies are done run at the same time (up to the parallel limit). A step gets the reports of the steps it depends on, and writers start from their work. When all steps finish, the work of writing steps is merged into the working tree in dependency order; a step that conflicts keeps its worktree. Returns each step's status, report, and merge result.".into(),
        input_schema: json!({ "type": "object", "properties": {
            "steps": { "type": "array", "minItems": 1, "maxItems": MAX_PLAN_STEPS, "items": { "type": "object", "properties": {
                "id": { "type": "string", "description": "Short id: letters, digits, - and _" },
                "role": { "type": "string", "enum": ["research", "implementation", "testing", "review", "documentation"] },
                "objective": { "type": "string", "description": "What to achieve and how to know it is done" },
                "context": { "type": "string", "description": "Relevant facts, files, and constraints" },
                "dependsOn": { "type": "array", "items": { "type": "string" }, "description": "Ids of steps that must finish first" }
            }, "required": ["id", "role", "objective"], "additionalProperties": false } },
            "merge": { "type": "boolean", "description": "Merge writers' work into the working tree (default true)" }
        }, "required": ["steps"], "additionalProperties": false }),
    }
}

impl Target {
    fn permission(&self) -> Permission {
        match self {
            Self::Builtin(d) => d.permission,
            Self::Mcp(..) => Permission::Network,
            Self::Skill | Self::Ask(_) | Self::FindTool | Self::Summarize | Self::Semantic | Self::Docs => Permission::Read,
            Self::Subagent(r) => {
                if r.writes() {
                    Permission::Write
                } else {
                    Permission::Read
                }
            }
            // Both worktree tools only touch a worktree this task created, outside the working tree,
            // so they are edits: discarding it is the agent cleaning up its own scratch space. As
            // `delete` they needed an explicit opt-in the user never writes, which left every
            // subagent's worktree behind and failed the call.
            Self::Worktree(_) => Permission::Write,
            Self::Plan(true) => Permission::Write,
            Self::Plan(false) => Permission::Read,
        }
    }
    fn is_read(&self) -> bool {
        self.permission() == Permission::Read
    }
    /// Reads and subagents (each writer has its own worktree) can run concurrently.
    fn parallel_safe(&self) -> bool {
        (self.is_read() || matches!(self, Self::Subagent(_))) && !matches!(self, Self::Plan(_))
    }
}

impl Run {
    fn emit(&self, event: Value) {
        (self.host.emit)(&self.rec.id, event);
    }

    fn save(&mut self) {
        self.rec.updated_ms = store::now_ms();
        if let Err(e) = self.host.store.save(&self.rec) {
            self.emit(json!({ "type": "warning", "message": format!("could not save task state: {e}") }));
        }
    }

    fn total_tokens(&self) -> u64 {
        self.rec.usage.input_tokens.unwrap_or(0) + self.rec.usage.output_tokens.unwrap_or(0)
    }

    fn add_usage(&mut self, u: Usage) {
        let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        self.rec.usage = Usage {
            input_tokens: add(self.rec.usage.input_tokens, u.input_tokens),
            output_tokens: add(self.rec.usage.output_tokens, u.output_tokens),
            cached_tokens: add(self.rec.usage.cached_tokens, u.cached_tokens),
        };
    }

    /// The model's context window: the user's setting, else what the provider reports for the model,
    /// else the built-in estimate for the model family.
    async fn window_dynamic(&mut self) -> usize {
        let window = match self.limits.context_window_tokens {
            Some(tokens) => tokens,
            None => match crate::provider::context_window_dynamic(&self.host.providers, &self.provider, &self.cancel).await {
                Ok(dynamic) => dynamic,
                Err(_) => {
                    warn!("Failed to fetch dynamic context window, using static fallback");
                    crate::provider::context_window(&self.provider)
                }
            },
        };
        window.max(MIN_WINDOW)
    }

    /// Estimated prompt tokens: the character estimate, calibrated by the model's last real count.
    fn used_tokens(&self) -> usize {
        let tools: usize = self.tool_specs.iter().map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len()).sum();
        let est = compact::estimate_tokens(&self.rec.system, &self.rec.messages) + tools / compact::CHARS_PER_TOKEN;
        (est as f64 * self.token_ratio).ceil() as usize
    }

    /// Compresses the history in stages until it fits under the threshold; reports what it did.
    /// Only `messages` are ever touched. The tool list (`tool_specs`), the system prompt, and the
    /// `<tools>` guide in it are never compressed: a run that loses them starts inventing tool
    /// names and argument fields, which is far more expensive than the tokens they cost.
    fn compress(&mut self, window: usize) {
        let limit = window * self.limits.compact_percent.clamp(30, 95) as usize / 100;
        let before = self.used_tokens();
        if before <= limit {
            return;
        }
        let pruned = compact::prune(&mut self.rec.messages);
        let mut folded = 0;
        if self.used_tokens() > limit {
            let files: Vec<String> = self.rec.changed_files.iter().cloned().collect();
            folded = compact::compact(&mut self.rec.messages, &files);
        }
        // Repository excerpts are the last thing removed, and only while the history is still over the limit.
        let dropped = self.used_tokens() > limit && compact::drop_retrieval(&mut self.rec.messages) > 0;
        if pruned.parts > 0 || folded > 0 || dropped {
            self.rec.metrics.compactions += 1;
            self.emit(json!({
                "type": "compacted", "folded": folded, "pruned": pruned.parts, "droppedRetrieval": dropped,
                "beforeTokens": before, "afterTokens": self.used_tokens(), "windowTokens": window,
            }));
        }
    }

    #[instrument(skip(self), fields(task_id = %self.rec.id, provider_model = %self.provider.model))]
    async fn model_turn(&mut self, with_tools: bool) -> Result<crate::provider::ChatOutcome, ProviderError> {
        let id = self.rec.id.clone();
        let emit = self.host.emit.clone();
        let mut attempt = 0;
        loop {
            // Rebuilt on every attempt: after a fallback the model, its context window, the amount
            // of history that fits, and the request shape (e.g. Ollama's `num_ctx`) all change.
            let window = self.window_dynamic().await;
            self.compress(window);
            // A history saved while a run stopped between a tool call and its result is rejected
            // whole by the provider, so it is repaired before each request (and the repair is kept).
            if repair_history(&mut self.rec.messages) > 0 {
                self.emit(json!({
                    "type": "warning",
                    "message": "The earlier history was missing tool results (the run was interrupted). task-x filled them in and continues.",
                }));
                self.save();
            }
            let estimate = self.used_tokens() as f64 / self.token_ratio;
            let req = ChatRequest {
                system: Some(self.rec.system.clone()),
                messages: self.rec.messages.clone(),
                tools: if with_tools { self.tool_specs.clone() } else { Vec::new() },
                max_tokens: self.limits.max_output_tokens,
                temperature: None,
                context_window: u32::try_from(window).ok(),
                effort: self.limits.effort,
            };
            debug!(estimate = estimate, window = window, model = %self.provider.model, "Starting model turn");
            let started = Instant::now();
            let res = self
                .host
                .providers
                .stream_chat(&self.provider, &req, &self.cancel, |ev| {
                    if let StreamEvent::TextDelta { text } = ev {
                        emit(&id, json!({ "type": "textDelta", "text": text }));
                    }
                })
                .await;
            self.rec.metrics.model_calls += 1;
            self.rec.metrics.model_ms += started.elapsed().as_millis() as u64;
            match res {
                Err(e) if e.retryable && attempt + 1 < PROVIDER_RETRIES => {
                    attempt += 1;
                    self.rec.metrics.retries += 1;
                    let wait = Duration::from_millis(1_000 * 2u64.pow(attempt - 1));
                    self.emit(json!({ "type": "retry", "attempt": attempt, "reason": e.message, "waitMs": wait.as_millis() as u64 }));
                    tokio::select! {
                        _ = self.cancel.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled, "cancelled")),
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                Err(e) if e.retryable && !self.fallbacks.is_empty() => {
                    let next = self.fallbacks.remove(0);
                    self.rec.metrics.fallbacks += 1;
                    self.emit(json!({
                        "type": "modelSwitched",
                        "from": { "kind": self.provider.kind, "model": self.provider.model },
                        "to": { "kind": next.kind, "model": next.model },
                        "reason": e.message,
                    }));
                    self.rec.model = next.model.clone();
                    self.provider = next;
                    attempt = 0;
                }
                Ok(out) => {
                    if let Some(input) = out.usage.input_tokens.filter(|&i| i > 0 && estimate > 0.0) {
                        self.token_ratio = (input as f64 / estimate).clamp(0.5, 3.0);
                    }
                    let used = out.usage.input_tokens.map_or_else(|| self.used_tokens(), |i| i as usize);
                    self.emit(json!({ "type": "context", "usedTokens": used, "windowTokens": window }));
                    let t = out.usage.input_tokens.unwrap_or(0) + out.usage.output_tokens.unwrap_or(0);
                    info!(
                        input_tokens = out.usage.input_tokens.unwrap_or(0),
                        output_tokens = out.usage.output_tokens.unwrap_or(0),
                        local = is_local(&self.provider),
                        "model call"
                    );
                    if is_local(&self.provider) {
                        self.rec.metrics.local_tokens += t;
                    } else {
                        self.rec.metrics.billed_tokens += t;
                        match cost::estimate(&self.prices, &self.provider, &out.usage) {
                            Some(c) => self.rec.metrics.cost_usd += c,
                            None => self.rec.metrics.unpriced_tokens += t,
                        }
                    }
                    return Ok(out);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Puts the questions to the user and waits for the answers (or a dismissal).
    async fn ask(&self, call: &ToolCall, qs: &[Question]) -> Value {
        let req = AskRequest { task_id: self.rec.id.clone(), call_id: call.id.clone(), questions: qs.to_vec() };
        let answers = (self.host.ask)(req, self.cancel.clone()).await;
        answer_json(qs, answers)
    }

    /// Applies the permission policy and the user's answer. A policy denial is `Forbidden` (Blocked:
    /// nothing the model sends can change it); a denial the user chose is a plain failure it can
    /// react to by changing the approach.
    async fn decide(&mut self, call: &ToolCall, name: &str, permission: Permission) -> Result<(), tools::ToolError> {
        let denied = |why: &str| {
            tools::ToolError::new(
                ToolErrorKind::Forbidden,
                format!("Permission denied: {name} ({permission:?}) is blocked by the permission policy; {why}"),
            )
        };
        match self.policy.decide(name, permission) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(denied("do not call it again, use another way or tell the user what they must allow")),
            Decision::Ask => {
                let req = ApprovalRequest {
                    task_id: self.rec.id.clone(),
                    call_id: call.id.clone(),
                    tool: name.into(),
                    permission,
                    input: call.input.clone(),
                    preview: preview(name, &call.input),
                };
                self.emit(json!({ "type": "approvalRequested", "callId": call.id, "tool": name }));
                let d = (self.host.approve)(req, self.cancel.clone()).await;
                self.emit(json!({ "type": "approvalResolved", "callId": call.id, "decision": format!("{d:?}") }));
                match d {
                    ApprovalDecision::Allow => Ok(()),
                    ApprovalDecision::AllowSession => {
                        self.policy.grant_session(name);
                        self.host.grants.lock().expect("grants lock").insert(name.into());
                        Ok(())
                    }
                    // The user decided, not the policy: the model may try another way.
                    ApprovalDecision::Deny => Err(tools::ToolError::new(
                        ToolErrorKind::Failed,
                        "The user denied this action. Do not retry it; adjust the approach or ask the user.",
                    )),
                }
            }
        }
    }

    fn ctx(&self) -> Result<Arc<ToolCtx>, String> {
        self.ctx_with(self.cancel.child_token())
    }

    fn ctx_with(&self, cancel: CancellationToken) -> Result<Arc<ToolCtx>, String> {
        let root = self
            .host
            .workspace
            .read()
            .expect("workspace lock")
            .as_ref()
            .map(|w| w.index.root().to_path_buf())
            .ok_or("no workspace is open")?;
        Ok(Arc::new(ToolCtx {
            root,
            workspace: self.host.workspace.clone(),
            cancel,
            seen: self.host.seen.clone(),
            edits: self.host.edits.clone(),
            task_id: Some(self.rec.id.clone()),
        }))
    }

    /// Checks one call (availability, input shape, repeated failures, hooks, permission) and returns
    /// what to execute. Approval prompts happen here, once per call, and only after the hooks that
    /// can still block the call have run.
    async fn prepare(&mut self, call: &ToolCall) -> Result<Target, tools::ToolError> {
        let failed = |m: &str| tools::ToolError::new(ToolErrorKind::Failed, m);
        // Bad input is the model's to fix: a UserError tells it so instead of "the same call fails again".
        let invalid = |m: &str| tools::ToolError::new(ToolErrorKind::InvalidInput, m);
        let blocked = |m: &str| tools::ToolError::new(ToolErrorKind::Forbidden, m);
        // The tool list is fixed for the task, so a tool that is not in it is out of reach whatever
        // the model sends: say so as Blocked rather than as a failing call it should keep trying.
        // A deferred tool called by name is loaded on the spot: the model knew its name from the
        // find_tool list, so making it ask first would only cost a turn.
        if let Some(i) = self.deferred.iter().position(|s| s.name == call.name) {
            let spec = self.deferred.remove(i);
            self.tool_specs.push(spec);
        }
        if !self.tool_specs.iter().any(|s| s.name == call.name) {
            return Err(blocked(&format!("Tool '{}' is not available in this mode.", call.name)));
        }
        let target = match tools::find(&call.name) {
            Some(def) => Target::Builtin(def),
            None if call.name == READ_SKILL => Target::Skill,
            None if call.name == "run_subagent" => {
                let raw = call.input["role"].as_str().unwrap_or_default();
                let role = team::Role::parse(raw).ok_or_else(|| {
                    invalid(&format!("unknown role '{raw}'; use one of: {} (any capitalisation works)", team::Role::NAMES))
                })?;
                Target::Subagent(role)
            }
            None if call.name == "run_plan" => Target::Plan(
                call.input["steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|s| serde_json::from_value::<team::Role>(s["role"].clone()).map_or(true, |r: team::Role| r.writes())),
            ),
            None if call.name == ASK_USER => Target::Ask(parse_questions(&call.input).map_err(|e| invalid(&e))?),
            None if call.name == FIND_TOOL => Target::FindTool,
            None if call.name == SUMMARIZE => Target::Summarize,
            None if call.name == SEMANTIC => Target::Semantic,
            None if call.name == DOCS_SEARCH => Target::Docs,
            None if call.name == "worktree_apply" => Target::Worktree("worktree_apply"),
            None if call.name == "worktree_discard" => Target::Worktree("worktree_discard"),
            None => self
                .mcp
                .get(&call.name)
                .map(|(c, t)| Target::Mcp(c.clone(), t.clone()))
                .ok_or_else(|| failed(&format!("Unknown tool '{}'.", call.name)))?,
        };
        if !call.input.is_object() {
            return Err(failed("Tool input must be a JSON object."));
        }
        let key = format!("{}:{}", call.name, call.input);
        if self.failures.get(&key).copied().unwrap_or(0) >= MAX_IDENTICAL_FAILURES {
            return Err(failed(
                "This exact call already failed twice. Change the approach (re-read the file, adjust the input, or choose another tool).",
            ));
        }
        let permission = if matches!(call.name.as_str(), "copy_file" | "move_file")
            && call.input["from"].as_str().is_some_and(|from| tools::outside_source(&self.cwd, from))
        {
            // Reading a file from outside the workspace is not covered by the workspace rules, so it
            // asks the user (and a read-only mode refuses it). Inside, both are ordinary edits.
            Permission::External
        } else {
            target.permission()
        };
        // Hooks run before the approval prompt: a hook that blocks a call must not leave the user with
        // a prompt for work that will not happen, and an approval must not be able to override it.
        let payload = json!({ "taskId": self.rec.id, "tool": call.name, "input": call.input });
        self.hook(hooks::Event::BeforeTool, &call.name, payload.clone(), None).await.map_err(|e| blocked(&e))?;
        if matches!(permission, Permission::Write | Permission::Delete | Permission::External) {
            self.hook(hooks::Event::BeforeEdit, &call.name, payload.clone(), None).await.map_err(|e| blocked(&e))?;
        }
        if VALIDATION_TOOLS.contains(&call.name.as_str()) {
            self.hook(hooks::Event::BeforeValidation, &call.name, payload, None).await.map_err(|e| blocked(&e))?;
        }
        self.decide(call, &call.name, permission).await?;
        Ok(target)
    }

    /// The hook context for this task: the event, the tool and its file, and the token budget.
    fn hook_context(
        &self,
        event: hooks::Event,
        tool: Option<&str>,
        payload: Value,
        severity: Option<errors::Severity>,
    ) -> hooks::HookContext {
        let mut ctx = hooks::HookContext::new(event, tool, payload);
        (ctx.budget_used, ctx.budget_limit, ctx.severity) = (Some(self.total_tokens()), self.limits.max_total_tokens, severity);
        ctx
    }

    /// Runs the lifecycle hooks for a tool event on a blocking thread; Err means a hook blocked it.
    async fn hook(&self, event: hooks::Event, tool: &str, payload: Value, severity: Option<errors::Severity>) -> Result<(), String> {
        if self.hooks.is_empty() {
            return Ok(());
        }
        let ctx = self.hook_context(event, Some(tool), payload, severity);
        let (hooks, cwd, cancel) = (self.hooks.clone(), self.cwd.clone(), self.cancel.clone());
        let results = tokio::task::spawn_blocking(move || hooks::run(&hooks, &ctx, &cwd, &cancel)).await.unwrap_or_default();
        for o in &results {
            self.emit(json!({ "type": "hook", "event": event, "command": o.command, "blocked": o.blocked, "message": o.message(), "durationMs": o.duration_ms }));
        }
        match results.iter().find(|o| o.blocked) {
            Some(o) => Err(format!("Blocked by a {event:?} hook ({}): {}", o.command, o.message())),
            None => Ok(()),
        }
    }

    async fn subagent(&self, role: team::Role, call: &ToolCall) -> Result<Value, String> {
        self.spawn_sub(role, call, &[], None, None).await.map(|(v, _)| v)
    }

    /// Runs a subagent to completion. Writers get a fresh worktree (with `base` patches applied as
    /// a commit) and their own workspace index; their events are forwarded to this task as
    /// `subagent` events. Returns the tool result and, for writers, the full patch.
    async fn spawn_sub(
        &self,
        role: team::Role,
        call: &ToolCall,
        base: &[String],
        provider: Option<ProviderConfig>,
        step: Option<&str>,
    ) -> Result<(Value, Option<String>), String> {
        let n = self.sub_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let name = call.input["name"]
            .as_str()
            .filter(|s| team::valid_name(s))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}-{:?}-{n}", self.rec.id, role).to_lowercase());
        let objective = call.input["objective"].as_str().unwrap_or_default();
        let context = call.input["context"].as_str().unwrap_or_default();
        let provider = provider
            .or_else(|| self.role_providers.iter().find(|r| r.role == role).map(|r| r.provider.clone()))
            // Research only reads and reports, so the cheap helper model does it when one is set.
            .or_else(|| self.helper.clone().filter(|_| role == team::Role::Research))
            .unwrap_or_else(|| self.provider.clone());
        let mut host = self.host.clone();
        host.store = Store::new(None);
        host.seen = Arc::default();
        host.edits = Arc::default();
        let mut dir = None;
        if role.writes() {
            let (repo, cancel, nm, base) = (self.cwd.clone(), self.cancel.clone(), name.clone(), base.to_vec());
            let d = tokio::task::spawn_blocking(move || team::create_with_base(&repo, &nm, &base, &cancel))
                .await
                .map_err(|e| e.to_string())??;
            self.worktrees.lock().expect("worktrees lock").insert(name.clone(), false);
            let d2 = d.clone();
            let ws = tokio::task::spawn_blocking(move || Workspace::open(&d2, &[])).await.map_err(|e| e.to_string())??;
            host.workspace = Arc::new(RwLock::new(Some(ws)));
            dir = Some(d);
        }
        let child_id = format!("{}-sub{n}", self.rec.id);
        let (parent_emit, parent_id, nm) = (self.host.emit.clone(), self.rec.id.clone(), name.clone());
        host.emit =
            Arc::new(move |task, ev| parent_emit(&parent_id, json!({ "type": "subagent", "name": nm, "taskId": task, "event": ev })));
        let mut policy = self.policy_cfg.clone();
        if role.writes() && policy.mode == Mode::AskBeforeEdit {
            policy.mode = Mode::AskBeforeExecution; // Writes land in the isolated worktree, reviewed later.
        }
        let params = RunParams {
            images: Vec::new(),
            prompt: format!(
                "{}\n\nObjective:\n{objective}\n\nContext from the lead agent:\n{}",
                role.brief(),
                if context.is_empty() { "(none)" } else { context }
            ),
            provider: provider.clone(),
            chat_mode: if role.writes() { ChatMode::Agent } else { ChatMode::Ask },
            policy,
            editor: EditorContext::default(),
            limits: Limits { max_iterations: SUBAGENT_ITERATIONS, deadline_secs: SUBAGENT_DEADLINE_SECS, ..self.limits.clone() },
            task_id: Some(child_id.clone()),
            routes: Vec::new(),
            fallbacks: self.fallbacks.clone(),
            prices: self.prices.clone(),
            role_providers: Vec::new(),
            helper: self.helper.clone(),
            embeddings: self.embeddings.clone(),
            depth: self.depth + 1,
        };
        let child_cancel = self.cancel.child_token();
        self.host.subagents.lock().expect("subagents lock").insert(child_id.clone(), child_cancel.clone());
        self.emit(json!({
            "type": "subagentStarted", "name": name, "taskId": child_id, "role": role, "objective": objective,
            "kind": provider.kind, "model": provider.model, "dir": dir, "step": step,
        }));
        let res = Box::pin(run(host, params, child_cancel)).await;
        self.host.subagents.lock().expect("subagents lock").remove(&child_id);
        let rec = match res {
            Ok(r) => r,
            Err(e) => {
                self.emit(
                    json!({ "type": "subagentFinished", "name": name, "taskId": child_id, "role": role, "status": "failed", "error": e }),
                );
                return Err(e);
            }
        };
        let m = &rec.metrics;
        let tokens = m.local_tokens + m.billed_tokens;
        {
            let mut t = self.sub_totals.lock().expect("subtotals lock");
            t.count += 1;
            t.cost_usd += m.cost_usd;
            t.tokens += tokens;
            t.unpriced += m.unpriced_tokens;
        }
        let mut out = json!({
            "role": role,
            "status": rec.status,
            "report": rec.final_text,
            "changedFiles": rec.changed_files,
            "validations": rec.validations,
            "model": provider.model,
            "costUsd": m.cost_usd,
            "tokens": tokens,
        });
        let mut full = None;
        let (mut files, mut added, mut removed) = (Vec::new(), 0, 0);
        if role.writes() {
            let (repo, cancel, nm) = (self.cwd.clone(), self.cancel.clone(), name.clone());
            let patch = tokio::task::spawn_blocking(move || team::diff(&repo, &nm, &cancel)).await.map_err(|e| e.to_string())??;
            (added, removed) = team::patch_stat(&patch);
            files = team::patch_paths(&patch);
            out["worktree"] = json!(name);
            out["dir"] = json!(dir);
            out["patch"] = json!(truncate(&patch, MAX_PATCH_CHARS));
            out["next"] = json!("Review the patch, then call worktree_apply or worktree_discard.");
            full = Some(patch);
        }
        self.emit(json!({
            "type": "subagentFinished", "name": name, "taskId": child_id, "role": role, "status": rec.status, "error": rec.error,
            "costUsd": m.cost_usd, "unpricedTokens": m.unpriced_tokens, "tokens": tokens, "files": files, "added": added, "removed": removed,
            "worktree": role.writes().then_some(&name),
        }));
        Ok((out, full))
    }

    /// Worktree name for a plan step: unique per task and within `team::valid_name` limits.
    fn step_worktree(&self, step: &str) -> String {
        let id: String = self.rec.id.chars().rev().take(48 - step.len() - 1).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{id}-{step}")
    }

    /// Runs one plan step as a subagent. `deps` are (id, result) of direct dependencies; `base`
    /// holds the patches of all writing ancestors, in dependency order.
    async fn run_step(
        &self,
        step: &PlanStep,
        deps: Vec<(String, Value)>,
        base: Vec<String>,
        call_id: &str,
    ) -> (usize, Result<(Value, Option<String>), String>) {
        let mut context = step.context.clone();
        for (id, out) in &deps {
            context
                .push_str(&format!("\n\nResult of step {id}:\n{}", truncate(out["report"].as_str().unwrap_or(""), MAX_DEP_REPORT_CHARS)));
        }
        let sub = ToolCall {
            id: format!("{call_id}:{}", step.id),
            name: "run_subagent".into(),
            input: json!({ "role": step.role, "objective": step.objective, "context": context, "name": self.step_worktree(&step.id) }),
        };
        (0, self.spawn_sub(step.role, &sub, &base, None, Some(&step.id)).await)
    }

    /// Runs a plan: a scheduler starts every step whose dependencies are done, up to the parallel
    /// limit, then the work of writing steps is merged into the working tree in dependency order.
    async fn run_plan(&self, call: &ToolCall) -> Result<Value, String> {
        let input: PlanInput = serde_json::from_value(call.input.clone()).map_err(|e| format!("invalid plan: {e}"))?;
        let steps = input.steps;
        let order = plan_order(&steps)?;
        let max = self.limits.max_parallel.clamp(1, MAX_PARALLEL) as usize;
        self.emit(json!({
            "type": "planStarted", "callId": call.id, "maxParallel": max,
            "steps": steps.iter().map(|s| json!({ "id": s.id, "role": s.role, "objective": s.objective, "dependsOn": s.depends_on })).collect::<Vec<_>>(),
        }));
        let index: HashMap<&str, usize> = steps.iter().enumerate().map(|(i, s)| (s.id.as_str(), i)).collect();
        let mut results: HashMap<usize, StepResult> = HashMap::new();
        let mut started: HashSet<usize> = HashSet::new();
        let mut running = FuturesUnordered::new();
        loop {
            // Start every ready step; a step whose dependency failed is skipped, which can make more steps ready.
            let mut changed = !self.cancel.is_cancelled();
            while changed {
                changed = false;
                for &i in &order {
                    if running.len() >= max || started.contains(&i) {
                        continue;
                    }
                    let s = &steps[i];
                    let deps: Vec<usize> = s.depends_on.iter().map(|d| index[d.as_str()]).collect();
                    if !deps.iter().all(|d| results.contains_key(d)) {
                        continue;
                    }
                    started.insert(i);
                    let failed: Vec<&str> = deps.iter().filter(|d| !results[*d].ok).map(|d| steps[*d].id.as_str()).collect();
                    if !failed.is_empty() {
                        let reason = format!("skipped: needs {} which did not finish", failed.join(", "));
                        self.emit(json!({ "type": "stepSkipped", "step": s.id, "reason": reason }));
                        results.insert(
                            i,
                            StepResult { ok: false, out: json!({ "status": "skipped", "report": reason }), patch: None, worktree: None },
                        );
                        changed = true;
                        continue;
                    }
                    let direct = deps.iter().map(|d| (steps[*d].id.clone(), results[d].out.clone())).collect();
                    let mut ancestors: Vec<usize> = Vec::new();
                    let mut todo = deps.clone();
                    while let Some(a) = todo.pop() {
                        if !ancestors.contains(&a) {
                            ancestors.push(a);
                            todo.extend(steps[a].depends_on.iter().map(|d| index[d.as_str()]));
                        }
                    }
                    let base = order.iter().filter(|o| ancestors.contains(o)).filter_map(|o| results[o].patch.clone()).collect();
                    running.push(async move {
                        let (_, r) = self.run_step(s, direct, base, &call.id).await;
                        (i, r)
                    });
                }
            }
            let Some((i, res)) = running.next().await else { break };
            let r = match res {
                Ok((out, patch)) => {
                    StepResult { ok: out["status"] == "completed", worktree: out["worktree"].as_str().map(str::to_owned), out, patch }
                }
                Err(e) => StepResult { ok: false, out: json!({ "status": "failed", "report": e }), patch: None, worktree: None },
            };
            results.insert(i, r);
        }
        let mut changed: Vec<Value> = Vec::new();
        let mut operations: Vec<Value> = Vec::new();
        let mut merges: HashMap<usize, Value> = HashMap::new();
        if input.merge && !self.cancel.is_cancelled() {
            for &i in &order {
                let Some(name) = results
                    .get(&i)
                    .filter(|r| r.ok && r.patch.as_deref().is_some_and(|p| !p.trim().is_empty()))
                    .and_then(|r| r.worktree.clone())
                else {
                    continue;
                };
                let ctx = self.ctx()?;
                let (repo, cancel, nm) = (self.cwd.clone(), self.cancel.clone(), name.clone());
                let res = tokio::task::spawn_blocking(move || {
                    let r = team::merge(&ctx, &repo, &nm, &cancel);
                    if r.is_ok() {
                        let _ = team::remove(&repo, &nm, true, true, &cancel);
                    }
                    r
                })
                .await
                .map_err(|e| e.to_string())?;
                let ok = res.is_ok();
                let m = match res {
                    Ok(v) => {
                        self.worktrees.lock().expect("worktrees lock").remove(&name);
                        for ch in v["changed"].as_array().into_iter().flatten() {
                            changed.push(json!({ "path": ch["path"], "diff": ch["diff"], "operation": v["operation"] }));
                        }
                        operations.push(v["operation"].clone());
                        json!({ "merged": true, "files": v["files"] })
                    }
                    Err(e) => json!({ "merged": false, "error": e, "worktree": name }),
                };
                self.emit(
                    json!({ "type": "merged", "step": steps[i].id, "name": name, "ok": ok, "files": m["files"], "error": m["error"] }),
                );
                merges.insert(i, m);
            }
        }
        let report: Vec<Value> = order
            .iter()
            .map(|&i| {
                let s = &steps[i];
                let r = results.get(&i);
                let out = r.map(|r| &r.out);
                let mut v = json!({
                    "id": s.id,
                    "role": s.role,
                    "status": out.map_or(json!("cancelled"), |o| o["status"].clone()),
                    "report": out.and_then(|o| o["report"].as_str()).map(|t| truncate(t, MAX_DEP_REPORT_CHARS)),
                    "changedFiles": out.map(|o| o["changedFiles"].clone()),
                    "validations": out.map(|o| o["validations"].clone()),
                });
                if let Some(m) = merges.get(&i) {
                    v["merge"] = m.clone();
                    if m["merged"] == false {
                        v["patch"] = json!(r.and_then(|r| r.patch.as_deref()).map(|p| truncate(p, MAX_PATCH_CHARS)));
                    }
                } else if let Some(w) = r.and_then(|r| r.worktree.as_ref()).filter(|_| !input.merge) {
                    v["worktree"] = json!(w);
                }
                v
            })
            .collect();
        Ok(json!({
            "steps": report,
            "changed": changed,
            "operations": operations,
            "next": "Check each step's result. Resolve failed merges yourself, then run the project's checks.",
        }))
    }

    fn skipped_checks(&self) -> Vec<String> {
        if self.rec.changed_files.is_empty() {
            return Vec::new();
        }
        let ws = self.host.workspace.read().expect("workspace lock");
        let Some(w) = ws.as_ref() else { return Vec::new() };
        let c = &w.project.commands;
        [
            ("format", "format_code", &c.format),
            ("lint", "run_linter", &c.lint),
            ("tests", "run_tests", &c.test),
            ("build", "check_build", &c.build),
        ]
        .iter()
        .filter(|(_, tool, cmds)| {
            !cmds.is_empty()
                && !self.rec.validations.iter().any(|v| v.tool == *tool || (*tool == "check_build" && v.tool == "get_diagnostics"))
        })
        .map(|(name, _, cmds)| format!("{name} ({})", cmds[0].argv.join(" ")))
        .collect()
    }

    async fn execute(&self, target: &Target, call: &ToolCall) -> Result<Value, tools::ToolError> {
        let started = Instant::now();
        let permission = target.permission();
        // The blocking hooks already ran in `prepare`, before the approval prompt.
        self.emit(json!({ "type": "toolStarted", "callId": call.id, "tool": call.name, "input": call.input, "permission": permission }));
        let res = match target {
            // Explicit background calls, and builds, tests, and installs, return at once for every provider.
            Target::Builtin(def)
                if def.name == "run_command"
                    && (call.input["background"] == true
                        || (call.input["background"] != false && tools::exec::is_long_command(&command_text(&call.input["command"])))) =>
            {
                self.start_background(def, call)
            }
            // A command that keeps running is handed to the background instead of blocking the turn.
            Target::Builtin(def) if def.name == "run_command" => self.command_with(def, call, COMMAND_HANDOFF).await,
            Target::Builtin(def) => {
                // The user can stop this call alone; the task goes on.
                let stop = self.cancel.child_token();
                let key = format!("{}:{}", self.rec.id, call.id);
                self.host.tool_cancels.lock().expect("tool cancels lock").insert(key.clone(), stop.clone());
                // Only read-only calls with a transient failure run again, a few times at most.
                let mut retries = 0;
                let res = loop {
                    let ctx = match self.ctx_with(stop.child_token()) {
                        Ok(c) => c,
                        Err(e) => break Err(tools::ToolError::new(ToolErrorKind::Failed, e)),
                    };
                    let res = tools::call(def, ctx, call.input.clone()).await;
                    let Err(e) = &res else { break res };
                    let failure = errors::Error { retry_count: retries, ..errors::Error::from_tool(&call.name, e) };
                    if !target.parallel_safe() || !failure.is_retryable() {
                        break res;
                    }
                    retries = failure.retry().retry_count;
                    let jitter =
                        u64::from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()));
                    let wait = tools::retry_backoff(retries, jitter);
                    info!(tool = %call.name, attempt = retries, wait_ms = wait.as_millis() as u64, "retrying a read-only tool call after a retryable error");
                    let cancelled = tokio::select! {
                        _ = stop.cancelled() => true,
                        _ = tokio::time::sleep(wait) => false,
                    };
                    if cancelled {
                        break Err(tools::ToolError::new(ToolErrorKind::Cancelled, "cancelled"));
                    }
                };
                self.host.tool_cancels.lock().expect("tool cancels lock").remove(&key);
                if stop.is_cancelled() && !self.cancel.is_cancelled() {
                    Err(tools::ToolError::new(ToolErrorKind::Cancelled, USER_STOPPED))
                } else {
                    res
                }
            }
            Target::Subagent(role) => self.subagent(*role, call).await.map_err(|e| tools::ToolError::new(ToolErrorKind::Failed, e)),
            Target::Plan(_) => self.run_plan(call).await.map_err(|e| tools::ToolError::new(ToolErrorKind::Failed, e)),
            Target::Ask(qs) => Ok(self.ask(call, qs).await),
            Target::FindTool => {
                let found = match_tools(&self.deferred, &call.input);
                Ok(
                    json!({ "loaded": found, "note": if found.is_empty() { "No tool matched; check the names in the find_tool description." } else { "Call them from the next step." } }),
                )
            }
            Target::Summarize => self.summarize(call).await,
            Target::Semantic => self.semantic_search(call).await,
            Target::Docs => self.docs_search(call).await,
            Target::Worktree(op) => {
                let name = call.input["name"].as_str().unwrap_or_default().to_owned();
                let (repo, cancel, op) = (self.cwd.clone(), self.cancel.clone(), *op);
                let applied = self.worktrees.lock().expect("worktrees lock").get(&name).copied();
                if applied.is_none() {
                    return Err(tools::ToolError::new(
                        ToolErrorKind::NotFound,
                        format!("no subagent worktree named '{name}' in this task"),
                    ));
                }
                let ctx = self.ctx().map_err(|e| tools::ToolError::new(ToolErrorKind::Failed, e))?;
                let res = tokio::task::spawn_blocking(move || match op {
                    "worktree_apply" => team::merge(&ctx, &repo, &name, &cancel).map(|v| (v, true)),
                    _ => team::remove(&repo, &name, true, false, &cancel).map(|_| (json!({ "discarded": name }), false)),
                })
                .await
                .map_err(|e| tools::ToolError::new(ToolErrorKind::Failed, e.to_string()))?
                .map_err(|e| tools::ToolError::new(ToolErrorKind::Conflict, e))?;
                let wt = call.input["name"].as_str().unwrap_or_default().to_owned();
                if res.1 {
                    // The merge is in the edit log (reviewable and undoable) and reindexed.
                    self.worktrees.lock().expect("worktrees lock").insert(wt.clone(), true);
                } else {
                    self.worktrees.lock().expect("worktrees lock").remove(&wt);
                }
                Ok(res.0)
            }
            Target::Skill => {
                let name = call.input["name"].as_str().unwrap_or_default();
                self.skills
                    .get(name)
                    .map(|b| json!({ "name": name, "instructions": b }))
                    .ok_or_else(|| tools::ToolError::new(ToolErrorKind::NotFound, format!("no skill named '{name}'")))
            }
            Target::Mcp(client, tool) => client
                .call(tool, call.input.clone(), &self.cancel)
                .await
                .map(tools::limit_output)
                .map_err(|e| tools::ToolError::new(ToolErrorKind::Failed, e)),
        };
        info!(tool = %call.name, ok = res.is_ok(), class = ?res.as_ref().err().map(|e| e.kind.class()), ms = started.elapsed().as_millis() as u64, "tool call");
        let after = json!({ "taskId": self.rec.id, "tool": call.name, "input": call.input, "ok": res.is_ok() });
        let severity = res.as_ref().err().map(|e| errors::Error::from_tool(&call.name, e).severity);
        let _ = self.hook(hooks::Event::AfterTool, &call.name, after.clone(), severity).await;
        if res.as_ref().is_ok_and(|v| v["changed"].as_array().is_some_and(|a| !a.is_empty())) {
            let _ = self.hook(hooks::Event::AfterEdit, &call.name, after, None).await;
        }
        res
    }

    /// `semantic_search`: embeds the project's changed chunks (the first call embeds them all, with
    /// progress notes), then returns the chunks closest to the query.
    async fn semantic_search(&self, call: &ToolCall) -> Result<Value, tools::ToolError> {
        use crate::context::semantic;
        let fail = |m: String| tools::ToolError::new(ToolErrorKind::Failed, m);
        let cfg = self.embeddings.clone().ok_or_else(|| fail("semantic search needs an embedding model: set task-x.embeddings.provider and task-x.embeddings.model".into()))?;
        let query = call.input["query"].as_str().map(str::trim).filter(|q| !q.is_empty()).ok_or_else(|| tools::ToolError::invalid("query is required"))?;
        let limit = call.input["limit"].as_u64().unwrap_or(8).clamp(1, 20) as usize;
        let glob = match call.input["glob"].as_str().filter(|g| !g.trim().is_empty()) {
            Some(g) => Some(globset::Glob::new(g).map_err(|e| tools::ToolError::invalid(format!("bad glob: {e}")))?.compile_matcher()),
            None => None,
        };
        let data = self.host.store.data_dir().ok_or_else(|| fail("no data folder for the index".into()))?;
        // The files, read once: (root, every chunk to embed, and each file's text for the hits).
        let (root, pieces, texts) = {
            let ws = self.host.workspace.read().expect("workspace lock");
            let w = ws.as_ref().ok_or_else(|| fail("no workspace is open".into()))?;
            let mut pieces = Vec::new();
            let mut texts = HashMap::new();
            for f in w.index.files() {
                let Some(c) = f.content.clone() else { continue };
                pieces.extend(semantic::code_pieces(&f.rel, &c));
                texts.insert(f.rel.clone(), c);
            }
            (w.index.root().to_path_buf(), pieces, texts)
        };
        let path = semantic::code_store_path(&data, &root);
        let model = format!("{:?}:{}", cfg.kind, cfg.model);
        let _guard = SEMANTIC_LOCK.lock().await;
        let mut store = semantic::Store::load(&path);
        let todo = store.plan(&model, pieces, false);
        if todo.len() > semantic::BATCH {
            self.emit(json!({ "type": "warning", "message": format!("Semantic search: embedding {} code chunks with {} (later searches only embed changed files).", todo.len(), cfg.model) }));
        }
        for batch in todo.chunks(semantic::BATCH) {
            let embed: Vec<String> = batch.iter().map(semantic::embed_text).collect();
            let vecs = match self.host.providers.embed(&cfg, &embed, &self.cancel).await {
                Ok(v) => v,
                Err(e) => {
                    // Keep the batches already embedded, so the next search does not repeat them.
                    let _ = store.save(&path);
                    return Err(fail(format!("embedding failed: {}", e.message)));
                }
            };
            for (p, v) in batch.iter().zip(vecs) {
                store.add(semantic::Piece { source: p.source.clone(), start: p.start, end: p.end, title: String::new(), text: p.text.clone() }, v, false);
            }
        }
        if !todo.is_empty() {
            let _ = store.save(&path);
        }
        let q = self.host.providers.embed(&cfg, &[query.to_owned()], &self.cancel).await.map_err(|e| fail(format!("embedding failed: {}", e.message)))?;
        let hits = store.search(&q[0], limit, |c| glob.as_ref().is_none_or(|g| g.is_match(&c.source)));
        let results: Vec<Value> = hits
            .into_iter()
            .map(|(score, c)| {
                let text = texts.get(&c.source).map(|t| t.lines().skip(c.start - 1).take(c.end + 1 - c.start).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                json!({ "path": c.source, "startLine": c.start, "endLine": c.end, "score": (score * 1000.0).round() / 1000.0, "text": truncate(&text, MAX_HIT_CHARS) })
            })
            .collect();
        Ok(json!({ "query": query, "chunks": store.chunks.len(), "results": results }))
    }

    /// `docs_search`: the best passages of the indexed library docs.
    async fn docs_search(&self, call: &ToolCall) -> Result<Value, tools::ToolError> {
        let fail = |m: String| tools::ToolError::new(ToolErrorKind::Failed, m);
        let query = call.input["query"].as_str().map(str::trim).filter(|q| !q.is_empty()).ok_or_else(|| tools::ToolError::invalid("query is required"))?;
        let limit = call.input["limit"].as_u64().unwrap_or(6).clamp(1, 20) as usize;
        let source = call.input["source"].as_str().map(str::trim).filter(|s| !s.is_empty());
        let data = self.host.store.data_dir().ok_or_else(|| fail("no data folder for docs".into()))?;
        let mut r = crate::docs::search(&self.host.providers, self.embeddings.as_ref(), &data, source, query, limit, &self.cancel).await.map_err(fail)?;
        for hit in r["results"].as_array_mut().into_iter().flatten() {
            if let Some(t) = hit["text"].as_str() {
                hit["text"] = json!(truncate(t, MAX_HIT_CHARS));
            }
        }
        Ok(r)
    }

    /// A helper model (or the task's model) reads a file and returns a short summary, so the file
    /// itself never enters this conversation.
    async fn summarize(&self, call: &ToolCall) -> Result<Value, tools::ToolError> {
        let fail = |m: String| tools::ToolError::new(ToolErrorKind::Failed, m);
        let path = call.input["path"].as_str().unwrap_or_default();
        let ctx = self.ctx().map_err(fail)?;
        let (rel, text) = tools::fs::read_text(&ctx, path)?;
        let numbered: String = text.lines().enumerate().map(|(n, l)| format!("{:>5} {l}\n", n + 1)).collect();
        let cut = numbered.len() > MAX_SUMMARY_SOURCE_CHARS;
        let source = truncate(&numbered, MAX_SUMMARY_SOURCE_CHARS);
        let question = call.input["question"]
            .as_str()
            .filter(|q| !q.trim().is_empty())
            .unwrap_or("What does this file do, and what are its main parts?");
        let provider = self.helper.clone().unwrap_or_else(|| self.provider.clone());
        let req = ChatRequest {
            system: Some("You summarize one source file for a coding agent that will not read it. Be short and exact: the file's purpose, its main parts with their line numbers, and the answer to the question. Quote names exactly. If the file does not answer the question, say so.".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentPart::Text { text: format!("<file path=\"{rel}\">\n{source}</file>\n\nQuestion: {question}") }],
            }],
            tools: Vec::new(),
            max_tokens: Some(SUMMARY_TOKENS),
            temperature: None,
            context_window: None,
            effort: None,
        };
        let out = self.host.providers.stream_chat(&provider, &req, &self.cancel, |_| {}).await.map_err(|e| fail(e.message))?;
        let t = out.usage.input_tokens.unwrap_or(0) + out.usage.output_tokens.unwrap_or(0);
        let mut totals = self.sub_totals.lock().expect("sub totals lock");
        totals.tokens += t;
        match (is_local(&provider), cost::estimate(&self.prices, &provider, &out.usage)) {
            (true, _) => {}
            (false, Some(c)) => totals.cost_usd += c,
            (false, None) => totals.unpriced += t,
        }
        Ok(json!({ "path": rel, "lines": text.lines().count(), "summary": out.text.trim(), "model": provider.model, "partial": cut }))
    }

    /// A repeated read whose earlier result is still in the conversation, unchanged: a short note
    /// points back to it instead of sending the same content again.
    fn unchanged_read(&mut self, call: &ToolCall, result: &str) -> Option<String> {
        if !REREAD_TOOLS.contains(&call.name.as_str()) {
            return None;
        }
        let key = format!("{}:{}", call.name, call.input);
        let hash = tools::fingerprint(result);
        if let Some((h, prev)) = self.reads.get(&key).filter(|(h, _)| *h == hash) {
            let still_there = self.rec.messages.iter().flat_map(|m| m.content.iter()).any(|p| {
                matches!(p, ContentPart::ToolResult { tool_use_id, content, .. } if tool_use_id == prev && tools::fingerprint(content) == *h)
            });
            if still_there {
                return Some(
                    json!({ "unchanged": true, "sameAs": prev, "note": "Same result as an earlier call with this input, which is still above in this conversation. Use that, or read a different range." })
                        .to_string(),
                );
            }
        }
        self.reads.insert(key, (hash, call.id.clone()));
        None
    }

    fn record(&mut self, call: &ToolCall, res: &Result<Value, tools::ToolError>) -> ContentPart {
        let key = format!("{}:{}", call.name, call.input);
        if call.name == FIND_TOOL {
            let loaded: Vec<String> = res
                .as_ref()
                .ok()
                .and_then(|v| v["loaded"].as_array())
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            for name in &loaded {
                if let Some(i) = self.deferred.iter().position(|s| &s.name == name) {
                    let spec = self.deferred.remove(i);
                    self.tool_specs.push(spec);
                }
            }
        }
        let (content, is_error, shown) = match res {
            Ok(v) => {
                self.failures.remove(&key);
                for ch in v["changed"].as_array().into_iter().flatten() {
                    if let Some(p) = ch["path"].as_str() {
                        self.rec.changed_files.insert(p.into());
                        let op = if ch["operation"].is_null() { &v["operation"] } else { &ch["operation"] };
                        self.emit(json!({ "type": "fileChanged", "path": p, "diff": ch["diff"], "operation": op }));
                    }
                }
                let ops = v["operations"].as_array().into_iter().flatten().chain(std::iter::once(&v["operation"]));
                self.rec.operations.extend(ops.filter_map(Value::as_u64).map(|o| o as usize));
                if VALIDATION_TOOLS.contains(&call.name.as_str()) {
                    let r = if v["result"].is_object() { &v["result"] } else { v };
                    let success = r["success"].as_bool().unwrap_or(false) && !r["timedOut"].as_bool().unwrap_or(false);
                    let cmd = r["argv"]
                        .as_array()
                        .or(v["command"].as_array())
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "));
                    let n_diag = v["diagnostics"].as_array().map(Vec::len);
                    let summary = match (success, n_diag) {
                        (_, Some(n)) => format!("{n} diagnostics"),
                        (true, None) => "passed".into(),
                        (false, None) => format!("failed (exit {})", r["exitCode"]),
                    };
                    self.rec.validations.push(ValidationRun { tool: call.name.clone(), command: cmd, success, summary });
                }
                // The chat shows the whole output; the model gets the errors and the end, and
                // read_log for the rest, so a noisy command does not ride along on every later turn.
                let mut for_model = v.clone();
                let shown = v.to_string();
                if tools::logs::shorten(&mut for_model, &format!("{}-{}", self.rec.id, call.id)) {
                    (for_model.to_string(), false, shown)
                } else {
                    (shown.clone(), false, shown)
                }
            }
            Err(e) => {
                *self.failures.entry(key).or_default() += 1;
                let r = errors::Error::from_tool(&call.name, e).render();
                (r.clone(), true, r)
            }
        };
        let content = truncate(&content, MAX_HISTORY_RESULT_CHARS);
        let content = if is_error { content } else { self.unchanged_read(call, &content).unwrap_or(content) };
        self.emit(json!({
            "type": "toolFinished", "callId": call.id, "tool": call.name, "ok": !is_error,
            "output": truncate(&shown, MAX_EVENT_OUTPUT_CHARS), "tokens": compact::chars_to_tokens(content.len()),
        }));
        ContentPart::ToolResult { tool_use_id: call.id.clone(), tool_name: Some(call.name.clone()), content, is_error }
    }

    /// Runs one response's tool calls. Consecutive read-only calls run concurrently; anything that
    /// writes or executes runs alone, in order, so writes never race.
    async fn run_calls(&mut self, calls: &[ToolCall]) -> Round {
        let mut results: Vec<Option<ContentPart>> = vec![None; calls.len()];
        let mut any_ok = false;
        // A round where every failure was a policy denial is not the model looping: nothing it does
        // to the input can help, and counting it towards `STUCK_ROUNDS` ended otherwise working tasks.
        let mut any_fixable = false;
        let mut note = |call: &ToolCall, res: &Result<Value, tools::ToolError>| match res {
            Ok(_) => any_ok = true,
            Err(e) => any_fixable |= errors::Error::from_tool(&call.name, e).severity != errors::Severity::Blocked,
        };
        let mut i = 0;
        let mut carry: Option<(usize, Target)> = None;
        while i < calls.len() || carry.is_some() {
            let mut batch: Vec<(usize, Target)> = carry.take().into_iter().collect();
            let solo = batch.first().is_some_and(|(_, t)| !t.parallel_safe());
            while !solo && i < calls.len() {
                let n = i;
                i += 1;
                match self.prepare(&calls[n]).await {
                    Err(e) => {
                        let res = Err(e);
                        note(&calls[n], &res);
                        results[n] = Some(self.record(&calls[n], &res));
                    }
                    Ok(t) if t.parallel_safe() => batch.push((n, t)),
                    Ok(t) => {
                        // A write or command runs alone, after the reads collected so far.
                        if batch.is_empty() {
                            batch.push((n, t));
                        } else {
                            carry = Some((n, t));
                        }
                        break;
                    }
                }
            }
            let started = Instant::now();
            let outs = join_all(batch.iter().map(|(n, t)| self.execute(t, &calls[*n]))).await;
            self.rec.metrics.tool_ms += started.elapsed().as_millis() as u64;
            for ((n, _), res) in batch.iter().zip(outs) {
                note(&calls[*n], &res);
                results[*n] = Some(self.record(&calls[*n], &res));
            }
            if self.cancel.is_cancelled() {
                break;
            }
        }
        let parts = results
            .into_iter()
            .zip(calls)
            .map(|(r, c)| {
                r.unwrap_or_else(|| ContentPart::ToolResult {
                    tool_use_id: c.id.clone(),
                    tool_name: Some(c.name.clone()),
                    content: errors::Error::cancelled().render(),
                    is_error: true,
                })
            })
            .collect();
        Round { parts, any_ok, any_fixable }
    }

    /// One last model turn without tools, so a run that hit a limit ends with a summary of its work.
    /// A failure here keeps the original status: the summary is a best effort.
    async fn synthesize(&mut self, limit: &str) {
        self.rec
            .messages
            .push(Message { role: Role::User, content: vec![ContentPart::Text { text: errors::Error::budget_exhausted(limit).render() }] });
        let Ok(out) = self.model_turn(false).await else { return };
        self.add_usage(out.usage);
        self.emit(json!({ "type": "usage", "usage": self.rec.usage, "costUsd": self.rec.metrics.cost_usd }));
        if out.text.is_empty() {
            return;
        }
        let text = out.text;
        self.rec.final_text = text.clone();
        self.emit(json!({ "type": "assistantMessage", "text": text, "toolCalls": 0 }));
        self.rec.messages.push(Message { role: Role::Assistant, content: vec![ContentPart::Text { text }] });
    }

    /// Starts a `run_command` call on its own and returns at once. When it ends, the chat row gets the
    /// real output (a second `toolFinished`) and the model gets a report before its next step.
    fn start_background(&self, def: &'static tools::ToolDef, call: &ToolCall) -> Result<Value, tools::ToolError> {
        // Dropping the receiver tells the command's task that nobody waits for it, so it reports.
        let (pending, _done) = self.launch(def, call)?;
        Ok(pending)
    }

    /// Starts a command in its own task. Returns what to tell the model if the command keeps running,
    /// plus the receiver that delivers the result when it ends first. Whoever still holds the
    /// receiver when the command ends gets the value; dropping it makes the task report the output
    /// as a message instead.
    #[allow(clippy::type_complexity)]
    fn launch(
        &self,
        def: &'static tools::ToolDef,
        call: &ToolCall,
    ) -> Result<(Value, tokio::sync::oneshot::Receiver<Result<Value, tools::ToolError>>), tools::ToolError> {
        let mut input = call.input.clone();
        if let Some(o) = input.as_object_mut() {
            o.remove("background");
        }
        // Stop on the row (`agent/cancelTool`) ends this command only; cancelling the task ends it too.
        let stop = self.cancel.child_token();
        let key = format!("{}:{}", self.rec.id, call.id);
        let cancels = self.host.tool_cancels.clone();
        cancels.lock().expect("tool cancels lock").insert(key.clone(), stop.clone());
        let ctx = self.ctx_with(stop.clone()).map_err(|e| tools::ToolError::new(ToolErrorKind::Failed, e))?;
        let command = command_text(&input["command"]);
        let (bg, emit, task, call_id, tool) =
            (self.background.clone(), self.host.emit.clone(), self.rec.id.clone(), call.id.clone(), call.name.clone());
        let task_cancel = self.cancel.clone();
        bg.running.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job = command.clone();
        tokio::spawn(async move {
            let res = tools::call(def, ctx, input).await;
            cancels.lock().expect("tool cancels lock").remove(&key);
            // Stopped from its own chat row: say so, rather than as a bare cancellation.
            let stopped = stop.is_cancelled() && !task_cancel.is_cancelled();
            let (ok, output) = match &res {
                _ if stopped => (false, json!({ "error": { "kind": "cancelled", "message": USER_STOPPED } }).to_string()),
                Ok(v) => (v["result"]["success"].as_bool().unwrap_or(true), v.to_string()),
                Err(e) => (false, json!({ "error": e }).to_string()),
            };
            // A dropped receiver means the caller moved on: the output becomes a message instead.
            if tx.send(if stopped { Err(tools::ToolError::new(ToolErrorKind::Cancelled, USER_STOPPED)) } else { res }).is_err() {
                emit(
                    &task,
                    json!({ "type": "toolFinished", "callId": call_id, "tool": tool, "ok": ok, "output": truncate(&output, MAX_EVENT_OUTPUT_CHARS), "background": true }),
                );
                let state = if ok { "finished" } else { "failed" };
                let report = format!(
                    "Background command `{job}` (call {call_id}) {state}. Its result:\n{}",
                    truncate(&output, MAX_BACKGROUND_REPORT_CHARS)
                );
                bg.done.lock().expect("background lock").push(report);
            }
            bg.running.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            bg.changed.notify_waiters();
        });
        let pending = json!({
            "background": true,
            "command": command,
            "message": format!(
                "`{command}` is still running, so it continues in the background beside your work. Its output comes to you as a message when it ends. Do not wait, sleep, or poll for it: continue with other work now."
            ),
        });
        Ok((pending, rx))
    }

    /// Runs a command while it is quick: its output comes back as the tool result. A command still
    /// running after `handoff` is left to the background, so a slow typecheck, build, or install never
    /// holds the loop: the agent keeps working and the output arrives as a message.
    async fn command_with(&self, def: &'static tools::ToolDef, call: &ToolCall, handoff: Duration) -> Result<Value, tools::ToolError> {
        let (pending, done) = self.launch(def, call)?;
        match tokio::time::timeout(handoff, done).await {
            Ok(Ok(res)) => res,
            // The timeout dropped the receiver, so the command reports when it ends.
            _ => Ok(pending),
        }
    }

    /// Waits until a background command ends (or the task is cancelled). False when none run.
    async fn wait_background(&self) -> bool {
        let bg = &self.background;
        loop {
            let changed = bg.changed.notified();
            if !bg.done.lock().expect("background lock").is_empty() {
                return true;
            }
            if bg.running.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return false;
            }
            tokio::select! {
                _ = changed => {}
                _ = self.cancel.cancelled() => return false,
            }
        }
    }

    /// Moves the steering messages and finished background reports into the conversation. Returns whether there were any.
    fn take_steers(&mut self) -> bool {
        let texts = self.host.steers.lock().expect("steers lock").get_mut(&self.rec.id).map(std::mem::take).unwrap_or_default();
        // Background reports are not shown as steers: their chat row already has the output.
        let reports = std::mem::take(&mut *self.background.done.lock().expect("background lock"));
        for (text, steered) in texts.iter().map(|t| (t, true)).chain(reports.iter().map(|t| (t, false))) {
            let part = ContentPart::Text { text: text.clone() };
            match self.rec.messages.last_mut() {
                Some(m) if matches!(m.role, Role::User) => m.content.push(part),
                _ => self.rec.messages.push(Message { role: Role::User, content: vec![part] }),
            }
            if steered {
                self.emit(json!({ "type": "steered", "text": text }));
            }
        }
        !texts.is_empty() || !reports.is_empty()
    }

    async fn run(mut self) -> TaskRecord {
        let deadline = Duration::from_secs(self.limits.deadline_secs.clamp(10, 24 * 3600));
        let mut failed_rounds = 0;
        // Final replies with no text and no tool call (a model that put the whole answer in its thinking).
        let mut empty_answers = 0;
        self.rec.status = Status::Running;
        self.save();
        let status = loop {
            if self.cancel.is_cancelled() {
                break Status::Cancelled;
            }
            if self.rec.iterations >= self.limits.max_iterations {
                break Status::MaxIterations;
            }
            if self.started.elapsed() > deadline {
                break Status::DeadlineExceeded;
            }
            if self.limits.max_total_tokens.is_some_and(|m| self.total_tokens() >= m) {
                break Status::BudgetExceeded;
            }
            self.take_steers();
            self.rec.iterations += 1;
            self.emit(json!({ "type": "iteration", "n": self.rec.iterations }));
            let out = match self.model_turn(true).await {
                Ok(o) => o,
                Err(e) if e.kind == ErrorKind::Cancelled => break Status::Cancelled,
                Err(e) => {
                    self.rec.error = Some(e.message.clone());
                    self.emit(json!({ "type": "error", "error": e }));
                    break Status::ProviderError;
                }
            };
            self.add_usage(out.usage);
            self.emit(json!({ "type": "usage", "usage": self.rec.usage, "costUsd": self.rec.metrics.cost_usd }));
            let mut content = Vec::new();
            if !out.text.is_empty() {
                content.push(ContentPart::Text { text: out.text.clone() });
                self.rec.final_text = out.text.clone();
            }
            content.extend(out.tool_calls.iter().map(|c| ContentPart::ToolUse {
                id: c.id.clone(),
                name: c.name.clone(),
                input: c.input.clone(),
            }));
            if content.is_empty() {
                content.push(ContentPart::Text { text: "(no response)".into() });
            }
            self.rec.messages.push(Message { role: Role::Assistant, content });
            self.emit(json!({ "type": "assistantMessage", "text": out.text, "toolCalls": out.tool_calls.len() }));
            if out.tool_calls.is_empty() {
                // A message that arrived during this model turn gets one more step instead of ending the task.
                if self.take_steers() {
                    continue;
                }
                // An empty reply is not an answer: ask for one, and after that a summary without tools,
                // so the task never ends with nothing to show.
                if out.text.trim().is_empty() {
                    empty_answers += 1;
                    if empty_answers <= MAX_EMPTY_ANSWERS {
                        self.emit(json!({ "type": "warning", "message": "The model replied with no text. task-x asks it for the answer." }));
                        self.rec.messages.push(Message { role: Role::User, content: vec![ContentPart::Text { text: EMPTY_ANSWER_NUDGE.into() }] });
                        continue;
                    }
                    self.rec.final_text.clear();
                    self.synthesize("The model gave no answer.").await;
                    if self.rec.final_text.is_empty() {
                        self.rec.error = Some("the model ended without an answer: its replies had no text".into());
                    }
                    self.save();
                    break Status::Completed;
                }
                // The task ends only after its background commands: their output can change what to do next.
                if self.background.running.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    self.emit(json!({ "type": "warning", "message": "Waiting for background commands to finish." }));
                    if self.wait_background().await && self.take_steers() {
                        continue;
                    }
                    if self.cancel.is_cancelled() {
                        break Status::Cancelled;
                    }
                }
                self.save();
                break Status::Completed;
            }
            self.rec.tool_calls += out.tool_calls.len() as u32;
            let round = self.run_calls(&out.tool_calls).await;
            self.rec.messages.push(Message { role: Role::User, content: round.parts });
            self.save();
            failed_rounds = if round.any_ok || !round.any_fixable { 0 } else { failed_rounds + 1 };
            if failed_rounds >= STUCK_ROUNDS {
                self.rec.error = Some(format!("every tool call failed in {STUCK_ROUNDS} consecutive rounds"));
                break Status::Stuck;
            }
        };
        let limit = match status {
            Status::MaxIterations => Some("The iteration limit was reached."),
            Status::BudgetExceeded => Some("The token budget was reached."),
            _ => None,
        };
        if let Some(limit) = limit {
            self.synthesize(limit).await;
        }
        self.rec.status = status;
        {
            let t = std::mem::take(&mut *self.sub_totals.lock().expect("subtotals lock"));
            let m = &mut self.rec.metrics;
            m.subagents += t.count;
            m.subagent_cost_usd += t.cost_usd;
            m.subagent_tokens += t.tokens;
            m.cost_usd += t.cost_usd;
            m.unpriced_tokens += t.unpriced;
        }
        self.rec.skipped_checks = self.skipped_checks();
        self.save();
        if !self.hooks.is_empty() {
            let ctx = self.hook_context(hooks::Event::AfterTask, None, summary(&self.rec), None);
            let (hl, cwd, c) = (self.hooks.clone(), self.cwd.clone(), CancellationToken::new());
            let _ = tokio::task::spawn_blocking(move || hooks::run(&hl, &ctx, &cwd, &c)).await;
        }
        self.emit(json!({ "type": "done", "summary": summary(&self.rec) }));
        self.rec
    }
}

pub fn summary(r: &TaskRecord) -> Value {
    json!({
        "taskId": r.id,
        "status": r.status,
        "chatMode": r.chat_mode,
        "text": r.final_text,
        "changedFiles": r.changed_files,
        "operations": r.operations,
        "validations": r.validations,
        "validationRan": !r.validations.is_empty(),
        "skippedChecks": r.skipped_checks,
        "usage": r.usage,
        "metrics": r.metrics,
        "model": r.model,
        "iterations": r.iterations,
        "toolCalls": r.tool_calls,
        "error": r.error,
    })
}

pub async fn run(host: Host, p: RunParams, cancel: CancellationToken) -> Result<TaskRecord, String> {
    let run = prepare(host.clone(), p, cancel).await?;
    let id = run.rec.id.clone();
    host.steers.lock().expect("steers lock").insert(id.clone(), Vec::new());
    let rec = run.run().await;
    host.steers.lock().expect("steers lock").remove(&id);
    Ok(rec)
}

/// What the model gets after the user stops one tool call. The task goes on.
pub const USER_STOPPED: &str = "Task has been stopped by user. Continue with the next step; do not run it again unless the user asks.";

/// Stops one running tool call (`agent/cancelTool`). The task gets an error result for it and goes on.
/// Returns `false` when no such call is running.
pub fn cancel_tool(tool_cancels: &Mutex<HashMap<String, CancellationToken>>, task_id: &str, call_id: &str) -> bool {
    tool_cancels.lock().expect("tool cancels lock").get(&format!("{task_id}:{call_id}")).inspect(|t| t.cancel()).is_some()
}

/// Queues a message for a running task (`agent/steer`). It joins the conversation at the task's next step.
/// Returns `false` when no such task is running or its queue is full.
pub fn steer(steers: &Steers, task_id: &str, text: &str) -> Result<bool, String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > MAX_STEER_CHARS {
        return Err(format!("steering text must be 1..={MAX_STEER_CHARS} characters"));
    }
    let mut map = steers.lock().expect("steers lock");
    Ok(match map.get_mut(task_id) {
        Some(queue) if queue.len() < MAX_STEERS => {
            queue.push(text.to_owned());
            true
        }
        _ => false,
    })
}

/// Changes a message still waiting for a running task (`agent/editSteer`): the first one equal to `old`
/// gets `text`, or is removed when `text` is empty. Returns `false` when the task already read it.
pub fn edit_steer(steers: &Steers, task_id: &str, old: &str, text: &str) -> Result<bool, String> {
    let text = text.trim();
    if text.chars().count() > MAX_STEER_CHARS {
        return Err(format!("steering text must be at most {MAX_STEER_CHARS} characters"));
    }
    let mut map = steers.lock().expect("steers lock");
    let Some(queue) = map.get_mut(task_id) else { return Ok(false) };
    let Some(i) = queue.iter().position(|s| s == old.trim()) else { return Ok(false) };
    if text.is_empty() {
        queue.remove(i);
    } else {
        queue[i] = text.to_owned();
    }
    Ok(true)
}

/// Builds a task (prompt enrichment, tools, skills, hooks, record) without running the loop.
async fn prepare(host: Host, p: RunParams, cancel: CancellationToken) -> Result<Run, String> {
    let prompt_text = p.prompt.trim();
    if prompt_text.is_empty() || prompt_text.len() > 200_000 {
        return Err("prompt must be 1..=200000 characters".into());
    }
    p.provider.resolve().map_err(|e| e.message)?;
    let mut policy_cfg = p.policy.clone();
    if !p.chat_mode.writes() {
        policy_cfg.mode = Mode::ReadOnly;
    }
    let mut policy = Policy::new(policy_cfg.clone());
    let policy_cfg_keep = policy_cfg.clone();
    for g in host.grants.lock().expect("grants lock").iter() {
        policy.grant_session(g);
    }
    let enriched: Enriched = {
        let ws = host.workspace.read().expect("workspace lock");
        prompt::enrich(
            ws.as_ref(),
            &Inputs {
                request: prompt_text,
                editor: &p.editor,
                budget_tokens: p.limits.context_budget_tokens.clamp(500, 100_000),
                mode_note: &mode_note(p.chat_mode, &policy_cfg),
            },
        )
    };
    let has_ws = host.workspace.read().expect("workspace lock").is_some();
    let mut provider = p.provider.clone();
    let mut route_note = None;
    if let Some(r) = p.routes.iter().find(|r| r.kinds.contains(&enriched.kind) && r.provider.resolve().is_ok()) {
        provider = r.provider.clone();
        route_note = Some(
            json!({ "type": "model", "kind": provider.kind, "model": provider.model, "reason": format!("route for {:?} tasks", enriched.kind) }),
        );
    }
    let fallbacks: Vec<ProviderConfig> = p.fallbacks.iter().filter(|f| f.resolve().is_ok()).cloned().collect();
    let root = host.workspace.read().expect("workspace lock").as_ref().map(|w| w.index.root().to_path_buf());
    let mut focus: Vec<String> = enriched.sources.iter().map(|s| s.path.clone()).collect();
    focus.extend(p.editor.active_file.iter().cloned());
    let all_skills = skills::load(root.as_deref(), &skills::user_dirs());
    let active_skills = skills::select(&all_skills, prompt_text, &focus);
    let skill_index = skills::index(&all_skills, &active_skills);
    let skill_tools: HashSet<&str> = active_skills.iter().flat_map(|s| s.tools.iter().map(String::as_str)).collect();
    let mut tool_specs = if has_ws {
        tools::specs(|t| {
            policy.decide(t.name, t.permission) != Decision::Deny
                && (skill_tools.is_empty() || t.permission == Permission::Read || skill_tools.contains(t.name))
        })
    } else {
        Vec::new()
    };
    let mut mcp_map = HashMap::new();
    for t in host.mcp.read().await.tools() {
        if policy.decide(&t.qualified, Permission::Network) == Decision::Deny
            || (!skill_tools.is_empty() && !skill_tools.contains(t.qualified.as_str()))
        {
            continue;
        }
        if let Some((c, tool)) = host.mcp.read().await.find(&t.qualified) {
            tool_specs.push(crate::provider::ToolSpec {
                name: t.qualified.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            });
            mcp_map.insert(t.qualified.clone(), (c, tool));
        }
    }
    let mut system = format!("{}\n\n{}", enriched.system, p.chat_mode.guidance());
    if let Some(ix) = &skill_index {
        system.push_str("\n\n");
        system.push_str(ix);
        tool_specs.push(crate::provider::ToolSpec {
            name: READ_SKILL.into(),
            description: "Load the full instructions of a skill listed in <available_skills>.".into(),
            input_schema: json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"], "additionalProperties": false }),
        });
    }
    let is_git = root.as_deref().is_some_and(|r| r.join(".git").exists());
    let team_ok = p.depth == 0 && is_git && p.chat_mode.writes() && policy.decide("run_subagent", Permission::Write) != Decision::Deny;
    if team_ok {
        tool_specs.extend(team_specs());
        if p.chat_mode == ChatMode::Team && policy.decide("run_plan", Permission::Write) != Decision::Deny {
            tool_specs.push(plan_spec());
        }
    }
    if p.depth == 0 {
        tool_specs.push(ask_spec());
    }
    if has_ws {
        tool_specs.push(summarize_spec());
    }
    let embeddings = p.embeddings.clone().filter(|e| e.resolve().is_ok());
    if has_ws && embeddings.is_some() {
        tool_specs.push(semantic_spec());
    }
    if host.store.data_dir().is_some_and(|d| d.join("docs").is_dir()) {
        tool_specs.push(docs_spec());
    }
    // Lazy tool list: the core tools, the task-x tools that are not registry tools (subagents,
    // questions, skills, summaries), and the tools a skill needs; everything else waits behind
    // find_tool. MCP tools wait too: their schemas are often the largest.
    let mut deferred = Vec::new();
    if p.limits.lazy_tools {
        let keep = |t: &crate::provider::ToolSpec| {
            CORE_TOOLS.contains(&t.name.as_str())
                || (tools::find(&t.name).is_none() && !t.name.starts_with("mcp__"))
                || skill_tools.contains(t.name.as_str())
        };
        let (now, later): (Vec<_>, Vec<_>) = tool_specs.into_iter().partition(keep);
        tool_specs = now;
        deferred = later;
        if !deferred.is_empty() {
            tool_specs.push(find_tool_spec(&deferred));
        }
    }
    if let Some(r) = root.as_deref() {
        let memory = tools::memory::prompt_block(r);
        if !memory.is_empty() {
            system.push_str("\n\n");
            system.push_str(&memory);
        }
    }
    // The names and parameters in plain words, so a provider that drops `tools` (or a model that
    // ignores the schema) still knows what it may call and with which fields.
    let guide = tool_guide(&tool_specs);
    if !guide.is_empty() {
        system.push_str("\n\n");
        system.push_str(&guide);
    }
    let team_warning = (p.chat_mode == ChatMode::Team && !tool_specs.iter().any(|t| t.name == "run_plan"))
        .then_some("Team mode needs a git repository (with a commit) and permission for run_plan; running as a single agent.");
    let skill_bodies: HashMap<String, String> = all_skills.iter().map(|s| (s.name.clone(), s.body.clone())).collect();
    for s in &active_skills {
        system.push_str(&format!("\n\n<skill name=\"{}\" reason=\"{}\">\n{}\n</skill>", s.name, s.reason, s.text));
    }
    let hook_list = host.hooks.read().expect("hooks lock").clone();
    let cwd = root.clone().unwrap_or_else(std::env::temp_dir);
    if !hook_list.is_empty() {
        let (hl, cwd2, c2) = (hook_list.clone(), cwd.clone(), cancel.clone());
        let mut ctx = hooks::HookContext::new(hooks::Event::BeforeTask, None, json!({ "prompt": prompt_text, "chatMode": p.chat_mode }));
        (ctx.budget_used, ctx.budget_limit) = (Some(0), p.limits.max_total_tokens);
        let outs = tokio::task::spawn_blocking(move || hooks::run(&hl, &ctx, &cwd2, &c2)).await.unwrap_or_default();
        if let Some(o) = outs.iter().find(|o| o.blocked) {
            return Err(format!("blocked by a beforeTask hook ({}): {}", o.command, o.message()));
        }
    }
    let mut user_content = vec![ContentPart::Text { text: enriched.user.clone() }];
    user_content.extend(crate::provider::types::image_parts(&p.images)?);
    let user_msg = Message { role: Role::User, content: user_content };

    let mut rec = match p.task_id.as_deref().and_then(|id| host.store.load(id)) {
        Some(mut r) => {
            // A follow-up keeps the earlier conversation; the newest system prompt applies. Only a
            // plain prompt is replaced: a turn holding tool results must stay, or the assistant's
            // tool calls lose their answers and the provider rejects the whole request.
            let unanswered = r
                .messages
                .last()
                .is_some_and(|m| m.role == Role::User && m.content.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })));
            if unanswered {
                // The interrupted run's results stay; the new request joins them in the same turn.
                if let Some(m) = r.messages.last_mut() {
                    m.content.extend(user_msg.content);
                }
            } else {
                if r.messages.last().is_some_and(|m| m.role == Role::User) {
                    r.messages.pop();
                }
                r.messages.push(user_msg);
            }
            r.system = system;
            r.chat_mode = p.chat_mode;
            r.final_text.clear();
            r.error = None;
            r
        }
        None => TaskRecord {
            version: store::RECORD_VERSION,
            id: p.task_id.filter(|i| store::valid_id(i)).unwrap_or_else(store::new_id),
            title: truncate(prompt_text.lines().next().unwrap_or(prompt_text), 80),
            created_ms: store::now_ms(),
            updated_ms: store::now_ms(),
            status: Status::Running,
            chat_mode: p.chat_mode,
            provider_kind: format!("{:?}", p.provider.kind),
            model: provider.model.clone(),
            system,
            messages: vec![user_msg],
            usage: Usage::default(),
            iterations: 0,
            tool_calls: 0,
            changed_files: Default::default(),
            operations: Vec::new(),
            validations: Vec::new(),
            final_text: String::new(),
            error: None,
            enrichment: None,
            metrics: Metrics::default(),
            skipped_checks: Vec::new(),
        },
    };
    rec.enrichment = Some(store::enrichment_json(&enriched));
    rec.model = provider.model.clone();
    if let Some(n) = route_note {
        (host.emit)(&rec.id, n);
    }
    if let Some(w) = team_warning {
        (host.emit)(&rec.id, json!({ "type": "warning", "message": w }));
    }
    (host.emit)(
        &rec.id,
        json!({ "type": "started", "chatMode": p.chat_mode, "enrichment": enriched, "tools": tool_specs.iter().map(|t| t.name.clone()).collect::<Vec<_>>(), "skills": active_skills }),
    );
    Ok(Run {
        host,
        rec,
        policy,
        cancel,
        limits: p.limits,
        provider,
        tool_specs,
        failures: HashMap::new(),
        started: Instant::now(),
        mcp: mcp_map,
        hooks: hook_list,
        cwd,
        fallbacks,
        skills: skill_bodies,
        depth: p.depth,
        policy_cfg: policy_cfg_keep,
        worktrees: Mutex::default(),
        sub_count: Default::default(),
        sub_totals: Mutex::default(),
        token_ratio: 1.0,
        prices: p.prices,
        role_providers: p.role_providers,
        background: Arc::default(),
        reads: HashMap::new(),
        deferred,
        helper: p.helper.filter(|h| h.resolve().is_ok()),
        embeddings,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BestOfParams {
    pub prompt: String,
    /// The models to compare (2 to 4); each one works in its own worktree.
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub editor: EditorContext,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub prices: Vec<cost::PriceRule>,
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Best-of-N: the same task runs on several models at once, each in its own worktree, and the
/// candidates come back for the user to compare. Nothing is merged; the client applies the chosen
/// worktree (`worktrees/apply`) and discards the others.
pub async fn best_of(host: Host, p: BestOfParams, cancel: CancellationToken) -> Result<Value, String> {
    if !(2..=4).contains(&p.providers.len()) {
        return Err("best-of-N needs 2 to 4 providers".into());
    }
    for pr in &p.providers {
        pr.resolve().map_err(|e| e.message)?;
    }
    let params = RunParams {
        prompt: p.prompt.clone(),
        images: Vec::new(),
        provider: p.providers[0].clone(),
        chat_mode: ChatMode::Agent,
        policy: p.policy,
        editor: p.editor,
        limits: p.limits,
        task_id: p.task_id,
        routes: Vec::new(),
        fallbacks: Vec::new(),
        prices: p.prices,
        role_providers: Vec::new(),
        helper: None,
        embeddings: None,
        depth: 0,
    };
    let lead = prepare(host, params, cancel).await?;
    if !lead.cwd.join(".git").exists() {
        return Err("best-of-N needs a git repository with at least one commit".into());
    }
    let objective = format!("{}\n\nWhen the change is done, run the project's tests or build and fix what fails.", p.prompt.trim());
    let runs = p.providers.iter().enumerate().map(|(i, pr)| {
        let call = ToolCall {
            id: format!("best-{}", i + 1),
            name: "run_subagent".into(),
            input: json!({ "role": "implementation", "objective": objective, "name": lead.step_worktree(&format!("try{}", i + 1)) }),
        };
        let pr = pr.clone();
        let lead = &lead;
        async move { lead.spawn_sub(team::Role::Implementation, &call, &[], Some(pr), Some(&call.id)).await }
    });
    let outs = join_all(runs).await;
    let candidates: Vec<Value> = outs
        .into_iter()
        .zip(&p.providers)
        .map(|(o, pr)| match o {
            Ok((v, patch)) => {
                let patch = patch.unwrap_or_default();
                let (added, removed) = team::patch_stat(&patch);
                json!({
                    "name": v["worktree"], "kind": pr.kind, "model": pr.model, "status": v["status"], "report": v["report"],
                    "files": team::patch_paths(&patch), "added": added, "removed": removed, "validations": v["validations"],
                    "costUsd": v["costUsd"], "tokens": v["tokens"], "dir": v["dir"], "patch": truncate(&patch, MAX_CANDIDATE_PATCH_CHARS),
                })
            }
            Err(e) => json!({ "name": null, "kind": pr.kind, "model": pr.model, "status": "failed", "error": e }),
        })
        .collect();
    let total: f64 = candidates.iter().filter_map(|c| c["costUsd"].as_f64()).sum();
    lead.emit(json!({ "type": "bestOfDone", "candidates": candidates.len(), "costUsd": total }));
    Ok(json!({ "taskId": lead.rec.id, "candidates": candidates, "costUsd": total }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::index::tests::temp_repo;
    use crate::provider::ProviderKind;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// Minimal OpenAI-compatible mock that replays one SSE body per request, in order.
    fn mock(bodies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            let mut bodies = bodies.into_iter();
            for conn in l.incoming() {
                let mut c = conn.unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut request = String::new();
                r.read_line(&mut request).unwrap();
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                // The model listing is not mocked: it answers 404 and the runtime uses its own window estimate.
                if request.starts_with("GET ") {
                    write!(c, "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).unwrap();
                s2.lock().unwrap().push(serde_json::from_slice(&buf).unwrap());
                let Some(body) = bodies.next() else { break };
                write!(c, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}").unwrap();
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    fn sse_tool(id: &str, name: &str, args: Value) -> String {
        let chunk = json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": id, "function": { "name": name, "arguments": args.to_string() } }] }, "finish_reason": "tool_calls" }] });
        format!("data: {chunk}\n\ndata: [DONE]\n\n")
    }

    fn sse_text(t: &str) -> String {
        format!("data: {}\n\ndata: [DONE]\n\n", json!({ "choices": [{ "index": 0, "delta": { "content": t }, "finish_reason": "stop" }] }))
    }

    /// Text with the token counts a real server reports, for the tests that check where tokens go.
    fn sse_text_usage(t: &str, prompt: u64, completion: u64) -> String {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({ "choices": [{ "index": 0, "delta": { "content": t }, "finish_reason": "stop" }] }),
            json!({ "choices": [], "usage": { "prompt_tokens": prompt, "completion_tokens": completion } })
        )
    }

    fn host(root: &std::path::Path, decision: ApprovalDecision, events: Arc<Mutex<Vec<Value>>>) -> Host {
        let ws = Workspace::open(root, &[]).unwrap();
        Host {
            providers: Providers::new().unwrap(),
            workspace: Arc::new(RwLock::new(Some(ws))),
            seen: Arc::default(),
            edits: Arc::default(),
            grants: Arc::default(),
            store: Store::new(Some(&root.join(".anvil-data"))),
            emit: Arc::new(move |_, e| events.lock().unwrap().push(e)),
            approve: Arc::new(move |_, _| Box::pin(async move { decision })),
            ask: Arc::new(|_: AskRequest, _: CancellationToken| Box::pin(async { None::<Answers> })),
            mcp: Arc::default(),
            hooks: Arc::default(),
            subagents: Arc::default(),
            steers: Arc::default(),
            tool_cancels: Arc::default(),
        }
    }

    fn params(base: &str, mode: ChatMode, policy: Mode) -> RunParams {
        RunParams {
            prompt: "Change the greeting to hello".into(),
            images: Vec::new(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAiCompatible,
                model: "m".into(),
                base_url: Some(base.into()),
                api_key: Some("k".into()),
                timeout_ms: Some(5_000),
            },
            chat_mode: mode,
            policy: PolicyConfig { mode: policy, ..Default::default() },
            editor: EditorContext::default(),
            limits: Limits::default(),
            task_id: None,
            routes: Vec::new(),
            helper: None,
            embeddings: None,
            fallbacks: Vec::new(),
            prices: Vec::new(),
            role_providers: Vec::new(),
            depth: 0,
        }
    }

    #[tokio::test]
    async fn agent_asks_the_user_and_gets_only_offered_answers() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![
            sse_tool(
                "c1",
                "ask_user",
                json!({ "questions": [
                { "question": "Which indent?", "type": "radio", "options": ["tabs", "spaces"] },
                { "question": "Which checks?", "type": "checkbox", "options": ["lint", "test", "build"] }
            ] }),
            ),
            sse_text("Using tabs."),
        ]);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut h = host(&root, ApprovalDecision::Allow, Arc::default());
        let log = asked.clone();
        h.ask = Arc::new(move |req: AskRequest, _: CancellationToken| {
            log.lock().unwrap().push(req.questions.len());
            Box::pin(async {
                Some(Answers {
                    selected: HashMap::from([
                        ("q1".to_string(), vec!["tabs".to_string(), "spaces".to_string()]),
                        ("q2".to_string(), vec!["lint".to_string(), "bogus".to_string()]),
                    ]),
                    other: HashMap::from([("q2".to_string(), "  also fmt  ".to_string()), ("q1".to_string(), "   ".to_string())]),
                })
            })
        });
        let r = run(h, params(&base, ChatMode::Agent, Mode::AskBeforeEdit), CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert_eq!(*asked.lock().unwrap(), vec![2]);
        let reqs = seen.lock().unwrap();
        let result = reqs[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap()["content"].to_string();
        assert!(result.contains("tabs") && !result.contains("spaces"), "radio keeps one choice");
        assert!(result.contains("lint") && !result.contains("bogus"), "unoffered options are dropped");
        assert!(result.contains(r#"\"other\":\"also fmt\""#), "typed text is trimmed and kept: {result}");
        assert_eq!(result.matches("other").count(), 1, "blank text is dropped");
        std::fs::remove_dir_all(root).ok();
    }

    /// The approval card shows what the call will do, without a raw JSON dump.
    #[test]
    fn approval_previews_cover_the_editing_tools() {
        assert!(preview("web_fetch", &json!({ "url": "https://example.com/a" })).unwrap().contains("https://example.com/a"));
        let batch = preview(
            "edit_files",
            &json!({ "edits": [
                { "path": "a.rs", "oldText": "x", "newText": "y" },
                { "path": "a.rs", "oldText": "y", "newText": "z" },
                { "path": "b.rs", "oldText": "p", "newText": "q" }
            ]}),
        )
        .unwrap();
        assert!(batch.starts_with("3 change(s) in 2 file(s)") && batch.contains("a.rs") && batch.contains("b.rs"), "{batch}");
        assert!(preview("insert_at_line", &json!({ "path": "a.rs", "line": 3, "text": "new" })).unwrap().contains("before line 3"));
        assert!(preview("delete_range", &json!({ "path": "a.rs", "start": 2, "end": 4 })).unwrap().contains("lines 2–4"));
        assert!(preview("read_file", &json!({ "path": "a.rs" })).is_none(), "reads need no approval card");
    }

    /// A record as a stopped run leaves it: resumable, with the given history.
    fn saved_record(id: &str, messages: Vec<Message>) -> store::TaskRecord {
        store::TaskRecord {
            version: store::RECORD_VERSION,
            id: id.into(),
            title: "earlier".into(),
            created_ms: 0,
            updated_ms: 0,
            status: Status::Cancelled,
            chat_mode: ChatMode::Agent,
            provider_kind: "OpenAiCompatible".into(),
            model: "m".into(),
            system: "s".into(),
            messages,
            usage: Usage::default(),
            iterations: 1,
            tool_calls: 1,
            changed_files: Default::default(),
            operations: Vec::new(),
            validations: Vec::new(),
            final_text: String::new(),
            error: None,
            enrichment: None,
            metrics: Metrics::default(),
            skipped_checks: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_follow_up_keeps_the_tool_results_of_an_interrupted_run() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let h = host(&root, ApprovalDecision::Allow, Arc::default());
        // A run that stopped after its tool calls leaves the results as the last turn.
        let saved = saved_record(
            "t1",
            vec![
                Message { role: Role::User, content: vec![ContentPart::Text { text: "read a.txt".into() }] },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolUse { id: "c1".into(), name: "read_file".into(), input: json!({ "path": "a.txt" }) }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentPart::ToolResult {
                        tool_use_id: "c1".into(),
                        tool_name: Some("read_file".into()),
                        content: "x".into(),
                        is_error: false,
                    }],
                },
            ],
        );
        h.store.save(&saved).unwrap();
        let mut p = params("http://127.0.0.1:1/v1", ChatMode::Agent, Mode::AskBeforeEdit);
        p.task_id = Some("t1".into());
        let r = prepare(h, p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.rec.messages.len(), 3, "the follow-up replaces only a plain prompt");
        assert!(matches!(r.rec.messages[2].content[0], ContentPart::ToolResult { .. }), "the tool result stays");
        assert!(
            r.rec.messages[2].content.iter().any(|c| matches!(c, ContentPart::Text { text } if text.contains("Change the greeting"))),
            "the new request joins the same turn"
        );
        assert_eq!(repair_history(&mut r.rec.messages.clone()), 0, "the history is valid as saved");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn agent_reads_edits_and_completes() {
        let root = temp_repo(&[("greet.txt", "hi\n")]);
        let (base, seen) = mock(vec![
            sse_tool("c1", "read_file", json!({ "path": "greet.txt" })),
            sse_tool("c2", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("Changed greet.txt."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let r = run(
            host(&root, ApprovalDecision::Allow, events.clone()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hello\n");
        assert_eq!(r.changed_files.iter().collect::<Vec<_>>(), ["greet.txt"]);
        assert!(r.validations.is_empty(), "no validation ran, and the summary must say so");
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| e["type"] == "approvalRequested" && e["tool"] == "replace_text"));
        assert!(!ev.iter().any(|e| e["type"] == "approvalRequested" && e["tool"] == "read_file"));
        let reqs = seen.lock().unwrap();
        assert!(reqs[0]["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "apply_patch"));
        assert_eq!(reqs[2]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count(), 2);
        assert!(Store::new(Some(&root.join(".anvil-data"))).load(&r.id).is_some());
        std::fs::remove_dir_all(root).ok();
    }

    /// Servers that number two calls 0 used to merge them into one call whose arguments were two
    /// JSON documents, which ended the run with "invalid tool arguments: trailing characters".
    #[tokio::test]
    async fn two_tool_calls_that_share_one_index_both_run() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let frag = |id: &str, path: &str| {
            let args = json!({ "path": path }).to_string();
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "id": id, "function": { "name": "read_file", "arguments": args } }
            ] } }] })
        };
        let two = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", frag("c1", "a.txt"), frag("c2", "a.txt"));
        let (base, seen) = mock(vec![two, sse_text("Read a.txt twice.")]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        assert_eq!(r.tool_calls, 2, "both calls ran");
        let reqs = seen.lock().unwrap();
        let calls = reqs[1]["messages"][2]["tool_calls"].as_array().expect("the assistant turn lists both calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(reqs[1]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count(), 2);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_waiting_steer_can_be_edited_or_removed_until_it_is_read() {
        let steers = Steers::default();
        steers.lock().unwrap().insert("t".into(), Vec::new());
        assert!(steer(&steers, "t", "add a test").unwrap() && steer(&steers, "t", "and docs").unwrap());
        assert!(edit_steer(&steers, "t", "add a test", "add two tests").unwrap());
        assert!(edit_steer(&steers, "t", "and docs", "  ").unwrap(), "empty text removes it");
        assert_eq!(steers.lock().unwrap()["t"], ["add two tests"]);
        assert!(!edit_steer(&steers, "t", "and docs", "x").unwrap(), "a message already gone cannot change");
        assert!(!edit_steer(&steers, "other", "add two tests", "x").unwrap());
    }

    #[tokio::test]
    async fn steering_joins_the_running_task_and_is_refused_once_it_ends() {
        let root = temp_repo(&[("greet.txt", "hi\n")]);
        let (base, seen) = mock(vec![
            sse_tool("c1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("Done."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut h = host(&root, ApprovalDecision::Allow, events.clone());
        let steers = h.steers.clone();
        let inbox = steers.clone();
        // The approval prompt runs while the task is busy, so the steer arrives mid-task.
        h.approve = Arc::new(move |_, _| {
            let inbox = inbox.clone();
            Box::pin(async move {
                assert!(steer(&inbox, "steer-task", "also add a test").unwrap());
                ApprovalDecision::Allow
            })
        });
        let mut p = params(&base, ChatMode::Agent, Mode::AskBeforeEdit);
        p.task_id = Some("steer-task".into());
        let r = run(h, p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert!(seen.lock().unwrap()[1]["messages"].to_string().contains("also add a test"));
        assert!(events.lock().unwrap().iter().any(|e| e["type"] == "steered" && e["text"] == "also add a test"));
        assert!(!steer(&steers, "steer-task", "too late").unwrap(), "a finished task refuses steering");
        assert!(steer(&steers, "steer-task", "   ").is_err());
        std::fs::remove_dir_all(root).ok();
    }

    /// A slow command must not hold the turn: after the hand-off it keeps running beside the agent
    /// and its output arrives as a message, like an explicit `background` command.
    #[tokio::test]
    async fn a_slow_command_is_handed_to_the_background() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let run = prepare(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params("http://127.0.0.1:1/v1", ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let def = tools::find("run_command").unwrap();
        let call = ToolCall {
            id: "slow".into(),
            name: "run_command".into(),
            input: json!({ "command": ["sh", "-c", "sleep 2; echo typecheck-done"] }),
        };
        let started = Instant::now();
        // A 100 ms hand-off stands in for the real 30 s window.
        let out = run.command_with(def, &call, Duration::from_millis(100)).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(1), "the loop was not held for {:?}", started.elapsed());
        assert_eq!(out["background"], true, "{out}");
        assert!(out["message"].as_str().unwrap().contains("continues in the background"));
        assert!(out.get("result").is_none(), "no output yet");

        // The command still runs, and its output reaches the model as a report.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(report) = run.background.done.lock().unwrap().pop() {
                assert!(report.contains("typecheck-done"), "{report}");
                break;
            }
            assert!(Instant::now() < deadline, "no report arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(run.background.running.load(std::sync::atomic::Ordering::SeqCst), 0, "nothing is left running");
        std::fs::remove_dir_all(root).ok();
    }

    /// A quick command still returns its output as the tool result, exactly as before.
    #[tokio::test]
    async fn a_quick_command_returns_its_output() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let run = prepare(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params("http://127.0.0.1:1/v1", ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let def = tools::find("run_command").unwrap();
        let call = ToolCall { id: "fast".into(), name: "run_command".into(), input: json!({ "command": ["echo", "hi"] }) };
        let out = run.command_with(def, &call, COMMAND_HANDOFF).await.unwrap();
        assert_eq!(out["result"]["stdout"], "hi\n", "{out}");
        assert!(out.get("background").is_none());
        assert!(run.background.done.lock().unwrap().is_empty(), "no report for a command that finished");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn background_commands_report_back_before_the_task_ends() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let cmd = json!({ "command": ["sh", "-c", "sleep 1; echo bg-done"], "background": true });
        let (base, seen) =
            mock(vec![sse_tool("c1", "run_command", cmd), sse_text("Started it; nothing else to do."), sse_text("The install finished.")]);
        let h = host(&root, ApprovalDecision::Allow, Arc::default());
        let started = Instant::now();
        let r = run(h, params(&base, ChatMode::Agent, Mode::AskBeforeEdit), CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert!(started.elapsed() >= Duration::from_secs(1), "the task waited for the command");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "one more step after the report");
        let first = seen[1]["messages"].to_string();
        assert!(first.contains("continues in the background"), "the call returned at once");
        assert!(!first.contains("exitCode"), "the command had not finished, so no output is in the step");
        assert!(seen[2]["messages"].to_string().contains("bg-done"), "the output came as a message");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn stopping_a_command_keeps_the_task_running() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) =
            mock(vec![sse_tool("c1", "run_command", json!({ "command": ["sleep", "30"] })), sse_text("Skipped the slow command.")]);
        let h = host(&root, ApprovalDecision::Allow, Arc::default());
        let cancels = h.tool_cancels.clone();
        let stopper = tokio::spawn(async move {
            while !cancel_tool(&cancels, "stop-task", "c1") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let mut p = params(&base, ChatMode::Agent, Mode::AskBeforeEdit);
        p.task_id = Some("stop-task".into());
        let started = Instant::now();
        let r = run(h.clone(), p, CancellationToken::new()).await.unwrap();
        stopper.await.unwrap();
        assert_eq!(r.status, Status::Completed, "the task goes on after the command stops");
        assert!(started.elapsed() < Duration::from_secs(20), "the command did not run to the end");
        assert!(seen.lock().unwrap()[1]["messages"].to_string().contains("Task has been stopped by user"));
        assert!(!cancel_tool(&h.tool_cancels, "stop-task", "c1"), "a finished call cannot be stopped");
        std::fs::remove_dir_all(root).ok();
    }

    /// A hook that blocks a call must stop it before the user is asked to approve it.
    #[tokio::test]
    async fn a_blocking_hook_runs_before_the_approval_prompt() {
        let root = temp_repo(&[("greet.txt", "hi\n")]);
        let (base, seen) = mock(vec![
            sse_tool("c1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("The hook blocks edits here."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let h = host(&root, ApprovalDecision::Allow, events.clone());
        *h.hooks.write().unwrap() = vec![hooks::Hook {
            event: hooks::Event::BeforeEdit,
            command: vec!["sh".into(), "-c".into(), "echo 'edits are frozen in this repo' >&2; exit 1".into()],
            matcher: None,
            timeout_ms: Some(5_000),
        }];
        let r = run(h, params(&base, ChatMode::Agent, Mode::AskBeforeEdit), CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hi\n", "the edit never ran");
        let asked = events.lock().unwrap().iter().any(|e| e["type"] == "approvalRequested");
        assert!(!asked, "the user is not asked to approve a call a hook already blocks");
        let sent = seen.lock().unwrap()[1].to_string();
        assert!(sent.contains("Blocked by a BeforeEdit hook"), "{sent}");
        assert!(sent.contains("edits are frozen"), "the hook's reason reaches the model");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn ask_mode_is_read_only_and_denials_reach_the_model() {
        let root = temp_repo(&[("greet.txt", "hi\n")]);
        let (base, seen) = mock(vec![
            sse_tool("c1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "x" })),
            sse_text("I can't edit in Ask mode."),
        ]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Ask, Mode::Autonomous),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hi\n");
        let reqs = seen.lock().unwrap();
        assert!(!reqs[0]["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "replace_text"));
        let tool_msg = reqs[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
        assert!(tool_msg["content"].as_str().unwrap().contains("not available in this mode"));
        std::fs::remove_dir_all(root).ok();
    }

    /// A tool the mode does not offer is out of reach whatever the model sends: three such rounds
    /// must not end a task that is otherwise fine.
    #[tokio::test]
    async fn a_tool_the_mode_does_not_offer_does_not_count_as_being_stuck() {
        let root = temp_repo(&[("a.txt", "x\n"), ("b.txt", "y\n"), ("c.txt", "z\n")]);
        // `delete_file` is destructive, so the autonomous policy keeps it out of the tool list.
        // Three different calls: an identical repeat would be stopped by the repeated-failure guard.
        let (base, seen) = mock(vec![
            sse_tool("c1", "delete_file", json!({ "path": "a.txt" })),
            sse_tool("c2", "delete_file", json!({ "path": "b.txt" })),
            sse_tool("c3", "delete_file", json!({ "path": "c.txt" })),
            sse_text("I cannot delete files here; the policy blocks it."),
        ]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Agent, Mode::Autonomous),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "unavailable tools alone are not being stuck: {:?}", r.error);
        assert_eq!(r.iterations, 4, "the model got a turn after each refusal");
        for f in ["a.txt", "b.txt", "c.txt"] {
            assert!(root.join(f).exists(), "{f} is still there");
        }
        let sent = seen.lock().unwrap().last().unwrap().to_string();
        assert!(sent.contains("not available in this mode"), "the model is told why: {sent}");
        std::fs::remove_dir_all(root).ok();
    }

    /// A policy denial at call time is reported as blocked, not as a failure to retry.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_policy_denial_is_blocked_and_the_task_goes_on() {
        let root = git_repo(&[("greet.txt", "hi\n")]);
        let (base, seen) = mock(vec![
            sse_tool("p1", "run_subagent", json!({ "role": "implementation", "objective": "say hello", "name": "hello" })),
            sse_tool("s1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("Changed greet.txt in the worktree."),
            // Denied by the user's own deny list, not by the tool's existence. Different names, so the
            // repeated-failure guard (which is a real "you are looping" signal) does not apply.
            sse_tool("p2", "worktree_discard", json!({ "name": "hello" })),
            sse_tool("p3", "worktree_discard", json!({ "name": "hello-2" })),
            sse_tool("p4", "worktree_discard", json!({ "name": "hello-3" })),
            sse_text("The deny list blocks discarding, so I left the worktree in place."),
        ]);
        let mut p = params(&base, ChatMode::Agent, Mode::Autonomous);
        p.policy.deny = vec!["worktree_discard".into()];
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed, "three denials are not being stuck: {:?}", r.error);
        let sent = seen.lock().unwrap().last().unwrap().to_string();
        assert!(sent.contains("blocked by the permission policy"), "{sent}");
        assert!(!sent.contains("the same call fails again"), "a denial is not a failure to retry: {sent}");
        assert!(team::worktree_dir(&root, "hello").exists(), "the worktree is left for the user");
        team::remove(&root, "hello", true, true, &CancellationToken::new()).unwrap();
        std::fs::remove_dir_all(team::worktree_dir(&root, "x").parent().unwrap()).ok();
        std::fs::remove_dir_all(root).ok();
    }

    /// The model's own mistakes still count, so a loop of unfixable-by-retry calls stops the task.
    #[tokio::test]
    async fn asking_for_a_missing_file_still_ends_as_stuck() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, _) = mock(vec![
            sse_tool("c1", "read_file", json!({ "path": "gone1.txt" })),
            sse_tool("c2", "read_file", json!({ "path": "gone2.txt" })),
            sse_tool("c3", "read_file", json!({ "path": "gone3.txt" })),
            sse_text("unused"),
        ]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Stuck, "three rounds of real failures stop the task");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn user_denial_and_stuck_detection() {
        let root = temp_repo(&[("greet.txt", "hi\n")]);
        let call = || sse_tool("c", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "x" }));
        let (base, _) = mock(vec![call(), call(), call(), call()]);
        let r = run(
            host(&root, ApprovalDecision::Deny, Arc::default()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Stuck);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hi\n");
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mcp_tools_hooks_and_skills_take_part_in_the_loop() {
        let root = temp_repo(&[
            ("a.txt", "x\n"),
            (
                ".anvil/skills/readonly-review.md",
                "---\ntriggers: greeting\ntools: mcp__demo__echo\n---\nOnly use read tools and the echo tool.",
            ),
        ]);
        let (base, seen) = mock(vec![
            sse_tool("c1", "mcp__demo__echo", json!({ "text": "hi" })),
            sse_tool("c2", "read_file", json!({ "path": "a.txt" })),
            sse_text("done"),
        ]);
        let h = host(&root, ApprovalDecision::Allow, Arc::default());
        let mut cfgs = std::collections::BTreeMap::new();
        cfgs.insert(
            "demo".to_string(),
            mcp::ServerConfig {
                command: "sh".into(),
                args: vec!["-c".into(), mcp::tests::SERVER.into()],
                timeout_ms: Some(5_000),
                ..Default::default()
            },
        );
        h.mcp.write().await.configure(cfgs, None).await;
        *h.hooks.write().unwrap() = vec![hooks::Hook {
            event: hooks::Event::BeforeTool,
            command: vec!["sh".into(), "-c".into(), "grep -q read_file && exit 1 || exit 0".into()],
            matcher: None,
            timeout_ms: Some(5_000),
        }];
        let r = run(h, params(&base, ChatMode::Agent, Mode::AskBeforeEdit), CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        let reqs = seen.lock().unwrap();
        let names: Vec<&str> = reqs[0]["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"mcp__demo__echo") && names.contains(&"read_file"));
        assert!(!names.contains(&"replace_text"), "skill tool list limits non-read tools");
        let results: Vec<&str> = reqs[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert!(results[0].contains("pong"));
        assert!(results[1].contains("Blocked by a BeforeTool hook"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn falls_back_to_configured_provider_and_reports_it() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, _) = mock(vec![sse_text("answer from fallback")]);
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut p = params(&format!("http://{dead}/v1"), ChatMode::Ask, Mode::ReadOnly);
        p.fallbacks = vec![params(&base, ChatMode::Ask, Mode::ReadOnly).provider];
        let events = Arc::new(Mutex::new(Vec::new()));
        let r = run(host(&root, ApprovalDecision::Allow, events.clone()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert_eq!(r.final_text, "answer from fallback");
        assert_eq!((r.metrics.retries, r.metrics.fallbacks), (2, 1));
        assert!(r.metrics.local_tokens == 0 || is_local(&params(&base, ChatMode::Ask, Mode::ReadOnly).provider));
        assert!(events.lock().unwrap().iter().any(|e| e["type"] == "modelSwitched"));
        std::fs::remove_dir_all(root).ok();
    }

    /// A fallback model has its own window: the request must be rebuilt (and the history squeezed to
    /// fit) instead of reusing the shape that was built for the model that just failed.
    #[tokio::test]
    async fn a_fallback_rebuilds_the_request_for_its_own_window() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![sse_text("answered by the fallback")]);
        // A history that fits the primary model (a 1M-token Anthropic model) but not 32K.
        let big = "a line of the file\n".repeat(20_000);
        let call = |id: &str| Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse { id: id.into(), name: "read_file".into(), input: json!({ "path": "a.txt" }) }],
        };
        let result = |id: &str, content: String| Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                tool_name: Some("read_file".into()),
                content,
                is_error: false,
            }],
        };
        let mut messages = vec![Message { role: Role::User, content: vec![ContentPart::Text { text: "read a.txt".into() }] }];
        for (i, content) in [big, "ok".into(), "ok".into(), "ok".into()].into_iter().enumerate() {
            let id = format!("c{i}");
            messages.push(call(&id));
            messages.push(result(&id, content));
        }
        let h = host(&root, ApprovalDecision::Allow, Arc::default());
        h.store.save(&saved_record("t-big", messages)).unwrap();
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut p = params(&format!("http://{dead}/v1"), ChatMode::Ask, Mode::ReadOnly);
        p.provider = ProviderConfig {
            kind: ProviderKind::Anthropic,
            model: "claude-sonnet-4-6".into(),
            base_url: Some(format!("http://{dead}/v1")),
            api_key: Some("k".into()),
            timeout_ms: Some(5_000),
        };
        p.fallbacks = vec![params(&base, ChatMode::Ask, Mode::ReadOnly).provider];
        p.task_id = Some("t-big".into());
        let r = run(h, p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        assert_eq!(r.metrics.fallbacks, 1);
        let reqs = seen.lock().unwrap();
        let sent = reqs[0].to_string();
        assert!(sent.len() < 100_000, "the 320 KB history was squeezed to fit the 32K model, not {} bytes", sent.len());
        assert!(sent.contains("[context compressed]"), "the old tool output was pruned for the fallback");
        std::fs::remove_dir_all(root).ok();
    }

    /// A model that writes "Research" (or "docs", "qa") must still get a subagent, not a failed call.
    #[tokio::test]
    async fn a_subagent_role_is_read_however_the_model_spells_it() {
        let root = git_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![
            sse_tool("s1", "run_subagent", json!({ "role": "Research", "objective": "find a.txt", "name": "look" })),
            sse_text("a.txt holds x."),
            sse_text("The subagent found a.txt."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let r = run(
            host(&root, ApprovalDecision::Allow, events.clone()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        assert_eq!(r.metrics.subagents, 1, "the subagent ran");
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| e["type"] == "subagentStarted" && e["role"] == "research"), "the role is normalized");
        assert!(!ev.iter().any(|e| e["type"] == "toolFinished" && e["ok"] == false), "no call failed");
        let sent = seen.lock().unwrap()[2].to_string();
        assert!(sent.contains("a.txt holds x."), "the report reached the lead agent");
        std::fs::remove_dir_all(root).ok();
    }

    /// An unusable role is the model's input to fix, so the error says which roles exist.
    #[tokio::test]
    async fn an_unknown_subagent_role_lists_the_choices() {
        let root = git_repo(&[("a.txt", "x\n")]);
        let (base, seen) =
            mock(vec![sse_tool("s1", "run_subagent", json!({ "role": "chef", "objective": "cook" })), sse_text("I used the wrong role.")]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "the task goes on after a bad input");
        assert_eq!(r.metrics.subagents, 0);
        let sent = seen.lock().unwrap()[1].to_string();
        assert!(sent.contains("unknown role 'chef'"), "{sent}");
        assert!(sent.contains("research, implementation, testing, review, documentation"), "{sent}");
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subagent_works_in_worktree_and_patch_is_applied() {
        let root = temp_repo(&[("greet.txt", "hi\n")]);
        let g = |a: &[&str]| std::process::Command::new("git").args(a).current_dir(&root).output().unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        let (base, seen) = mock(vec![
            sse_tool("p1", "run_subagent", json!({ "role": "implementation", "objective": "say hello", "name": "hello" })),
            sse_tool("s1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("Changed greet.txt in the worktree."),
            sse_tool("p2", "worktree_apply", json!({ "name": "hello" })),
            sse_text("Applied the subagent's change."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let r = run(
            host(&root, ApprovalDecision::Allow, events.clone()),
            params(&base, ChatMode::Agent, Mode::AskBeforeEdit),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hello\n");
        let reqs = seen.lock().unwrap();
        let sub_result =
            reqs[3]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_owned();
        assert!(sub_result.contains("+hello") && sub_result.contains("\"worktree\":\"hello\""), "{sub_result}");
        assert!(events.lock().unwrap().iter().any(|e| e["type"] == "subagent"));
        let wt = team::worktree_dir(&root, "hello");
        assert!(wt.exists(), "worktrees stay until discarded");
        team::remove(&root, "hello", true, true, &CancellationToken::new()).unwrap();
        std::fs::remove_dir_all(team::worktree_dir(&root, "x").parent().unwrap()).ok();
        std::fs::remove_dir_all(root).ok();
    }
    /// Discarding is the agent cleaning up its own worktree, so it is an edit: no opt-in needed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_worktree_the_agent_created_can_be_discarded() {
        let root = git_repo(&[("greet.txt", "hi\n")]);
        let (base, seen) = mock(vec![
            sse_tool("p1", "run_subagent", json!({ "role": "implementation", "objective": "say hello", "name": "hello" })),
            sse_tool("s1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("Changed greet.txt in the worktree."),
            sse_tool("p2", "worktree_discard", json!({ "name": "hello" })),
            sse_text("Discarded it; the main tree is unchanged."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        // Autonomous: the policy alone decides, with an empty allow list, as the extension sends it.
        let r = run(
            host(&root, ApprovalDecision::Allow, events.clone()),
            params(&base, ChatMode::Agent, Mode::Autonomous),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hi\n", "the worktree's change is not applied");
        assert!(!team::worktree_dir(&root, "hello").exists(), "the worktree is gone");
        let ev = events.lock().unwrap();
        assert!(!ev.iter().any(|e| e["type"] == "approvalRequested" && e["tool"] == "worktree_discard"), "no approval for an edit");
        assert!(ev.iter().any(|e| e["type"] == "toolFinished" && e["tool"] == "worktree_discard" && e["ok"] == true));
        assert!(r.final_text.contains("Discarded it"), "the task went on after the discard: {:?}", r.final_text);
        assert!(
            seen.lock().unwrap().iter().any(|q| q.to_string().contains("\\\"discarded\\\":\\\"hello\\\"")),
            "the discard result reached the next turn"
        );
        std::fs::remove_dir_all(team::worktree_dir(&root, "x").parent().unwrap()).ok();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn worktree_tools_are_edits_not_deletions() {
        // `delete` is destructive: it is denied unless the user names the tool in `allowTools`,
        // which nobody does, so a discard was impossible.
        assert_eq!(Target::Worktree("worktree_discard").permission(), Permission::Write);
        assert_eq!(Target::Worktree("worktree_apply").permission(), Permission::Write);
        let p = Policy::new(PolicyConfig { mode: Mode::Autonomous, ..Default::default() });
        assert_eq!(p.decide("worktree_discard", Permission::Write), Decision::Allow);
    }

    /// Mock that answers each request with `f(request body)`, for runs whose request order varies.
    fn mock_with(n: usize, f: impl Fn(&Value) -> String + Send + 'static) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut served = 0;
            for conn in l.incoming() {
                let mut c = conn.unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut request = String::new();
                r.read_line(&mut request).unwrap();
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                // The model listing is not mocked: it answers 404 and does not count as a turn.
                if request.starts_with("GET ") {
                    write!(c, "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).unwrap();
                let body = f(&serde_json::from_slice(&buf).unwrap());
                write!(c, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}").unwrap();
                served += 1;
                if served == n {
                    break;
                }
            }
        });
        format!("http://{addr}/v1")
    }

    fn git_repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = temp_repo(files);
        let g = |a: &[&str]| std::process::Command::new("git").args(a).current_dir(&root).output().unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        root
    }

    fn step(id: &str, deps: &[&str]) -> PlanStep {
        PlanStep {
            id: id.into(),
            role: team::Role::Research,
            objective: "x".into(),
            context: String::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn plan_order_follows_dependencies_and_rejects_bad_plans() {
        let order = plan_order(&[step("c", &["a", "b"]), step("a", &[]), step("b", &["a"])]).unwrap();
        assert_eq!(order, [1, 2, 0]);
        assert!(plan_order(&[step("a", &["b"]), step("b", &["a"])]).unwrap_err().contains("cycle"));
        assert!(plan_order(&[step("a", &["zz"])]).unwrap_err().contains("unknown"));
        assert!(plan_order(&[step("a", &[]), step("a", &[])]).unwrap_err().contains("twice"));
        assert!(plan_order(&[step("bad id", &[])]).is_err());
        assert!(plan_order(&[]).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn team_mode_runs_a_plan_and_merges_into_the_edit_log() {
        let root = git_repo(&[("greet.txt", "hi\n")]);
        let plan = json!({ "steps": [
            { "id": "look", "role": "research", "objective": "find the greeting" },
            { "id": "edit", "role": "implementation", "objective": "say hello", "dependsOn": ["look"] }
        ] });
        let (base, seen) = mock(vec![
            sse_tool("p1", "run_plan", plan),
            sse_text("The greeting is in greet.txt."),
            sse_tool("s1", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" })),
            sse_text("Changed greet.txt."),
            sse_text("The team changed the greeting."),
        ]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let h = host(&root, ApprovalDecision::Allow, events.clone());
        let edits = h.edits.clone();
        let r = run(h, params(&base, ChatMode::Team, Mode::AskBeforeEdit), CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hello\n");
        assert_eq!(r.metrics.subagents, 2);
        assert!(r.changed_files.contains("greet.txt"));
        assert_eq!(tools::pending(&edits.lock().unwrap())[0].path, "greet.txt", "merged work is reviewable and undoable");
        let reqs = seen.lock().unwrap();
        let impl_prompt = reqs[2]["messages"].to_string();
        assert!(impl_prompt.contains("Result of step look") && impl_prompt.contains("greet.txt"), "dependency report is passed on");
        let ev = events.lock().unwrap();
        let types: Vec<&str> = ev.iter().filter_map(|e| e["type"].as_str()).collect();
        assert!(types.contains(&"planStarted") && types.contains(&"subagentStarted") && types.contains(&"subagentFinished"));
        assert!(ev.iter().any(|e| e["type"] == "merged" && e["ok"] == true && e["step"] == "edit"));
        assert!(team::list(&root, &CancellationToken::new()).unwrap().is_empty(), "merged worktrees are removed");
        std::fs::remove_dir_all(team::worktree_dir(&root, "x").parent().unwrap()).ok();
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn best_of_runs_candidates_in_parallel_worktrees() {
        let root = git_repo(&[("greet.txt", "hi\n")]);
        let base = mock_with(4, |req| {
            let done = req["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool");
            if done {
                sse_text("done")
            } else {
                sse_tool("c", "replace_text", json!({ "path": "greet.txt", "oldText": "hi", "newText": "hello" }))
            }
        });
        let pr = params(&base, ChatMode::Agent, Mode::AskBeforeEdit).provider;
        let p = BestOfParams {
            prompt: "Say hello".into(),
            providers: vec![pr.clone(), pr],
            policy: PolicyConfig { mode: Mode::AskBeforeExecution, ..Default::default() },
            editor: EditorContext::default(),
            limits: Limits::default(),
            prices: vec![cost::PriceRule { model: "m".into(), input_per_mtok: 1.0, output_per_mtok: 1.0 }],
            task_id: Some("bo1".into()),
        };
        let out = best_of(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        let c = out["candidates"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        for cand in c {
            assert_eq!(cand["status"], "completed", "{cand}");
            assert_eq!((cand["added"].as_u64(), cand["removed"].as_u64()), (Some(1), Some(1)));
            assert!(cand["patch"].as_str().unwrap().contains("+hello"));
        }
        assert_eq!(std::fs::read_to_string(root.join("greet.txt")).unwrap(), "hi\n", "nothing is merged until the user picks");
        for cand in c {
            team::remove(&root, cand["name"].as_str().unwrap(), true, false, &CancellationToken::new()).unwrap();
        }
        std::fs::remove_dir_all(team::worktree_dir(&root, "x").parent().unwrap()).ok();
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn compresses_the_history_when_it_nears_the_window() {
        let root = temp_repo(&[("big.txt", &"some long line of file content\n".repeat(300))]);
        // Different ranges each time: a repeat of the same read is answered with a short note.
        let read = |id: &str| sse_tool(id, "read_file", json!({ "path": "big.txt", "offset": id[1..].parse::<u32>().unwrap() }));
        let (base, seen) = mock(vec![read("r1"), read("r2"), read("r3"), read("r4"), sse_text("done")]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut p = params(&base, ChatMode::Ask, Mode::ReadOnly);
        p.limits.context_window_tokens = Some(8_000);
        p.limits.compact_percent = 30;
        let r = run(host(&root, ApprovalDecision::Allow, events.clone()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        let ev = events.lock().unwrap();
        let c = ev.iter().find(|e| e["type"] == "compacted").expect("compression ran");
        assert!(c["pruned"].as_u64().unwrap() >= 1 && c["afterTokens"].as_u64() < c["beforeTokens"].as_u64(), "{c}");
        assert_eq!(c["windowTokens"], 8_000);
        assert!(ev.iter().any(|e| e["type"] == "context" && e["windowTokens"] == 8_000));
        let last = seen.lock().unwrap().last().unwrap()["messages"].to_string();
        assert!(last.contains(compact::MARK) || last.contains("<progress_summary>"), "the model sees the compressed history");
        assert!(last.contains("some long line of file content"), "the newest read is kept in full");
        std::fs::remove_dir_all(root).ok();
    }

    /// The tool list and its plain-text guide survive compression: losing them makes the model
    /// invent tool names and fields, which costs far more than the tokens they take.
    #[tokio::test]
    async fn compression_never_touches_the_tools() {
        let root = temp_repo(&[("big.txt", &"some long line of file content\n".repeat(300))]);
        let read = |id: &str| sse_tool(id, "read_file", json!({ "path": "big.txt", "offset": id[1..].parse::<u32>().unwrap() }));
        let (base, seen) = mock(vec![read("r1"), read("r2"), read("r3"), read("r4"), sse_text("done")]);
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut p = params(&base, ChatMode::Ask, Mode::ReadOnly);
        p.limits.context_window_tokens = Some(8_000);
        p.limits.compact_percent = 30;
        let r = run(host(&root, ApprovalDecision::Allow, events.clone()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        assert!(events.lock().unwrap().iter().any(|e| e["type"] == "compacted"), "compression ran");
        let reqs = seen.lock().unwrap();
        let first = &reqs[0];
        let last = reqs.last().unwrap();
        // Every turn offers the same tools, with the same schemas.
        assert_eq!(first["tools"], last["tools"], "the tool list is identical after compression");
        assert!(first["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "read_file"));
        // The guide stays in the system prompt, before and after.
        let system = |q: &Value| q["messages"][0]["content"].as_str().unwrap_or_default().to_owned();
        for q in [first, last] {
            let s = system(q);
            assert!(s.contains("<tools>") && s.contains("read_file("), "the tool guide survives: {s}");
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn find_tool_matches_names_or_every_word() {
        let spec = |name: &str, description: &str| crate::provider::ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema: json!({}),
        };
        let deferred =
            vec![spec("git_log", "Show recent commits"), spec("web_fetch", "Fetch a public web page"), spec("git_show", "Show one commit")];
        assert_eq!(match_tools(&deferred, &json!({ "names": ["web_fetch", "nope"] })), ["web_fetch"]);
        assert_eq!(match_tools(&deferred, &json!({ "query": "git commit" })), ["git_log", "git_show"]);
        assert!(match_tools(&deferred, &json!({ "query": "" })).is_empty());
    }

    #[tokio::test]
    async fn lazy_tools_offer_a_core_set_and_load_the_rest_on_first_call() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![sse_tool("t1", "get_file_tree", json!({})), sse_text("Listed.")]);
        let mut p = params(&base, ChatMode::Ask, Mode::ReadOnly);
        p.limits.lazy_tools = true;
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed);
        let reqs = seen.lock().unwrap();
        let names = |q: &Value| {
            q["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
        };
        let first = names(&reqs[0]);
        assert!(first.contains(&"read_file".to_owned()) && first.contains(&FIND_TOOL.to_owned()), "{first:?}");
        assert!(!first.contains(&"get_file_tree".to_owned()), "a non-core tool waits");
        assert!(names(&reqs[1]).contains(&"get_file_tree".to_owned()), "calling it by name loads it");
        let result = reqs[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap()["content"].to_string();
        assert!(result.contains("a.txt"), "the deferred tool ran: {result}");
        std::fs::remove_dir_all(root).ok();
    }

    /// `summarize_file`: a separate model reads the file, so its text never enters this conversation,
    /// and its tokens count with the subagents' (the file's contents are not sent to the next turn).
    #[tokio::test]
    async fn summarize_file_reads_the_file_with_the_helper_model() {
        let root = temp_repo(&[("big.txt", "line one\nline two\nline three\n")]);
        let helper = mock(vec![sse_text_usage("Two short lines of text.", 1_000, 20)]);
        let (base, seen) = mock(vec![
            sse_tool("c1", SUMMARIZE, json!({ "path": "big.txt", "question": "How many lines?" })),
            sse_text("It is a short file."),
        ]);
        let mut p = params(&base, ChatMode::Agent, Mode::AskBeforeEdit);
        p.helper = Some(ProviderConfig {
            kind: ProviderKind::OpenAiCompatible,
            model: "small".into(),
            base_url: Some(helper.0),
            api_key: Some("k".into()),
            timeout_ms: Some(5_000),
        });
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        // The helper model got the file, with line numbers, and the question.
        let asked = helper.1.lock().unwrap()[0].to_string();
        assert!(asked.contains("big.txt") && asked.contains("3 line three") && asked.contains("How many lines?"), "{asked}");
        let calls = seen.lock().unwrap();
        let result = calls[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap()["content"].to_string();
        assert!(result.contains("Two short lines of text."), "the summary is the tool result: {result}");
        assert!(!result.contains("line two"), "the file itself stays out of the conversation: {result}");
        assert_eq!(r.metrics.subagent_tokens, 1_020, "the helper's tokens count with the subagents'");
        std::fs::remove_dir_all(root).ok();
    }

    /// Project memory saved by an earlier task is in the system prompt of the next one, so the agent
    /// does not explore the repository again to learn the same facts.
    #[tokio::test]
    async fn project_memory_starts_the_next_task() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        std::fs::create_dir_all(root.join(".task-x")).unwrap();
        std::fs::write(root.join(".task-x/memory.md"), "# Project memory\n\n- Tests: cargo test --workspace\n").unwrap();
        let (base, seen) = mock(vec![sse_text("I used the saved test command.")]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Ask, Mode::ReadOnly),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed, "{:?}", r.error);
        let system = seen.lock().unwrap()[0]["messages"][0]["content"].as_str().unwrap().to_owned();
        assert!(system.contains("<project_memory>") && system.contains("cargo test --workspace"), "{system}");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn a_repeated_read_is_answered_with_a_note() {
        let root = temp_repo(&[("a.txt", "the original content\n")]);
        let read = |id: &str| sse_tool(id, "read_file", json!({ "path": "a.txt" }));
        let (base, seen) = mock(vec![read("r1"), read("r2"), sse_text("done")]);
        let r = run(
            host(&root, ApprovalDecision::Allow, Arc::default()),
            params(&base, ChatMode::Ask, Mode::ReadOnly),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r.status, Status::Completed);
        let reqs = seen.lock().unwrap();
        let results: Vec<String> =
            reqs[2]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["content"].to_string()).collect();
        assert!(results[0].contains("the original content"));
        assert!(results[1].contains("unchanged") && !results[1].contains("the original content"), "{}", results[1]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_tool_guide_names_every_tool_and_its_fields() {
        let specs = vec![
            crate::provider::ToolSpec {
                name: "read_file".into(),
                description: "long description that is not repeated in the guide".into(),
                input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" }, "limit": { "type": "integer" }, "raw": { "type": "boolean" }, "tags": { "type": "array" } }, "required": ["path"] }),
            },
            crate::provider::ToolSpec {
                name: "git_status".into(),
                description: "d".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ];
        let guide = tool_guide(&specs);
        // One line per tool: name, required first-class fields, `?` for optional, short type names.
        assert!(guide.starts_with("<tools>") && guide.ends_with("</tools>"), "{guide}");
        assert!(guide.contains("read_file(path:s, limit?:n, raw?:b, tags?:[])"), "{guide}");
        assert!(guide.contains("\ngit_status\n"), "a tool without arguments is just its name: {guide}");
        assert!(!guide.contains("long description"), "descriptions stay out, keeping it short: {guide}");
        assert!(tool_guide(&[]).is_empty(), "no tools, no block");
    }

    #[tokio::test]
    async fn limit_reached_forces_a_summary_without_tools() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![sse_tool("c", "read_file", json!({ "path": "a.txt" })), sse_text("Read a.txt. The rest is open.")]);
        let mut p = params(&base, ChatMode::Agent, Mode::AskBeforeEdit);
        p.limits.max_iterations = 1;
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::MaxIterations);
        assert_eq!(r.final_text, "Read a.txt. The rest is open.");
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2, "one turn with tools, then the summary turn");
        assert!(reqs[1]["tools"].as_array().is_none_or(Vec::is_empty), "the summary turn has no tools");
        assert!(reqs[1]["messages"].to_string().contains("iteration limit was reached"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn token_budget_forces_a_summary() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![sse_text("Budget reached.")]);
        let mut p = params(&base, ChatMode::Agent, Mode::AskBeforeEdit);
        p.limits.max_total_tokens = Some(0);
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::BudgetExceeded);
        assert_eq!(r.final_text, "Budget reached.");
        let reqs = seen.lock().unwrap();
        assert!(reqs[0]["tools"].as_array().is_none_or(Vec::is_empty), "only the summary turn runs, without tools");
        assert!(reqs[0]["messages"].to_string().contains("token budget was reached"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn iteration_limit_stops_the_loop() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, _) = mock(vec![
            sse_tool("c", "read_file", json!({ "path": "a.txt" })),
            sse_tool("d", "list_files", json!({})),
            sse_text("Stopped at the limit."),
        ]);
        let mut p = params(&base, ChatMode::Agent, Mode::AskBeforeEdit);
        p.limits.max_iterations = 2;
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), p, CancellationToken::new()).await.unwrap();
        assert_eq!(r.status, Status::MaxIterations);
        assert_eq!(r.iterations, 2);
        std::fs::remove_dir_all(root).ok();
    }

    /// A model that ends with an empty reply (its answer only in its reasoning) is asked again, so the
    /// task does not end with nothing to show.
    #[tokio::test]
    async fn an_empty_final_reply_is_asked_again() {
        let root = temp_repo(&[("a.txt", "x\n")]);
        let (base, seen) = mock(vec![sse_text(""), sse_text("Here are the missing features.")]);
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), params(&base, ChatMode::Agent, Mode::AskBeforeEdit), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(r.status, Status::Completed);
        assert_eq!(r.final_text, "Here are the missing features.");
        assert!(seen.lock().unwrap()[1]["messages"].to_string().contains("Your last reply had no text"));

        let (base, _) = mock(vec![sse_text(""), sse_text(""), sse_text(""), sse_text("Summary of the work.")]);
        let r = run(host(&root, ApprovalDecision::Allow, Arc::default()), params(&base, ChatMode::Agent, Mode::AskBeforeEdit), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!((r.status, r.final_text.as_str()), (Status::Completed, "Summary of the work."), "after the nudges, a summary without tools");
        std::fs::remove_dir_all(root).ok();
    }
}
