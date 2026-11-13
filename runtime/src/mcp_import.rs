//! Finds the MCP servers the user already set up for Claude (Claude Desktop, Claude Code), VS Code
//! (Copilot chat), Copilot CLI, and Cursor, so task-x starts the same servers with no second setup.
//!
//! Files are read from the most general to the most specific, and a later file wins for a name:
//! Claude Desktop, `~/.claude.json` (user, then this project), Copilot CLI, Cursor, VS Code's user
//! `mcp.json` and its `mcp.servers` setting, then (in a trusted workspace only) the workspace's
//! `.mcp.json`, `.vscode/mcp.json`, and `.cursor/mcp.json`. Missing files are skipped quietly;
//! broken files and entries are skipped with a note.

use crate::mcp::ServerConfig;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The sources, as named in `mcp/configure { import }` and the `--mcp-import` option.
pub const SOURCES: [&str; 4] = ["claude", "vscode", "copilotCli", "cursor"];
const MAX_SERVERS: usize = 50;

/// Where to look, and the values `${…}` variables expand to.
pub struct Env {
    pub home: PathBuf,
    pub root: Option<PathBuf>,
    /// Workspace files run commands from the repository, so they count only in a trusted workspace.
    pub trusted: bool,
    pub vars: HashMap<String, String>,
    pub os: &'static str,
}

impl Env {
    pub fn current(home: PathBuf, root: Option<PathBuf>, trusted: bool) -> Self {
        Self { home, root, trusted, vars: std::env::vars().collect(), os: std::env::consts::OS }
    }

    /// `%APPDATA%` on Windows, `~/Library/Application Support` on macOS, `~/.config` elsewhere.
    fn app_dir(&self) -> PathBuf {
        match self.os {
            "macos" => self.home.join("Library").join("Application Support"),
            "windows" => self.vars.get("APPDATA").map_or_else(|| self.home.join("AppData").join("Roaming"), PathBuf::from),
            _ => self.vars.get("XDG_CONFIG_HOME").map_or_else(|| self.home.join(".config"), PathBuf::from),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    pub servers: BTreeMap<String, ServerConfig>,
    /// Where each server came from: `Copilot CLI (~/.copilot/mcp-config.json)`.
    pub origins: BTreeMap<String, String>,
    /// What was found, renamed, or skipped, and why.
    pub notes: Vec<String>,
}

type Pick = fn(&Value, &Env) -> Option<Value>;

fn pick_key(key: &'static str) -> impl Fn(&Value) -> Option<Value> {
    move |j: &Value| j.get(key).filter(|v| v.is_object()).cloned()
}

/// `~/.claude.json`: the user's servers, then this project's.
fn claude_code(j: &Value, env: &Env) -> Option<Value> {
    let mut all = j.get("mcpServers").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(p) = env.root.as_ref().and_then(|r| j["projects"][r.to_string_lossy().as_ref()]["mcpServers"].as_object()) {
        all.extend(p.clone());
    }
    Some(Value::Object(all))
}

fn mcp_servers(j: &Value, _: &Env) -> Option<Value> {
    pick_key("mcpServers")(j)
}

fn servers(j: &Value, _: &Env) -> Option<Value> {
    pick_key("servers")(j)
}

/// VS Code's older setting: `"mcp": { "servers": … }`, or `"mcp.servers"`.
fn vscode_setting(j: &Value, _: &Env) -> Option<Value> {
    j["mcp"]["servers"].as_object().or_else(|| j["mcp.servers"].as_object()).map(|o| Value::Object(o.clone()))
}

/// The files to read, from the most general to the most specific: (source, label, path, pick).
fn files(env: &Env, sources: &[String]) -> Vec<(&'static str, String, PathBuf, Pick)> {
    let mut out: Vec<(&'static str, String, PathBuf, Pick)> = Vec::new();
    let mut add = |source: &'static str, label: &str, path: PathBuf, pick: Pick, workspace: bool| {
        if sources.iter().any(|s| s == source) && (!workspace || env.trusted) {
            out.push((source, label.to_owned(), path, pick));
        }
    };
    let app = env.app_dir();
    add("claude", "Claude Desktop", app.join("Claude").join("claude_desktop_config.json"), mcp_servers, false);
    add("claude", "Claude Code", env.home.join(".claude.json"), claude_code, false);
    add("copilotCli", "Copilot CLI", env.home.join(".copilot").join("mcp-config.json"), mcp_servers, false);
    add("cursor", "Cursor", env.home.join(".cursor").join("mcp.json"), mcp_servers, false);
    for (app_name, label) in [("Code", "VS Code"), ("Code - Insiders", "VS Code Insiders")] {
        let user = app.join(app_name).join("User");
        add("vscode", label, user.join("mcp.json"), servers, false);
        add("vscode", &format!("{label} settings"), user.join("settings.json"), vscode_setting, false);
    }
    if let Some(root) = &env.root {
        add("claude", "Claude Code project", root.join(".mcp.json"), mcp_servers, true);
        add("vscode", "VS Code workspace", root.join(".vscode").join("mcp.json"), servers, true);
        add("cursor", "Cursor workspace", root.join(".cursor").join("mcp.json"), mcp_servers, true);
    }
    out
}

/// JSON, or JSON with comments and trailing commas (VS Code's settings and `mcp.json`).
pub fn parse_jsonc(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for n in chars.by_ref() {
                    if last == '*' && n == '/' {
                        break;
                    }
                    last = n;
                }
            }
            _ => out.push(c),
        }
    }
    static TRAILING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r",(\s*[}\]])").expect("trailing comma regex"));
    serde_json::from_str(&TRAILING.replace_all(&out, "$1")).ok()
}

/// Fills `${workspaceFolder}`, `${userHome}`, `${env:NAME}`, and Claude's `${NAME}` / `${NAME:-default}`.
/// `None` when the value asks for an `${input:…}` prompt, which only VS Code can show.
fn expand(s: &str, env: &Env) -> Option<String> {
    static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([^}]+)\}").expect("var regex"));
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\w+)(?::-(.*))?$").expect("named var regex"));
    if s.contains("${input:") {
        return None;
    }
    Some(
        VAR.replace_all(s, |c: &regex::Captures| {
            let key = &c[1];
            if key == "workspaceFolder" {
                return env.root.as_ref().map_or_else(|| c[0].to_owned(), |r| r.to_string_lossy().into_owned());
            }
            if key == "userHome" {
                return env.home.to_string_lossy().into_owned();
            }
            if let Some(name) = key.strip_prefix("env:") {
                return env.vars.get(name).cloned().unwrap_or_default();
            }
            match NAMED.captures(key) {
                Some(m) => env.vars.get(&m[1]).cloned().or_else(|| m.get(2).map(|d| d.as_str().to_owned())).unwrap_or_default(),
                None => c[0].to_owned(),
            }
        })
        .into_owned(),
    )
}

/// One entry, checked and with its variables filled in; `Err` says why it was skipped.
fn normalize(raw: &Value, env: &Env) -> Result<ServerConfig, String> {
    let o = raw.as_object().ok_or("not an object")?;
    let s = |k: &str| o.get(k).and_then(Value::as_str);
    let mut needs_input = false;
    let mut x = |v: &str| {
        expand(v, env).unwrap_or_else(|| {
            needs_input = true;
            String::new()
        })
    };
    let command = s("command").unwrap_or_default();
    let url = s("url").or_else(|| s("serverUrl"));
    if command.trim().is_empty() && url.is_none() {
        return Err("no command or url".into());
    }
    // Copilot CLI calls a local server "local"; Claude and VS Code call it "stdio".
    let transport = match s("type") {
        None => None,
        Some("stdio" | "local") => Some("stdio"),
        Some("http" | "streamable-http" | "streamableHttp") => Some("http"),
        Some("sse") => Some("sse"),
        Some(t) => return Err(format!("unsupported type \"{t}\"")),
    };
    let map = |v: Option<&Value>, x: &mut dyn FnMut(&str) -> String| -> BTreeMap<String, String> {
        v.and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), x(v)))).collect()).unwrap_or_default()
    };
    let cfg = ServerConfig {
        transport: transport.map(str::to_owned),
        command: x(command),
        args: o.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(&mut x).collect()).unwrap_or_default(),
        env: map(o.get("env"), &mut x),
        cwd: s("cwd").map(|c| PathBuf::from(x(c))),
        timeout_ms: o.get("timeout").and_then(Value::as_u64),
        disabled: o.get("disabled") == Some(&Value::Bool(true)),
        url: url.map(&mut x),
        headers: map(o.get("headers"), &mut x),
        oauth: None,
    };
    if needs_input {
        return Err("it asks for an ${input:…} value; copy it into task-x's own MCP settings with the value filled in".into());
    }
    Ok(cfg)
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A usable name for a registry-style one: `io.github.ChromeDevTools/chrome-devtools-mcp` →
/// `chrome-devtools`. Long tool names stay readable, and the server's tools keep short names.
fn short_name(n: &str) -> Option<String> {
    let last = n.rsplit('/').next().unwrap_or(n);
    let base = last.strip_suffix("-mcp").or_else(|| last.strip_suffix("_mcp")).filter(|b| !b.is_empty()).unwrap_or(last);
    let clean: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '-' }).collect();
    let clean = clean.trim_matches('-').chars().take(64).collect::<String>();
    valid_name(&clean).then_some(clean)
}

/// Reads every enabled source with `read` (a file's text, `None` when it is missing).
pub fn import(sources: &[String], env: &Env, read: &dyn Fn(&Path) -> Option<String>) -> Imported {
    let mut out = Imported::default();
    for (_, label, path, pick) in files(env, sources) {
        let Some(text) = read(&path) else { continue };
        let shown = path.strip_prefix(&env.home).map_or_else(|_| path.display().to_string(), |p| format!("~/{}", p.display()));
        let Some(json) = parse_jsonc(&text) else {
            out.notes.push(format!("{label}: could not parse {shown}"));
            continue;
        };
        let Some(entries) = pick(&json, env) else { continue };
        let mut n = 0;
        for (raw_name, raw) in entries.as_object().into_iter().flatten() {
            let name = if valid_name(raw_name) { Some(raw_name.clone()) } else { short_name(raw_name) };
            let Some(name) = name.filter(|n| n != crate::mcp_server::SERVER_NAME) else {
                out.notes.push(format!("{label}: skipped \"{raw_name}\" (no usable name)"));
                continue;
            };
            if name != *raw_name {
                out.notes.push(format!("{label}: \"{raw_name}\" is named \"{name}\" here"));
            }
            match normalize(raw, env) {
                Err(why) => out.notes.push(format!("{label}: skipped \"{raw_name}\": {why}")),
                Ok(_) if !out.servers.contains_key(&name) && out.servers.len() >= MAX_SERVERS => {
                    out.notes.push(format!("{label}: skipped \"{raw_name}\": more than {MAX_SERVERS} servers"))
                }
                Ok(cfg) => {
                    out.servers.insert(name.clone(), cfg);
                    out.origins.insert(name, format!("{label} ({shown})"));
                    n += 1;
                }
            }
        }
        if n > 0 {
            out.notes.push(format!("{label}: {n} server{}", if n == 1 { "" } else { "s" }));
        }
    }
    out
}

/// Reads the files from disk.
pub fn read_file(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(trusted: bool) -> Env {
        Env {
            home: PathBuf::from("/h"),
            root: Some(PathBuf::from("/w")),
            trusted,
            vars: HashMap::from([("TOKEN".to_string(), "t0k".to_string())]),
            os: "macos",
        }
    }

    fn fake(files: Vec<(&str, String)>) -> impl Fn(&Path) -> Option<String> {
        let files: HashMap<PathBuf, String> = files.into_iter().map(|(p, t)| (PathBuf::from(p), t)).collect();
        move |p: &Path| files.get(p).cloned()
    }

    fn all() -> Vec<String> {
        SOURCES.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn imports_claude_vscode_copilot_and_cursor() {
        let read = fake(vec![
            ("/h/.claude.json", json!({ "mcpServers": { "neon": { "type": "http", "url": "https://mcp.neon.tech/mcp" } }, "projects": { "/w": { "mcpServers": { "hostinger": { "command": "npx", "args": ["h"] } } }, "/other": { "mcpServers": { "nope": { "command": "x" } } } } }).to_string()),
            ("/h/.copilot/mcp-config.json", json!({ "mcpServers": { "io.github.ChromeDevTools/chrome-devtools-mcp": { "type": "local", "command": "npx", "args": ["-y", "chrome-devtools-mcp@latest"], "tools": ["*"] } } }).to_string()),
            ("/h/.cursor/mcp.json", json!({ "mcpServers": { "neon": { "command": "cursor-neon" } } }).to_string()),
            ("/h/Library/Application Support/Code/User/mcp.json", "{ // comments\n \"servers\": { \"gh\": { \"type\": \"http\", \"url\": \"https://api.github.com/mcp\", \"headers\": { \"Authorization\": \"Bearer ${env:TOKEN}\" } }, }, }".into()),
            ("/h/Library/Application Support/Code/User/settings.json", "{ \"mcp\": { \"servers\": { \"old\": { \"command\": \"${userHome}/bin/old\", \"args\": [\"${workspaceFolder}\"] } } } }".into()),
            ("/w/.vscode/mcp.json", json!({ "servers": { "repo": { "command": "./run" } } }).to_string()),
        ]);
        let got = import(&all(), &env(false), &read);
        let names: Vec<&str> = got.servers.keys().map(String::as_str).collect();
        assert_eq!(names, ["chrome-devtools", "gh", "hostinger", "neon", "old"], "{:?}", got.notes);
        let chrome = &got.servers["chrome-devtools"];
        assert_eq!((chrome.transport.as_deref(), chrome.command.as_str(), chrome.args.len()), (Some("stdio"), "npx", 2), "Copilot's local type is stdio");
        assert_eq!(got.servers["neon"].command, "cursor-neon", "a later file wins for a name");
        assert_eq!(got.servers["gh"].headers["Authorization"], "Bearer t0k");
        assert_eq!((got.servers["old"].command.as_str(), got.servers["old"].args[0].as_str()), ("/h/bin/old", "/w"));
        assert!(got.origins["chrome-devtools"].starts_with("Copilot CLI (~/.copilot/mcp-config.json)"));
        assert!(got.notes.iter().any(|n| n.contains("is named \"chrome-devtools\"")));
        assert!(!got.servers.contains_key("repo") && !got.servers.contains_key("nope"), "workspace files need trust; other projects stay out");
        assert!(import(&all(), &env(true), &read).servers.contains_key("repo"));
        assert!(import(&["vscode".to_string()], &env(true), &read).servers.keys().all(|k| ["gh", "old", "repo"].contains(&k.as_str())), "only the chosen sources");
    }

    #[test]
    fn skips_what_it_cannot_start_and_says_why() {
        let read = fake(vec![
            ("/h/.cursor/mcp.json", json!({ "mcpServers": { "ask": { "command": "x", "env": { "K": "${input:key}" } }, "odd": { "type": "ws", "url": "ws://x" }, "empty": {}, "task-x": { "command": "x" } } }).to_string()),
            ("/h/.claude.json", "{ not json".into()),
        ]);
        let got = import(&all(), &env(true), &read);
        assert!(got.servers.is_empty());
        for want in ["could not parse ~/.claude.json", "\"ask\": it asks for an ${input", "\"odd\": unsupported type", "\"empty\": no command or url", "skipped \"task-x\""] {
            assert!(got.notes.iter().any(|n| n.contains(want)), "{want} in {:?}", got.notes);
        }
        assert_eq!(short_name("io.github.ChromeDevTools/chrome-devtools-mcp").as_deref(), Some("chrome-devtools"));
        assert_eq!(short_name("acme/tool").as_deref(), Some("tool"));
        assert_eq!(parse_jsonc("{\"a\": \"// not a comment\", /* x */ \"b\": [1,],}"), Some(json!({ "a": "// not a comment", "b": [1] })));
    }
}
