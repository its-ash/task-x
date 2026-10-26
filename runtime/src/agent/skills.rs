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

use crate::context::index::globset;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_SKILL_BYTES: usize = 96 * 1024;
/// Larger files are skipped; the body is cut to `MAX_SKILL_BYTES`.
const MAX_SKILL_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_ACTIVE: usize = 3;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub patterns: Vec<String>,
    pub tools: Vec<String>,
    pub params: Vec<String>,
    pub source: String,
    /// Skills with no triggers or patterns are listed for the model, which loads them on demand.
    pub on_demand: bool,
    #[serde(skip)]
    pub body: String,
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
}

pub fn parse(text: &str, fallback_name: &str, source: &str) -> Option<Skill> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (meta, body) = match text.strip_prefix("---") {
        Some(rest) => {
            let end = rest.find("\n---")?;
            (&rest[..end], rest[end + 4..].trim_start_matches(['-', '\r', '\n']))
        }
        None => ("", text),
    };
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for line in meta.lines() {
        if let Some((k, v)) = line.split_once(':') {
            fields.insert(k.trim().to_ascii_lowercase(), v.trim().trim_matches('"').to_owned());
        }
    }
    let name = fields.get("name").cloned().unwrap_or_else(|| fallback_name.to_owned());
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
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
    Some(Skill {
        description: fields.get("description").cloned().unwrap_or_default(),
        triggers: get("triggers").into_iter().map(|t| t.to_lowercase()).collect(),
        patterns: get("patterns"),
        tools: get("tools"),
        params: get("params"),
        source: source.to_owned(),
        on_demand: false,
        name,
        body,
    })
}

fn load_dir(dir: &Path, out: &mut BTreeMap<String, Skill>) {
    load_dir_with(dir, ".md", out)
}

fn load_dir_with(dir: &Path, suffix: &str, out: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        let (file, fallback) = if p.is_dir() {
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
        if let Some(mut s) = std::fs::read_to_string(&file).ok().and_then(|t| parse(&t, &fallback, &file.display().to_string())) {
            s.on_demand = s.triggers.is_empty() && s.patterns.is_empty();
            out.insert(s.name.clone(), s);
        }
    }
}

pub fn load(workspace: Option<&Path>, user_dirs: &[PathBuf]) -> Vec<Skill> {
    let mut out = BTreeMap::new();
    for u in user_dirs {
        load_dir(u, &mut out);
    }
    if let Some(w) = workspace {
        // .anvil/skills is the old name of .task-x/skills; the newer location wins on a clash.
        for d in [".claude/skills", ".github/skills", ".anvil/skills", ".task-x/skills"] {
            load_dir(&w.join(d), &mut out);
        }
        load_dir_with(&w.join(".github").join("prompts"), ".prompt.md", &mut out);
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
    for s in skills.iter().filter(|s| !active.iter().any(|a| a.name == s.name)) {
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

pub fn select(skills: &[Skill], prompt: &str, paths: &[String]) -> Vec<Active> {
    let lower = prompt.to_lowercase();
    let exp = explicit(prompt);
    let mut out: Vec<Active> = Vec::new();
    for s in skills {
        let reason = if exp.as_ref().is_some_and(|(n, _, _)| *n == s.name) {
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
        if let Some((_, params, _)) = exp.as_ref().filter(|(n, _, _)| *n == s.name) {
            for p in &s.params {
                text = text.replace(&format!("{{{{{p}}}}}"), params.get(p).map_or("(not given)", String::as_str));
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
    fn task_x_skills_override_the_old_anvil_folder() {
        let ws = std::env::temp_dir().join(format!("task-x-skill-dirs-{}", std::process::id()));
        for (d, body) in [(".anvil/skills", "old"), (".task-x/skills", "new")] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
            std::fs::write(ws.join(d).join("lint.md"), body).unwrap();
        }
        std::fs::create_dir_all(ws.join(".anvil/skills/legacy")).unwrap();
        std::fs::write(ws.join(".anvil/skills/legacy/SKILL.md"), "still read").unwrap();
        let s = load(Some(&ws), &[]);
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
        let s = load(None, std::slice::from_ref(&dir));
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
        let s = load(Some(&ws), std::slice::from_ref(&user));
        let names: Vec<&str> = s.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["pdf", "release", "review"]);
        assert_eq!(s[2].body, "workspace version");
        assert!(s.iter().all(|x| x.on_demand));
        let idx = index(&s, &[]).unwrap();
        assert!(idx.contains("- pdf: Work with PDF files") && idx.contains("- release: Prepare a release"));
        assert!(select(&s, "/release now", &[])[0].text.contains("Bump the version."));
        std::fs::remove_dir_all(base).ok();
    }
}
