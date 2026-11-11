//! Custom skills: reusable task instructions loaded from Markdown files with a small front matter.
//!
//! Locations, lowest precedence first (a later file wins on a name clash):
//! `~/.claude/skills/`, `~/.copilot/skills/`, `~/.anvil/skills/`, `~/.task-x/skills/`, then in the
//! workspace `.claude/skills/`, `.github/skills/`, `.anvil/skills/`, `.task-x/skills/`, and Copilot prompt files
//! `.github/prompts/<name>.prompt.md` (usable as `/name`).
//! A skill is `<name>.md` or `<name>/SKILL.md`:
//!
//! ```text
//! ---
//! name: db-migration
//! description: Write a reversible database migration
//! triggers: migration, migrate, schema change
//! patterns: migrations/**, **/*.sql
//! tools: read_file, search_text, create_file, run_tests
//! params: table
//! ---
//! Instructions… may use {{table}}.
//! ```
//!
//! A skill applies when the prompt starts with `/name`, mentions a trigger, or touches a file that
//! matches a pattern. `tools` limits the non-read tools available while the skill is active.
//!
//! Commands are skills the user runs by hand: `.claude/commands/<name>.md`, `~/.claude/commands/`,
//! `.task-x/commands/`, and the `commands/` folder of a plugin (`/plugin:name`). Their body may use
//! `$ARGUMENTS` (everything after `/name`) and `$1`…`$9` (its words); `allowed-tools` limits the
//! tools and `argument-hint` is shown in the menu. They never apply by themselves and are not listed
//! for the model.

use crate::context::index::globset;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_SKILL_BYTES: usize = 96 * 1024;
/// Larger files are skipped; the body is cut to `MAX_SKILL_BYTES`.
const MAX_SKILL_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_ACTIVE: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// Applies by `/name`, a trigger, a pattern, or on demand.
    Skill,
    /// A Claude Code slash command: `/name` only.
    Command,
    /// A Copilot prompt file (`.github/prompts/*.prompt.md`): `/name` or on demand.
    Prompt,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub kind: Kind,
    pub triggers: Vec<String>,
    pub patterns: Vec<String>,
    pub tools: Vec<String>,
    pub params: Vec<String>,
    /// What to type after `/name`, for commands (`argument-hint`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub source: String,
    /// Skills with no triggers or patterns are listed for the model, which loads them on demand.
    pub on_demand: bool,
    #[serde(skip)]
    pub body: String,
}

fn list(v: &str) -> Vec<String> {
    v.trim_matches(['[', ']']).split(',').map(|s| s.trim().trim_matches(['"', '\'']).to_owned()).filter(|s| !s.is_empty()).collect()
}

/// The front matter fields (keys lowercased) and the body of a Markdown file.
pub fn front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut fields = BTreeMap::new();
    let (meta, body) = match text.strip_prefix("---").and_then(|rest| rest.find("\n---").map(|end| (&rest[..end], &rest[end + 4..]))) {
        Some((meta, body)) => (meta, body.trim_start_matches(['-', '\r', '\n'])),
        None => ("", text),
    };
    for line in meta.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if !k.starts_with([' ', '\t', '-']) {
                fields.insert(k.trim().to_ascii_lowercase(), v.trim().trim_matches(['"', '\'']).to_owned());
            }
        }
    }
    (fields, body)
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

#[cfg(test)]
pub fn parse(text: &str, fallback_name: &str, source: &str) -> Option<Skill> {
    parse_kind(text, fallback_name, source, Kind::Skill)
}

/// `allowed-tools` as Claude Code writes it: `Bash(git:*), Read, Edit` → task-x tool names.
fn allowed_tools(v: &str) -> Vec<String> {
    list(v).iter().map(|t| t.split('(').next().unwrap_or(t).trim().to_owned()).flat_map(|t| super::agents::map_tool(&t)).collect()
}

pub fn parse_kind(text: &str, fallback_name: &str, source: &str, kind: Kind) -> Option<Skill> {
    let (fields, body) = front_matter(text);
    let name = fields.get("name").cloned().unwrap_or_else(|| fallback_name.to_owned());
    if !valid_name(&name) {
        return None;
    }
    let mut body = body.trim().to_owned();
    if body.len() > MAX_SKILL_BYTES {
        let mut cut = MAX_SKILL_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    let get = |k: &str| fields.get(k).map(|v| list(v)).unwrap_or_default();
    let mut tools = get("tools");
    if let Some(a) = fields.get("allowed-tools").or_else(|| fields.get("allowed_tools")) {
        tools.extend(allowed_tools(a));
    }
    let (triggers, patterns) = if kind == Kind::Command {
        (Vec::new(), Vec::new())
    } else {
        (get("triggers").into_iter().map(|t| t.to_lowercase()).collect(), get("patterns"))
    };
    Some(Skill {
        description: fields.get("description").cloned().unwrap_or_default(),
        kind,
        triggers,
        patterns,
        tools,
        params: get("params"),
        argument_hint: fields.get("argument-hint").or_else(|| fields.get("argument_hint")).cloned().filter(|h| !h.is_empty()),
        source: source.to_owned(),
        on_demand: false,
        name,
        body,
    })
}

fn load_dir(dir: &Path, out: &mut BTreeMap<String, Skill>) {
    load_dir_with(dir, ".md", Kind::Skill, "", out)
}

fn load_dir_with(dir: &Path, suffix: &str, kind: Kind, prefix: &str, out: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        let (file, fallback) = if p.is_dir() {
            if kind == Kind::Command {
                // Claude Code namespaces commands by folder: commands/frontend/test.md is /frontend:test.
                load_dir_with(&p, suffix, kind, &format!("{prefix}{fname}:"), out);
                continue;
            }
            (p.join("SKILL.md"), fname)
        } else if let Some(stem) = fname.strip_suffix(suffix) {
            (p.clone(), stem.to_owned())
        } else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&file) else { continue };
        if meta.len() > MAX_SKILL_FILE_BYTES {
            continue;
        }
        if let Some(mut s) = std::fs::read_to_string(&file).ok().and_then(|t| parse_kind(&t, &fallback, &file.display().to_string(), kind)) {
            s.on_demand = kind != Kind::Command && s.triggers.is_empty() && s.patterns.is_empty();
            if !prefix.is_empty() {
                s.name = format!("{prefix}{}", s.name);
            }
            out.insert(s.name.clone(), s);
        }
    }
}

/// Commands that come with task-x. A file with the same name (in any folder) replaces one.
const BUILTIN: &[(&str, &str, &str, &str)] = &[
    (
        "init",
        "Write or update the project instructions file (CLAUDE.md) from the code",
        "[focus]",
        "Create or update the project instructions file for coding agents.\n\n\
1. Find the instructions file: CLAUDE.md at the root, else .github/copilot-instructions.md, else AGENTS.md. If none exists, create CLAUDE.md. If one exists, improve it in place and keep what is still true.\n\
2. Read the manifests, the build and test configuration, the CI workflows, the README, and the main source folders. Run nothing that changes the repository.\n\
3. Write short sections: what the project is (two lines), the layout (folders and what each holds), the exact commands to build, test, lint, and run (copied from the manifests, not guessed), the code conventions you can see in the code, and the traps a newcomer would hit.\n\
4. Keep it under 150 lines. No generic advice, no marketing, nothing you did not verify in the files.\n\
5. Save lasting facts with remember as well.\n\nFocus, if given: $ARGUMENTS",
    ),
    (
        "review",
        "Review the uncommitted changes for defects",
        "[path or focus]",
        "Review the uncommitted changes in this repository (git_diff, then read the changed code around each hunk). Look for bugs, missed edge cases, security problems, broken error handling, and missing tests. Report each finding with file:line, severity, and a one-line fix. Do not edit files. If there are no changes, say so.\n\nFocus: $ARGUMENTS",
    ),
    (
        "security-review",
        "Review the uncommitted changes for security problems",
        "[path or focus]",
        "Do a security review of the uncommitted changes (git_diff, then the code the changes call into). Check injection (SQL, shell, path), XSS, authentication and authorization gaps, secrets in code or logs, unsafe deserialization, SSRF, and missing input validation. Report only real, reachable issues, each with file:line, severity, the attack, and the fix. Do not edit files.\n\nFocus: $ARGUMENTS",
    ),
    (
        "create-skill",
        "Create a skill for Claude Code and GitHub Copilot from a prompt",
        "<what the skill does>",
        "Create a new skill for both Claude Code and GitHub Copilot from the request at the end.\n\n\
1. If the request is empty, ask what the skill must do, and stop.\n\
2. Pick a name: lowercase letters, digits, and hyphens, up to 64 characters. If .claude/skills/<name> or .github/skills/<name> exists, ask before you change it.\n\
3. Read the code the skill is about, so that its commands, paths, and conventions come from this project and are not guesses.\n\
4. Write .claude/skills/<name>/SKILL.md. Front matter: name (the same as the folder) and description, one line of up to 600 characters that says what the skill does and when to use it, with the words a request would contain. Put the description in double quotes if it contains a colon. Body: short imperative instructions, one fact per line, under 100 lines. Move long examples and catalogs to references/<topic>.md in the skill folder, and link them from SKILL.md.\n\
5. Copy every file of the skill, unchanged, to .github/skills/<name>/, so that Copilot reads the same skill.\n\
6. Report the files you wrote.\n\nRequest: $ARGUMENTS",
    ),
    (
        "create-agent",
        "Create a custom agent for Claude Code and GitHub Copilot from a prompt",
        "<what the agent does>",
        "Create a new custom agent for both Claude Code and GitHub Copilot from the request at the end.\n\n\
1. If the request is empty, ask what the agent must do, and stop.\n\
2. Pick a name: lowercase letters, digits, and hyphens, up to 64 characters. If .claude/agents/<name>.md or .github/agents/<name>.agent.md exists, ask before you change it.\n\
3. Read the code the agent will work on, so that its instructions use the commands, paths, and conventions of this project and are not guesses.\n\
4. Write the instructions: its role, its steps, its limits, and what it reports back. Keep them short and imperative. Give it only the tools it needs; an agent that only investigates gets no edit or shell tools.\n\
5. Write .claude/agents/<name>.md. Front matter: name; description, one line that says what the agent does and when to delegate to it, in double quotes if it contains a colon; tools, a comma-separated list from Read, Grep, Glob, Edit, Write, Bash, WebFetch, WebSearch; model: inherit. The instructions follow the front matter.\n\
6. Write .github/agents/<name>.agent.md for Copilot. Front matter: the same name and description; tools, a YAML list such as [\"read\", \"search\"] from read, search, edit, execute, web; no model. The instructions are the same.\n\
7. Report the files you wrote.\n\nRequest: $ARGUMENTS",
    ),
];

/// Skills that come with task-x. They are listed for the model, which loads one when it fits; a
/// file with the same name replaces one.
const BUILTIN_SKILLS: &[(&str, &str, &str)] = &[(
    "vector-design",
    "Design vector graphics as SVG: logos, icons, marks, badges, favicons, simple illustrations. Use it when asked to create, draw, or redesign a logo, icon, or any .svg.",
    "Make the graphic as hand-written SVG, then check it with render_svg until it is right.\n\n\
1. Brief. Take the name, what the brand does, the style words, and the colours from the request. Ask only when the name is missing; otherwise choose and say what you chose.\n\
2. Concept. Pick one simple idea: a monogram, a geometric mark built from the initial, an abstract symbol of what the brand does, or a wordmark. Say it in one line before you draw.\n\
3. Write the file with create_file, at assets/logo/<name>.svg unless the project has a better place:\n\
   - Root: <svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 512 512\" role=\"img\" aria-labelledby=\"title\"> with a <title id=\"title\">. No width or height, so it scales. Use a wide viewBox (such as 0 0 1200 400) for a horizontal lockup.\n\
   - Build on a grid: whole or half numbers, shapes centred, about 8% margin on every side, nothing outside the viewBox.\n\
   - Shapes only: <path>, <circle>, <rect>, <polygon>, <g>, <linearGradient> when asked. No <image>, <script>, <foreignObject>, external links, or embedded fonts.\n\
   - Colours: 1 to 3 flat colours plus one neutral, written as #rrggbb. Put each colour in a fill attribute, not CSS classes, so tools can recolour it.\n\
   - Letters: prefer drawing the letters as paths. If you use <text>, name a common family with a fallback (font-family=\"Inter, Arial, sans-serif\") and say that the user should outline it in a design tool.\n\
   - Keep it small: under 30 elements and 10 KB for a logo; reuse with <use> and <symbol>.\n\
4. Check it with render_svg { path }. Look at the picture: balance, centring, spacing, stroke weight, legibility, and anything clipped. Read every warning and fix it. Render again with background black when the logo must work on dark backgrounds, and with size 32 to see it as a favicon. Repeat until there are no warnings and it looks right; at most 4 rounds.\n\
5. If you cannot see images (render_svg gives no picture, or you are a text-only model), call render_svg with image false and work from contentBox, colors, and warnings.\n\
6. On request, add variants next to it: <name>-mark.svg (symbol only, square), <name>-mono.svg (one colour, currentColor), <name>-dark.svg (for dark backgrounds). For PNGs, favicons, or app icons, call export_png { path, sizes } when the SVG is final.\n\
7. Report the files, the concept, and the colours used. Do not paste the whole SVG into the reply.",
)];

/// The skills that come with task-x: (name, instructions).
pub fn builtin_skills() -> impl Iterator<Item = (&'static str, &'static str)> {
    BUILTIN_SKILLS.iter().map(|(name, _, body)| (*name, *body))
}

fn builtin(out: &mut BTreeMap<String, Skill>) {
    for (name, description, hint, body) in BUILTIN {
        out.insert(
            (*name).to_owned(),
            Skill {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                kind: Kind::Command,
                triggers: Vec::new(),
                patterns: Vec::new(),
                tools: Vec::new(),
                params: Vec::new(),
                argument_hint: Some((*hint).to_owned()),
                source: "built-in".into(),
                on_demand: false,
                body: (*body).to_owned(),
            },
        );
    }
    for (name, description, body) in BUILTIN_SKILLS {
        out.insert(
            (*name).to_owned(),
            Skill {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                kind: Kind::Skill,
                triggers: Vec::new(),
                patterns: Vec::new(),
                tools: Vec::new(),
                params: Vec::new(),
                argument_hint: None,
                source: "built-in".into(),
                on_demand: true,
                body: (*body).to_owned(),
            },
        );
    }
}

/// Every skill, command, and prompt file. `user_dirs` are the user's `skills` folders (their sibling
/// `commands` folders are read too); `plugins` are `(name, path)` of the enabled plugins, whose skills
/// and commands are named `<plugin>:<name>`.
pub fn load(workspace: Option<&Path>, user_dirs: &[PathBuf], plugins: &[(String, PathBuf)]) -> Vec<Skill> {
    let mut out = BTreeMap::new();
    builtin(&mut out);
    for (name, path) in plugins {
        let prefix = format!("{name}:");
        load_dir_with(&path.join("skills"), ".md", Kind::Skill, &prefix, &mut out);
        load_dir_with(&path.join("commands"), ".md", Kind::Command, &prefix, &mut out);
    }
    for u in user_dirs {
        load_dir(u, &mut out);
        if let Some(parent) = u.parent() {
            load_dir_with(&parent.join("commands"), ".md", Kind::Command, "", &mut out);
        }
    }
    if let Some(w) = workspace {
        // .anvil/skills is the old name of .task-x/skills; the newer location wins on a clash.
        for d in [".claude/skills", ".github/skills", ".anvil/skills", ".task-x/skills"] {
            load_dir(&w.join(d), &mut out);
        }
        load_dir_with(&w.join(".github").join("prompts"), ".prompt.md", Kind::Prompt, "", &mut out);
        for d in [".claude/commands", ".task-x/commands"] {
            load_dir_with(&w.join(d), ".md", Kind::Command, "", &mut out);
        }
    }
    out.into_values().collect()
}

pub fn user_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else { return Vec::new() };
    [".claude", ".copilot", ".anvil", ".task-x"].iter().map(|d| Path::new(&home).join(d).join("skills")).collect()
}

const MAX_INDEX_CHARS: usize = 16_000;

/// The on-demand skill list for the system prompt (names and descriptions only).
pub fn index(skills: &[Skill], active: &[Active]) -> Option<String> {
    let mut lines = Vec::new();
    let mut size = 0;
    for s in skills.iter().filter(|s| s.kind != Kind::Command && !active.iter().any(|a| a.name == s.name)) {
        let d: String = s.description.chars().take(140).collect();
        let line = format!("- {}: {}", s.name, if d.is_empty() { "(no description)" } else { &d });
        size += line.len();
        if size > MAX_INDEX_CHARS {
            lines.push("- … more skills omitted".into());
            break;
        }
        lines.push(line);
    }
    (!lines.is_empty()).then(|| {
        format!("<available_skills>\nSkills are reusable instructions for specific tasks. When one fits the request, call read_skill with its name before starting, then follow it.\n{}\n</available_skills>", lines.join("\n"))
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Active {
    pub name: String,
    pub reason: String,
    #[serde(skip)]
    pub text: String,
    pub tools: Vec<String>,
}

/// Parses `/name key=value …` at the start of a prompt. Returns the skill name, params, and the rest.
pub fn explicit(prompt: &str) -> Option<(String, BTreeMap<String, String>, String)> {
    let rest = prompt.trim_start().strip_prefix('/')?;
    let (head, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let mut params = BTreeMap::new();
    let mut remaining = Vec::new();
    for tok in tail.split_whitespace() {
        match tok.split_once('=') {
            Some((k, v)) if remaining.is_empty() && !k.is_empty() => {
                params.insert(k.to_owned(), v.to_owned());
            }
            _ => remaining.push(tok),
        }
    }
    Some((head.to_owned(), params, remaining.join(" ")))
}

/// Everything after `/name` in a prompt, untouched.
fn arguments(prompt: &str) -> &str {
    let rest = prompt.trim_start().strip_prefix('/').unwrap_or_default();
    rest.split_once(char::is_whitespace).map_or("", |(_, t)| t.trim())
}

/// Fills `$ARGUMENTS`, `${ARGUMENTS}`, and `$1`…`$9` in a command body.
fn fill_arguments(body: &str, args: &str) -> String {
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut out = body.replace("${ARGUMENTS}", args).replace("$ARGUMENTS", args);
    for i in (1..=9).rev() {
        out = out.replace(&format!("${i}"), words.get(i - 1).copied().unwrap_or_default());
    }
    out
}

/// The skill a leading `/name` names; a plugin skill also answers to its short name when unique.
fn requested<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name).or_else(|| {
        let mut short = skills.iter().filter(|s| s.name.rsplit(':').next() == Some(name));
        let first = short.next();
        if short.next().is_none() { first } else { None }
    })
}

pub fn select(skills: &[Skill], prompt: &str, paths: &[String]) -> Vec<Active> {
    let lower = prompt.to_lowercase();
    let exp = explicit(prompt);
    let asked = exp.as_ref().and_then(|(n, _, _)| requested(skills, n)).map(|s| s.name.as_str());
    let mut out: Vec<Active> = Vec::new();
    for s in skills {
        let reason = if asked == Some(s.name.as_str()) {
            Some("requested with /".to_owned() + &s.name)
        } else if let Some(t) = s.triggers.iter().find(|t| lower.contains(t.as_str())) {
            Some(format!("mentions '{t}'"))
        } else if !s.patterns.is_empty() && globset(&s.patterns).is_ok_and(|g| paths.iter().any(|p| g.is_match(p))) {
            Some("matches files in the request".into())
        } else {
            None
        };
        let Some(reason) = reason else { continue };
        let mut text = s.body.clone();
        if let Some((_, params, _)) = exp.as_ref().filter(|_| asked == Some(s.name.as_str())) {
            for p in &s.params {
                text = text.replace(&format!("{{{{{p}}}}}"), params.get(p).map_or("(not given)", String::as_str));
            }
            if s.kind == Kind::Command || text.contains("$ARGUMENTS") || text.contains("${ARGUMENTS}") {
                text = fill_arguments(&text, arguments(prompt));
            }
        }
        out.push(Active { name: s.name.clone(), reason, text, tools: s.tools.clone() });
    }
    // Explicit requests first, then the rest in name order; cap how many apply.
    out.sort_by_key(|a| !a.reason.starts_with("requested"));
    out.truncate(MAX_ACTIVE);
    out
}

/// The skills that apply to a prompt, with their expanded instructions and source file
/// (for clients that run another agent, such as the Claude Code CLI).
pub fn resolve(skills: &[Skill], prompt: &str, paths: &[String]) -> Vec<serde_json::Value> {
    select(skills, prompt, paths)
        .into_iter()
        .map(|a| {
            let source = skills.iter().find(|s| s.name == a.name).map_or("", |s| s.source.as_str());
            serde_json::json!({ "name": a.name, "reason": a.reason, "source": source, "text": a.text })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_expands_explicit_and_triggered_skills() {
        let mut s = parse(DOC, "x", "/w/.anvil/skills/db-migration.md").unwrap();
        s.on_demand = false;
        let r = resolve(&[s.clone()], "/db-migration table=users add a column", &[]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["source"], "/w/.anvil/skills/db-migration.md");
        assert!(r[0]["reason"].as_str().unwrap().starts_with("requested"));
        assert!(r[0]["text"].as_str().unwrap().contains("migration for users"));
        assert_eq!(resolve(&[s.clone()], "write a schema change", &[])[0]["reason"], "mentions 'schema change'");
        assert!(resolve(&[s], "rename a variable", &[]).is_empty());
    }

    const DOC: &str = "---\nname: db-migration\ndescription: Reversible migrations\ntriggers: migration, schema change\npatterns: migrations/**\ntools: read_file, create_file\nparams: table\n---\nCreate a migration for {{table}} with up and down steps.\n";

    #[test]
    fn parses_front_matter_and_selects_by_trigger_pattern_and_command() {
        let s = parse(DOC, "x", "mem").unwrap();
        assert_eq!((s.name.as_str(), s.tools.len(), s.params.as_slice()), ("db-migration", 2, ["table".to_string()].as_slice()));
        let all = vec![s, parse("Always write docstrings.", "docs", "mem").unwrap()];
        assert_eq!(select(&all, "add a schema change for users", &[])[0].name, "db-migration");
        assert_eq!(select(&all, "tweak this", &["migrations/001.sql".into()])[0].reason, "matches files in the request");
        let a = select(&all, "/db-migration table=users please", &[]);
        assert!(a[0].text.contains("migration for users"));
        assert!(select(&all, "unrelated request", &["src/a.rs".into()]).is_empty());
        assert!(parse("---\nname: bad name!\n---\nx", "f", "mem").is_none());
    }

    #[test]
    fn commands_fill_arguments_and_stay_out_of_the_index() {
        let base = std::env::temp_dir().join(format!("task-x-commands-{}", std::process::id()));
        let (user, ws, plug) = (base.join("home/.claude/skills"), base.join("ws"), base.join("plug"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(base.join("home/.claude/commands")).unwrap();
        std::fs::write(base.join("home/.claude/commands/fix.md"), "---\ndescription: Fix an issue\nargument-hint: [issue]\nallowed-tools: Bash(git:*), Read\n---\nFix issue $1 then $ARGUMENTS").unwrap();
        std::fs::create_dir_all(ws.join(".claude/commands/frontend")).unwrap();
        std::fs::write(ws.join(".claude/commands/frontend/test.md"), "Test ${ARGUMENTS}").unwrap();
        std::fs::create_dir_all(plug.join("commands")).unwrap();
        std::fs::create_dir_all(plug.join("skills/audit")).unwrap();
        std::fs::write(plug.join("commands/seo-audit.md"), "Audit $ARGUMENTS").unwrap();
        std::fs::write(plug.join("skills/audit/SKILL.md"), "---\ndescription: audit\n---\nAudit site").unwrap();
        let s: Vec<Skill> = load(Some(&ws), &[user], &[("seo".into(), plug)]).into_iter().filter(|x| x.source != "built-in").collect();
        let names: Vec<&str> = s.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["fix", "frontend:test", "seo:audit", "seo:seo-audit"]);
        let fix = &s[0];
        assert_eq!((fix.kind, fix.argument_hint.as_deref(), fix.on_demand), (Kind::Command, Some("[issue]"), false));
        assert!(fix.tools.contains(&"run_command".to_string()) && fix.tools.contains(&"read_file".to_string()));
        let a = select(&s, "/fix 42 in the login form", &[]);
        assert_eq!(a[0].text, "Fix issue 42 then 42 in the login form");
        assert_eq!(select(&s, "/frontend:test the header", &[])[0].text, "Test the header");
        assert_eq!(select(&s, "/seo-audit example.com", &[])[0].name, "seo:seo-audit", "short plugin name");
        assert_eq!(select(&s, "/seo:audit", &[])[0].name, "seo:audit");
        assert!(select(&s, "please fix issue 42", &[]).is_empty(), "commands never apply by themselves");
        let idx = index(&s, &[]).unwrap();
        assert!(idx.contains("- seo:audit") && !idx.contains("fix") && !idx.contains("frontend:test"));
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn built_in_commands_fill_their_arguments() {
        let s = load(None, &[], &[]);
        let init = select(&s, "/init the runtime folder", &[]);
        assert_eq!(init[0].name, "init");
        assert!(init[0].text.contains("CLAUDE.md") && init[0].text.ends_with("Focus, if given: the runtime folder"));
        assert!(s.iter().any(|x| x.name == "security-review" && x.kind == Kind::Command));
        let idx = index(&s, &[]).unwrap();
        assert!(!idx.contains("- init:") && !idx.contains("- review:"), "built-in commands are not listed for the model: {idx}");
        assert!(idx.contains("- vector-design: Design vector graphics"), "the built-in skill is listed: {idx}");
        let v = select(&s, "/vector-design a fox logo", &[]);
        assert!(v[0].text.contains("render_svg"), "it names the tool that checks the drawing");
    }

    #[test]
    fn task_x_skills_override_the_old_anvil_folder() {
        let ws = std::env::temp_dir().join(format!("task-x-skill-dirs-{}", std::process::id()));
        for (d, body) in [(".anvil/skills", "old"), (".task-x/skills", "new")] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
            std::fs::write(ws.join(d).join("lint.md"), body).unwrap();
        }
        std::fs::create_dir_all(ws.join(".anvil/skills/legacy")).unwrap();
        std::fs::write(ws.join(".anvil/skills/legacy/SKILL.md"), "still read").unwrap();
        let s = load(Some(&ws), &[], &[]);
        assert_eq!(s.iter().find(|x| x.name == "lint").unwrap().body, "new");
        assert!(s.iter().any(|x| x.name == "legacy"), "the old folder still works");
        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn large_skill_files_are_listed() {
        let dir = std::env::temp_dir().join(format!("anvil-big-skill-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("design")).unwrap();
        let body = "x".repeat(70 * 1024);
        std::fs::write(dir.join("design/SKILL.md"), format!("---\nname: design\ndescription: UI\n---\n{body}")).unwrap();
        let s: Vec<Skill> = load(None, std::slice::from_ref(&dir), &[]).into_iter().filter(|x| x.source != "built-in").collect();
        assert_eq!(s.len(), 1, "a 70 KB skill must not be skipped");
        assert_eq!(s[0].body.len(), body.len());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn loads_workspace_skills_over_user_skills() {
        let base = std::env::temp_dir().join(format!("anvil-skills-{}", std::process::id()));
        let user = base.join("user");
        let ws = base.join("ws");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(ws.join(".anvil/skills/review")).unwrap();
        std::fs::write(user.join("review.md"), "user version").unwrap();
        std::fs::write(ws.join(".anvil/skills/review/SKILL.md"), "---\ndescription: ws\n---\nworkspace version").unwrap();
        std::fs::create_dir_all(ws.join(".github/prompts")).unwrap();
        std::fs::create_dir_all(ws.join(".github/skills/pdf")).unwrap();
        std::fs::write(ws.join(".github/prompts/release.prompt.md"), "---\ndescription: Prepare a release\n---\nBump the version.")
            .unwrap();
        std::fs::write(ws.join(".github/skills/pdf/SKILL.md"), "---\nname: pdf\ndescription: Work with PDF files\n---\nUse pdftotext.")
            .unwrap();
        let s = load(Some(&ws), std::slice::from_ref(&user), &[]);
        let names: Vec<&str> = s.iter().filter(|x| x.source != "built-in" || x.name == "review").map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["pdf", "release", "review"], "the user's review replaces the built-in one");
        let s: Vec<Skill> = s.into_iter().filter(|x| x.source != "built-in").collect();
        assert_eq!(s[2].body, "workspace version");
        assert!(s.iter().all(|x| x.on_demand));
        assert_eq!((s[0].kind, s[1].kind), (Kind::Skill, Kind::Prompt));
        let idx = index(&s, &[]).unwrap();
        assert!(idx.contains("- pdf: Work with PDF files") && idx.contains("- release: Prepare a release"));
        assert!(select(&s, "/release now", &[])[0].text.contains("Bump the version."));
        std::fs::remove_dir_all(base).ok();
    }
}
