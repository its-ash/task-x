//! Custom agents: named subagent definitions loaded from Markdown files with a small front matter,
//! in the format Claude Code and Copilot use.
//!
//! Locations, lowest precedence first (a later file wins on a name clash):
//! `~/.claude/agents/`, `~/.copilot/agents/`, `~/.task-x/agents/`, then in the workspace
//! `.claude/agents/`, `.github/agents/`, `.task-x/agents/`, and the `agents/` folder of every
//! enabled plugin (named `<plugin>:<agent>`).
//!
//! ```text
//! ---
//! name: code-reviewer
//! description: Review changed code for defects
//! tools: read_file, search_text, run_tests     # optional: the tools the agent may call
//! mode: read                                    # optional: read (default when every tool reads) or write
//! model: haiku                                  # optional: alias or model id; inherit = the lead's model
//! maxIterations: 20                             # optional
//! ---
//! You are a code reviewer…
//! ```
//!
//! The lead delegates with `run_subagent { agent: "code-reviewer", … }`, and a prompt that starts
//! with `@code-reviewer` runs the whole task as that agent.

use super::skills::front_matter;
use super::team::Role;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_AGENT_BYTES: usize = 64 * 1024;
const MAX_AGENT_FILE_BYTES: u64 = 256 * 1024;
const MAX_INDEX_CHARS: usize = 8_000;
pub const MAX_ITERATIONS: u32 = 60;

/// Tool names that only read; an agent limited to these gets no worktree.
const READ_ONLY_HINTS: &[&str] = &["read", "grep", "glob", "search", "find", "list", "get_", "web", "fetch", "summarize", "docs"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Tools the agent may call; empty means every tool of its role.
    pub tools: Vec<String>,
    /// `write` agents get a worktree and the edit tools; `read` agents only investigate.
    pub writes: bool,
    /// Model alias or id; `None` or `inherit` keeps the caller's model.
    pub model: Option<String>,
    pub max_iterations: Option<u32>,
    pub source: String,
    #[serde(skip)]
    pub body: String,
}

impl AgentDef {
    /// The built-in role whose permissions and worktree handling this agent uses.
    pub fn role(&self) -> Role {
        if self.writes { Role::Implementation } else { Role::Research }
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

fn list(v: &str) -> Vec<String> {
    v.trim_matches(['[', ']']).split(',').map(|s| s.trim().trim_matches(['"', '\'']).to_owned()).filter(|s| !s.is_empty()).collect()
}

/// Tool names Claude Code and Copilot use, mapped to the task-x registry.
pub fn map_tool(t: &str) -> Vec<String> {
    let key = t.trim().to_ascii_lowercase();
    let mapped: &[&str] = match key.as_str() {
        "read" => &["read_file", "read_range", "read_symbol", "read_multiple_files", "get_file_outline"],
        "grep" | "search" => &["search_text", "search_regex", "multi_search", "find_symbol", "find_references"],
        "glob" | "ls" => &["find_files", "list_files", "get_file_tree"],
        "edit" | "multiedit" | "write" | "notebookedit" => &["replace_text", "edit_files", "create_file", "apply_patch", "insert_at_line", "delete_range"],
        "bash" | "shell" | "terminal" | "runcommands" | "execute" => &["run_command", "run_tests", "check_build", "run_linter", "format_code"],
        "webfetch" => &["web_fetch"],
        "websearch" => &["web_search"],
        "web" => &["web_fetch", "web_search"],
        "task" | "agent" => &["run_subagent"],
        "todowrite" | "todo" => &["todo_write"],
        _ => return vec![t.trim().to_owned()],
    };
    mapped.iter().map(|s| (*s).to_owned()).collect()
}

fn reads_only(tools: &[String]) -> bool {
    !tools.is_empty() && tools.iter().all(|t| READ_ONLY_HINTS.iter().any(|h| t.starts_with(h) || t.contains(h)) && !t.starts_with("run_"))
}

pub fn parse(text: &str, fallback_name: &str, source: &str) -> Option<AgentDef> {
    let (fields, body) = front_matter(text);
    let name = fields.get("name").cloned().unwrap_or_else(|| fallback_name.to_owned());
    if !valid_name(&name) {
        return None;
    }
    let mut body = body.trim().to_owned();
    if body.is_empty() {
        return None;
    }
    if body.len() > MAX_AGENT_BYTES {
        let mut cut = MAX_AGENT_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    let tools: Vec<String> = fields.get("tools").map(|v| list(v)).unwrap_or_default().iter().flat_map(|t| map_tool(t)).collect();
    let mode = fields.get("mode").map(|m| m.to_ascii_lowercase());
    let writes = match mode.as_deref() {
        Some("read" | "readonly" | "read-only" | "ask" | "plan") => false,
        Some("write" | "edit" | "agent") => true,
        _ => !reads_only(&tools),
    };
    let model = fields.get("model").map(|m| m.trim().to_owned()).filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("inherit"));
    let max_iterations = fields
        .get("maxiterations")
        .or_else(|| fields.get("max_iterations"))
        .or_else(|| fields.get("max-turns"))
        .or_else(|| fields.get("maxturns"))
        .and_then(|v| v.parse::<u32>().ok())
        .map(|n| n.clamp(1, MAX_ITERATIONS));
    Some(AgentDef {
        description: fields.get("description").cloned().unwrap_or_default(),
        tools,
        writes,
        model,
        max_iterations,
        source: source.to_owned(),
        name,
        body,
    })
}

fn load_dir(dir: &Path, prefix: &str, rank: u8, out: &mut BTreeMap<String, (u8, AgentDef)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        let (file, fallback) = if p.is_dir() {
            (p.join("AGENT.md"), fname)
        } else if let Some(stem) = fname.strip_suffix(".agent.md").or_else(|| fname.strip_suffix(".md")) {
            (p.clone(), stem.to_owned())
        } else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&file) else { continue };
        if meta.len() > MAX_AGENT_FILE_BYTES {
            continue;
        }
        let Some(mut a) = std::fs::read_to_string(&file).ok().and_then(|t| parse(&t, &fallback, &file.display().to_string())) else { continue };
        if !prefix.is_empty() {
            a.name = format!("{prefix}:{}", a.name);
        }
        out.insert(a.name.clone(), (rank, a));
    }
}

pub fn user_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else { return Vec::new() };
    [".claude", ".copilot", ".task-x"].iter().map(|d| Path::new(&home).join(d).join("agents")).collect()
}

/// Agents that come with task-x: (name, description, tools, instructions). They use the caller's
/// model, so they work with any provider. A file with the same name replaces one.
const BUILTIN: &[(&str, &str, &[&str], &str)] = &[(
    "designer",
    "Designs logos, icons, and other vector graphics as SVG files, checks each one by rendering it, and exports PNG sizes on request.",
    &["create_file", "replace_text", "edit_files", "apply_patch", "render_svg", "export_png"],
    "You are a graphic designer who works in hand-written SVG.\n\n\
Start with read_skill { name: \"vector-design\" } and follow it for every graphic.\n\n\
- Work only on the graphics the objective asks for. Do not change code, configuration, or other assets.\n\
- Write each SVG with create_file and fix it with replace_text or edit_files; never paste the SVG into your reply instead of writing the file.\n\
- Check every version with render_svg. Fix every warning and anything that looks wrong, then render again; stop after 4 rounds.\n\
- When asked for PNGs, favicons, or app icons, call export_png once the SVG is final (favicon: 16, 32, 48; Apple touch: 180; Android: 192, 512).\n\
- Finish with a short report: the files, the concept in one line, the colours, and anything the user should check by eye.",
)];

fn builtin(out: &mut BTreeMap<String, (u8, AgentDef)>) {
    for (name, description, tools, body) in BUILTIN {
        let a = AgentDef {
            name: (*name).to_owned(),
            description: (*description).to_owned(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            writes: true,
            model: None,
            max_iterations: None,
            source: "built-in".into(),
            body: (*body).to_owned(),
        };
        out.insert(a.name.clone(), (0, a));
    }
}

/// Every agent found; `plugins` are `(name, path)` of the enabled plugins. Workspace agents come
/// first, then the user's, then plugin agents, then the built-in ones (each group by name), so the
/// index the lead sees keeps the project's own agents when it has to cut the list.
pub fn load(workspace: Option<&Path>, user_dirs: &[PathBuf], plugins: &[(String, PathBuf)]) -> Vec<AgentDef> {
    let mut out = BTreeMap::new();
    builtin(&mut out);
    for (name, path) in plugins {
        load_dir(&path.join("agents"), name, 1, &mut out);
    }
    for u in user_dirs {
        load_dir(u, "", 2, &mut out);
    }
    if let Some(w) = workspace {
        for d in [".claude/agents", ".github/agents", ".task-x/agents"] {
            load_dir(&w.join(d), "", 3, &mut out);
        }
    }
    let mut all: Vec<(u8, AgentDef)> = out.into_values().collect();
    all.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    all.into_iter().map(|(_, a)| a).collect()
}

/// Finds an agent by name; a plugin agent also answers to its short name when it is the only one.
pub fn find<'a>(agents: &'a [AgentDef], name: &str) -> Option<&'a AgentDef> {
    let key = name.trim().trim_start_matches('@');
    agents.iter().find(|a| a.name.eq_ignore_ascii_case(key)).or_else(|| {
        let mut short = agents.iter().filter(|a| a.name.rsplit(':').next().is_some_and(|s| s.eq_ignore_ascii_case(key)));
        let first = short.next();
        if short.next().is_none() { first } else { None }
    })
}

/// `@name rest` at the start of a prompt: the agent to run the whole task as, and the rest.
pub fn explicit<'a>(agents: &'a [AgentDef], prompt: &str) -> Option<(&'a AgentDef, String)> {
    let rest = prompt.trim_start().strip_prefix('@')?;
    let (head, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if head.contains('/') || head.contains('.') && find(agents, head).is_none() {
        return None;
    }
    find(agents, head).map(|a| (a, tail.trim().to_owned()))
}

/// The agent list for the lead's system prompt (names and descriptions only).
pub fn index(agents: &[AgentDef]) -> Option<String> {
    let mut lines = Vec::new();
    let mut size = 0;
    for a in agents {
        let d: String = a.description.chars().take(160).collect();
        let line = format!("- {} ({}): {}", a.name, if a.writes { "edits" } else { "read-only" }, if d.is_empty() { "(no description)" } else { &d });
        size += line.len();
        if size > MAX_INDEX_CHARS {
            lines.push("- … more agents omitted".into());
            break;
        }
        lines.push(line);
    }
    (!lines.is_empty()).then(|| {
        format!(
            "<available_agents>\nCustom agents are specialists with their own instructions. Delegate to one with run_subagent {{ agent: \"<name>\", objective }} when its description fits a bounded part of the work; several can run at the same time.\n{}\n</available_agents>",
            lines.join("\n")
        )
    })
}

/// The model id an agent file's `model` means for the caller's provider. Claude aliases resolve
/// on Anthropic only; on another provider they are ignored, so the caller's model stays.
pub fn model_for(alias: &str, kind: crate::provider::ProviderKind) -> Option<String> {
    use crate::provider::ProviderKind::Anthropic;
    match (alias.to_ascii_lowercase().as_str(), kind) {
        ("sonnet", Anthropic) => Some("claude-sonnet-5-5".into()),
        ("opus", Anthropic) => Some("claude-opus-5-5".into()),
        ("haiku", Anthropic) => Some("claude-haiku-5-5".into()),
        ("sonnet" | "opus" | "haiku", _) => None,
        _ => Some(alias.to_owned()),
    }
}

/// The caller's provider with the agent's model, when the agent names one that fits the provider.
pub fn provider_for(base: &crate::provider::ProviderConfig, agent: Option<&AgentDef>) -> crate::provider::ProviderConfig {
    let Some(m) = agent.and_then(|a| a.model.as_deref()).and_then(|m| model_for(m, base.kind)) else { return base.clone() };
    let mut alt = base.clone();
    alt.model = m;
    if alt.resolve().is_ok() { alt } else { base.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nname: code-reviewer\ndescription: \"Review changed code\"\ntools: Read, Grep, Glob\nmodel: haiku\nmaxIterations: 500\n---\n\nYou review code.\n";

    #[test]
    fn parses_claude_code_agent_files() {
        let a = parse(DOC, "x", "mem").unwrap();
        assert_eq!(a.name, "code-reviewer");
        assert_eq!(a.description, "Review changed code");
        assert!(!a.writes, "Read, Grep, Glob only read");
        assert_eq!(a.role(), Role::Research);
        assert!(a.tools.contains(&"read_file".to_string()) && a.tools.contains(&"search_regex".to_string()));
        assert_eq!(a.model.as_deref(), Some("haiku"));
        assert_eq!(a.max_iterations, Some(MAX_ITERATIONS), "clamped");
        let w = parse("---\ntools: Read, Edit, Bash\n---\nbuild it", "builder", "mem").unwrap();
        assert!(w.writes && w.role() == Role::Implementation);
        let plain = parse("You write docs.", "docs", "mem").unwrap();
        assert!(plain.writes, "no tool list means every tool");
        assert!(plain.model.is_none());
        assert!(parse("---\nmodel: inherit\n---\nx", "a", "mem").unwrap().model.is_none());
        assert!(parse("---\nname: bad name\n---\nx", "a", "mem").is_none());
        assert!(parse("---\nname: empty\n---\n", "a", "mem").is_none(), "an agent needs instructions");
        assert!(!parse("---\nmode: read\ntools: Bash\n---\nx", "a", "mem").unwrap().writes, "mode wins over tools");
    }

    #[test]
    fn loads_workspace_over_user_and_prefixes_plugin_agents() {
        let base = std::env::temp_dir().join(format!("task-x-agents-{}", std::process::id()));
        let (user, ws, plug) = (base.join("user"), base.join("ws"), base.join("plug"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(ws.join(".claude/agents")).unwrap();
        std::fs::create_dir_all(plug.join("agents")).unwrap();
        std::fs::write(user.join("review.md"), "user version").unwrap();
        std::fs::write(ws.join(".claude/agents/review.md"), "---\ndescription: ws\n---\nworkspace version").unwrap();
        std::fs::write(plug.join("agents/seo.md"), "---\ndescription: seo\n---\naudit").unwrap();
        let all = load(Some(&ws), &[user], &[("claude-seo".into(), plug)]);
        assert_eq!(all.iter().find(|a| a.name == "review").unwrap().body, "workspace version");
        assert!(all.iter().any(|a| a.name == "claude-seo:seo"));
        assert_eq!(find(&all, "seo").unwrap().name, "claude-seo:seo", "short plugin name resolves when unique");
        assert_eq!(find(&all, "@Review").unwrap().name, "review");
        let (a, rest) = explicit(&all, "@review the diff please").unwrap();
        assert_eq!((a.name.as_str(), rest.as_str()), ("review", "the diff please"));
        assert!(explicit(&all, "@src/main.rs explain").is_none(), "a path is a mention, not an agent");
        assert!(explicit(&all, "@nobody do it").is_none());
        assert!(index(&all).unwrap().contains("- review (edits)"));
        assert_eq!(all.last().unwrap().name, "designer", "built-in agents come last");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_built_in_designer_draws_and_checks_with_the_callers_model() {
        let all = load(None, &[], &[]);
        let d = find(&all, "@designer").expect("built in");
        assert!(d.writes && d.model.is_none(), "it edits and keeps the selected model");
        assert!(d.tools.iter().any(|t| t == "render_svg") && d.tools.iter().any(|t| t == "export_png"));
        assert!(!d.tools.iter().any(|t| t == "run_command"), "no shell for a designer");
        assert!(d.body.contains("vector-design"));
        let base = std::env::temp_dir().join(format!("task-x-agents-designer-{}", std::process::id()));
        std::fs::create_dir_all(base.join(".claude/agents")).unwrap();
        std::fs::write(base.join(".claude/agents/designer.md"), "---\ndescription: ours\n---\nOur brand rules.").unwrap();
        let all = load(Some(&base), &[], &[]);
        assert_eq!(find(&all, "designer").unwrap().body, "Our brand rules.", "a project file replaces the built-in agent");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn model_aliases_resolve_for_anthropic_only() {
        use crate::provider::ProviderKind::{Anthropic, Ollama};
        assert_eq!(model_for("haiku", Anthropic).as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(model_for("haiku", Ollama), None, "a Claude alias means nothing to Ollama");
        assert_eq!(model_for("qwen3:8b", Ollama).as_deref(), Some("qwen3:8b"));
        let base = crate::provider::ProviderConfig { kind: Ollama, model: "llama3".into(), base_url: None, api_key: None, timeout_ms: None };
        let a = parse("---\nmodel: qwen3:8b\n---\nx", "a", "mem").unwrap();
        assert_eq!(provider_for(&base, Some(&a)).model, "qwen3:8b");
        let b = parse("---\nmodel: opus\n---\nx", "b", "mem").unwrap();
        assert_eq!(provider_for(&base, Some(&b)).model, "llama3");
        assert_eq!(provider_for(&base, None).model, "llama3");
    }
}
