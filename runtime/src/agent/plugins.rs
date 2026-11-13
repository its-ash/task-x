//! Plugins: folders that bundle skills, agents, commands, and MCP servers, in the Claude Code plugin
//! layout (`.claude-plugin/plugin.json`, `skills/`, `agents/`, `commands/`, `.mcp.json`).
//!
//! Sources, lowest precedence first:
//! - plugins Claude Code installed (`~/.claude/plugins/installed_plugins.json`), enabled per
//!   `enabledPlugins` in `~/.claude/settings.json` (a missing entry counts as enabled);
//! - `~/.task-x/plugins/<name>/` and `.task-x/plugins/<name>/` in the workspace;
//! - extra folders the client passes (`plugins/configure`).
//!
//! A `.task-x/plugins.json` file in the workspace (`{ "disabled": ["name"] }`) turns plugins off.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_PLUGINS: usize = 200;

/// What the client adds (`plugins/configure`): extra plugin folders and names to turn off.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// The client trusts the workspace: its plugins' hooks and MCP servers may run.
    #[serde(default)]
    pub workspace_trusted: bool,
}
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    /// Short name, as skills and agents are prefixed (`<name>:<skill>`).
    pub name: String,
    /// Claude Code's id (`name@marketplace`) or the short name for local plugins.
    pub id: String,
    pub version: String,
    pub description: String,
    pub path: PathBuf,
    pub enabled: bool,
    /// `claude`, `user`, `workspace`, or `client`.
    pub source: String,
    pub skills: usize,
    pub agents: usize,
    pub commands: usize,
    /// MCP servers from `.mcp.json` or the manifest, in Claude Code's `mcpServers` format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Value>,
}

/// Names a plugin folder may have.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn read_json(p: &Path) -> Option<Value> {
    let meta = std::fs::metadata(p).ok()?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn count(dir: &Path, f: impl Fn(&std::fs::DirEntry) -> bool) -> usize {
    std::fs::read_dir(dir).map(|e| e.flatten().filter(|e| f(e)).count()).unwrap_or(0)
}

/// Reads one plugin folder. `id` is Claude Code's id when known.
fn read(path: &Path, id: Option<&str>, source: &str, enabled: bool) -> Option<Plugin> {
    if !path.is_dir() {
        return None;
    }
    let manifest = read_json(&path.join(".claude-plugin").join("plugin.json")).unwrap_or(Value::Null);
    let folder = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let short = id.map(|i| i.split('@').next().unwrap_or(i)).or_else(|| manifest["name"].as_str()).unwrap_or(folder).to_owned();
    if !valid_name(&short) {
        return None;
    }
    let mcp = read_json(&path.join(".mcp.json"))
        .and_then(|v| if v.get("mcpServers").is_some() { v.get("mcpServers").cloned() } else { Some(v) })
        .or_else(|| manifest.get("mcpServers").cloned())
        .filter(|v| v.is_object() && !v.as_object().is_some_and(|o| o.is_empty()));
    Some(Plugin {
        id: id.map(str::to_owned).unwrap_or_else(|| short.clone()),
        version: manifest["version"].as_str().unwrap_or_default().to_owned(),
        description: manifest["description"].as_str().unwrap_or_default().chars().take(200).collect(),
        path: path.to_path_buf(),
        enabled,
        source: source.to_owned(),
        skills: count(&path.join("skills"), |e| e.path().join("SKILL.md").is_file()),
        agents: count(&path.join("agents"), |e| e.path().extension().is_some_and(|x| x == "md")),
        commands: count(&path.join("commands"), |e| e.path().extension().is_some_and(|x| x == "md")),
        mcp_servers: mcp,
        name: short,
    })
}

/// Plugins Claude Code installed: the newest install of each id, enabled per `settings.json`.
fn claude_plugins(home: &Path, out: &mut BTreeMap<String, Plugin>) {
    let Some(installed) = read_json(&home.join(".claude").join("plugins").join("installed_plugins.json")) else { return };
    let enabled = read_json(&home.join(".claude").join("settings.json")).and_then(|s| s.get("enabledPlugins").cloned()).unwrap_or(Value::Null);
    let Some(map) = installed["plugins"].as_object() else { return };
    for (id, entries) in map {
        let entry = match entries {
            Value::Array(a) => a.last(),
            v @ Value::Object(_) => Some(v),
            _ => None,
        };
        let Some(path) = entry.and_then(|e| e["installPath"].as_str()) else { continue };
        let on = enabled[id.as_str()].as_bool().unwrap_or(true);
        if let Some(p) = read(Path::new(path), Some(id), "claude", on) {
            out.insert(p.name.clone(), p);
        }
    }
}

fn local_plugins(dir: &Path, source: &str, disabled: &[String], out: &mut BTreeMap<String, Plugin>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        // `.new-*` and `.tmp-*` are installs in progress.
        if name.starts_with('.') {
            continue;
        }
        let on = !disabled.iter().any(|d| d == name);
        if let Some(pl) = read(&p, None, source, on) {
            out.insert(pl.name.clone(), pl);
        }
    }
}

/// Plugins the workspace turned off (`.task-x/plugins.json`).
pub fn workspace_disabled(root: &Path) -> Vec<String> {
    read_json(&root.join(".task-x").join("plugins.json"))
        .and_then(|v| v["disabled"].as_array().cloned())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Every plugin found. `extra` are folders the client named; `disabled` names apply to all sources.
pub fn load(workspace: Option<&Path>, home: Option<&Path>, extra: &[PathBuf], disabled: &[String]) -> Vec<Plugin> {
    let mut out = BTreeMap::new();
    if let Some(h) = home {
        claude_plugins(h, &mut out);
        local_plugins(&h.join(".task-x").join("plugins"), "user", disabled, &mut out);
    }
    let mut off = disabled.to_vec();
    if let Some(w) = workspace {
        off.extend(workspace_disabled(w));
        local_plugins(&w.join(".task-x").join("plugins"), "workspace", &off, &mut out);
    }
    for e in extra.iter().filter(|e| e.is_absolute()) {
        if let Some(p) = read(e, None, "client", true) {
            out.insert(p.name.clone(), p);
        }
    }
    for p in out.values_mut() {
        if off.iter().any(|d| *d == p.name || *d == p.id) {
            p.enabled = false;
        }
    }
    out.into_values().take(MAX_PLUGINS).collect()
}

/// `(name, path)` of the enabled plugins, for the skill, agent, and command loaders.
/// The folders of the enabled plugins. A workspace's own plugins count only in a trusted workspace:
/// their agents and skills shape what the agent does, and a cloned repository could ship them.
pub fn enabled_dirs(plugins: &[Plugin], workspace_trusted: bool) -> Vec<(String, PathBuf)> {
    plugins.iter().filter(|p| p.enabled && (workspace_trusted || p.source != "workspace")).map(|p| (p.name.clone(), p.path.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_dir(base: &Path, name: &str, manifest: &str) -> PathBuf {
        let p = base.join(name);
        std::fs::create_dir_all(p.join(".claude-plugin")).unwrap();
        std::fs::write(p.join(".claude-plugin/plugin.json"), manifest).unwrap();
        std::fs::create_dir_all(p.join("skills/audit")).unwrap();
        std::fs::write(p.join("skills/audit/SKILL.md"), "---\ndescription: audit\n---\nrun the audit").unwrap();
        std::fs::create_dir_all(p.join("agents")).unwrap();
        std::fs::write(p.join("agents/checker.md"), "---\ndescription: checks\n---\ncheck").unwrap();
        std::fs::create_dir_all(p.join("commands")).unwrap();
        std::fs::write(p.join("commands/fix.md"), "Fix $ARGUMENTS").unwrap();
        p
    }

    #[test]
    fn finds_claude_installed_and_local_plugins() {
        let base = std::env::temp_dir().join(format!("task-x-plugins-{}", std::process::id()));
        let home = base.join("home");
        let ws = base.join("ws");
        let cache = plugin_dir(&home.join(".claude/plugins/cache/mk/seo"), "1.0.0", r#"{ "name": "seo", "version": "1.0.0", "description": "SEO tools" }"#);
        std::fs::write(cache.join(".mcp.json"), r#"{ "mcpServers": { "dataforseo": { "command": "npx", "args": ["x"] } } }"#).unwrap();
        let off = plugin_dir(&home.join(".claude/plugins/cache/mk/old"), "2.0.0", r#"{ "name": "old" }"#);
        std::fs::write(
            home.join(".claude/plugins/installed_plugins.json"),
            serde_json::json!({ "version": 2, "plugins": {
                "seo@mk": [{ "installPath": cache.to_string_lossy(), "version": "1.0.0" }],
                "old@mk": [{ "installPath": off.to_string_lossy(), "version": "2.0.0" }]
            } })
            .to_string(),
        )
        .unwrap();
        std::fs::write(home.join(".claude/settings.json"), r#"{ "enabledPlugins": { "seo@mk": true, "old@mk": false } }"#).unwrap();
        plugin_dir(&home.join(".task-x/plugins"), "mine", r#"{ "description": "local" }"#);
        plugin_dir(&ws.join(".task-x/plugins"), "team", "{}");
        std::fs::write(ws.join(".task-x/plugins.json"), r#"{ "disabled": ["team"] }"#).unwrap();
        let all = load(Some(&ws), Some(&home), &[], &[]);
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["mine", "old", "seo", "team"]);
        let seo = all.iter().find(|p| p.name == "seo").unwrap();
        assert!(seo.enabled && seo.id == "seo@mk" && seo.version == "1.0.0" && seo.source == "claude");
        assert_eq!((seo.skills, seo.agents, seo.commands), (1, 1, 1));
        assert!(seo.mcp_servers.as_ref().unwrap()["dataforseo"].is_object());
        assert!(!all.iter().find(|p| p.name == "old").unwrap().enabled, "settings.json turns it off");
        assert!(!all.iter().find(|p| p.name == "team").unwrap().enabled, ".task-x/plugins.json turns it off");
        assert_eq!(all.iter().find(|p| p.name == "mine").unwrap().source, "user");
        let on = enabled_dirs(&all, true);
        assert_eq!(on.len(), 2);
        assert!(on.iter().any(|(n, _)| n == "seo") && on.iter().any(|(n, _)| n == "mine"));
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn ignores_folders_with_bad_names_and_missing_homes() {
        let base = std::env::temp_dir().join(format!("task-x-plugins-bad-{}", std::process::id()));
        std::fs::create_dir_all(base.join(".task-x/plugins/bad name")).unwrap();
        assert!(load(Some(&base), None, &[], &[]).is_empty());
        assert!(load(None, Some(&base.join("nohome")), &[], &[]).is_empty());
        std::fs::remove_dir_all(base).ok();
    }
}
