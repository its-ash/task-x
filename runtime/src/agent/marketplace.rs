//! Plugin marketplaces and installs without the Claude Code CLI.
//!
//! A marketplace is a git repository (or folder) with `.claude-plugin/marketplace.json`:
//! `{ "name", "plugins": [{ "name", "description", "version", "source" }] }`, where `source` is a
//! folder in the repository (`"./plugins/x"`), `{ "source": "github", "repo": "owner/repo" }`, or
//! `{ "source": "url" | "git", "url" }`. This is Claude Code's format, so its marketplaces work.
//!
//! - task-x keeps marketplaces in `~/.task-x/marketplaces/<name>/`, and also reads the ones Claude
//!   Code added (`~/.claude/plugins/marketplaces/`).
//! - `install` copies or clones a plugin into `~/.task-x/plugins/<name>/` (scope `user`) or
//!   `<workspace>/.task-x/plugins/<name>/` (scope `workspace`), where `plugins::load` finds it, and
//!   records where it came from in `.task-x-source.json` for `update`.
//! - Every git call is an argument list (no shell), with `--` before URLs and paths.

use crate::tools::exec;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const GIT_TIMEOUT: Duration = Duration::from_secs(180);
const SOURCE_FILE: &str = ".task-x-source.json";
const MAX_COPY_FILES: usize = 20_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Marketplace {
    pub name: String,
    pub path: PathBuf,
    /// `task-x` (ours) or `claude` (added by Claude Code; read only).
    pub owner: String,
    pub plugins: Vec<Value>,
}

/// Where a plugin comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Git(String),
    Dir(PathBuf),
}

fn valid_name(n: &str) -> bool {
    super::plugins::valid_name(n) && !n.starts_with('.')
}

/// Reads a source the user typed: `owner/repo` (GitHub), an https or ssh git URL, or an absolute folder.
pub fn parse_source(s: &str) -> Result<Source, String> {
    let s = s.trim();
    if s.is_empty() || s.starts_with('-') || s.contains(char::is_whitespace) {
        return Err(format!("'{s}' is not a git URL, owner/repo, or folder"));
    }
    if s.starts_with("https://") || s.starts_with("git@") || s.starts_with("ssh://") {
        return Ok(Source::Git(s.to_owned()));
    }
    if s.starts_with("http://") || s.starts_with("file://") {
        return Err("use an https or ssh git URL".into());
    }
    let p = Path::new(s);
    if p.is_absolute() {
        return if p.is_dir() { Ok(Source::Dir(p.to_path_buf())) } else { Err(format!("{s} is not a folder")) };
    }
    let parts: Vec<&str> = s.trim_end_matches(".git").split('/').collect();
    if parts.len() == 2 && parts.iter().all(|p| valid_name(p)) {
        return Ok(Source::Git(format!("https://github.com/{}/{}.git", parts[0], parts[1])));
    }
    Err(format!("'{s}' is not a git URL, owner/repo, or absolute folder"))
}

fn git(args: &[&str], cwd: &Path, cancel: &CancellationToken) -> Result<String, String> {
    let mut argv = vec!["git".to_string(), "-c".into(), "core.hooksPath=/dev/null".into(), "-c".into(), "protocol.file.allow=never".into()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let o = exec::run(&argv, cwd, ".", GIT_TIMEOUT, cancel).map_err(|e| e.message)?;
    if o.success { Ok(o.stdout) } else { Err(format!("git {}: {}", args.first().unwrap_or(&""), o.stderr.trim())) }
}

/// Copies a folder (no symlinks, no `.git`), up to `MAX_COPY_FILES` files.
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    let mut count = 0;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        std::fs::create_dir_all(&dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        for e in std::fs::read_dir(&src).map_err(|e| format!("{}: {e}", src.display()))?.flatten() {
            let ty = e.file_type().map_err(|e| e.to_string())?;
            let name = e.file_name();
            if name == ".git" || ty.is_symlink() {
                continue;
            }
            if ty.is_dir() {
                stack.push((e.path(), dst.join(&name)));
            } else if ty.is_file() {
                count += 1;
                if count > MAX_COPY_FILES {
                    return Err(format!("{} has more than {MAX_COPY_FILES} files", from.display()));
                }
                std::fs::copy(e.path(), dst.join(&name)).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Puts `source` (a whole repository or a folder) at `dest`, through a temporary folder next to it,
/// so a failed clone leaves nothing behind. `subdir` picks a folder inside a cloned repository.
fn fetch(source: &Source, subdir: Option<&str>, dest: &Path, cancel: &CancellationToken) -> Result<(), String> {
    let parent = dest.parent().ok_or("no parent folder")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(".tmp-{}-{}", std::process::id(), super::store::now_ms()));
    let res = (|| {
        match source {
            Source::Git(url) => {
                git(&["clone", "--depth", "1", "--", url, &tmp.to_string_lossy()], parent, cancel)?;
            }
            Source::Dir(d) => copy_dir(d, &tmp)?,
        }
        let pick = match subdir.map(|s| s.trim_start_matches("./").trim_end_matches('/')).filter(|s| !s.is_empty() && *s != ".") {
            Some(s) => {
                let p = tmp.join(s);
                let (canon, base) = (p.canonicalize().map_err(|_| format!("{s} is not in the source"))?, tmp.canonicalize().map_err(|e| e.to_string())?);
                if !canon.starts_with(&base) || !canon.is_dir() {
                    return Err(format!("{s} is not a folder inside the source"));
                }
                canon
            }
            None => tmp.clone(),
        };
        if pick == tmp {
            std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
        } else {
            copy_dir(&pick, dest)
        }
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    if res.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    res
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn read_marketplace(dir: &Path, owner: &str) -> Option<Marketplace> {
    let m = read_json(&dir.join(".claude-plugin").join("marketplace.json"))?;
    let folder = dir.file_name()?.to_str()?.to_owned();
    let name = m["name"].as_str().filter(|n| valid_name(n)).map_or(folder, str::to_owned);
    let plugins = m["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["name"].as_str().is_some_and(valid_name))
        .map(|p| json!({ "name": p["name"], "description": p["description"], "version": p["version"], "source": p["source"] }))
        .collect();
    Some(Marketplace { name, path: dir.to_path_buf(), owner: owner.to_owned(), plugins })
}

/// Marketplaces task-x keeps, then those Claude Code added (a task-x one wins on a name clash).
pub fn list(home: &Path) -> Vec<Marketplace> {
    let mut out: Vec<Marketplace> = Vec::new();
    for (dir, owner) in [(home.join(".task-x").join("marketplaces"), "task-x"), (home.join(".claude").join("plugins").join("marketplaces"), "claude")] {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        entries.sort();
        for p in entries {
            if let Some(m) = read_marketplace(&p, owner).filter(|m| !out.iter().any(|o| o.name == m.name)) {
                out.push(m);
            }
        }
    }
    out
}

/// Adds a marketplace from a git URL, `owner/repo`, or a folder. Returns it.
pub fn add(home: &Path, source: &str, cancel: &CancellationToken) -> Result<Marketplace, String> {
    let src = parse_source(source)?;
    let root = home.join(".task-x").join("marketplaces");
    let staging = root.join(format!(".new-{}", super::store::now_ms()));
    fetch(&src, None, &staging, cancel)?;
    let m = read_marketplace(&staging, "task-x").ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&staging);
        format!("{source} has no .claude-plugin/marketplace.json")
    })?;
    let dest = root.join(&m.name);
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("a marketplace named {} is already added; update or remove it", m.name));
    }
    std::fs::rename(&staging, &dest).map_err(|e| e.to_string())?;
    std::fs::write(dest.join(SOURCE_FILE), json!({ "source": source }).to_string()).map_err(|e| e.to_string())?;
    read_marketplace(&dest, "task-x").ok_or_else(|| "the marketplace could not be read after adding it".into())
}

fn ours(home: &Path, name: &str) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!("invalid marketplace name '{name}'"));
    }
    let d = home.join(".task-x").join("marketplaces").join(name);
    if d.is_dir() { Ok(d) } else { Err(format!("task-x has no marketplace named {name} (Claude Code's are managed with `claude plugin marketplace`)")) }
}

/// Pulls the newest commit of a marketplace, or copies its folder again.
pub fn update(home: &Path, name: &str, cancel: &CancellationToken) -> Result<Marketplace, String> {
    let d = ours(home, name)?;
    if d.join(".git").exists() {
        git(&["pull", "--ff-only", "--depth", "1"], &d, cancel)?;
    } else if let Some(src) = read_json(&d.join(SOURCE_FILE)).and_then(|v| v["source"].as_str().map(str::to_owned)) {
        let src = parse_source(&src)?;
        let tmp = d.with_extension("update");
        fetch(&src, None, &tmp, cancel)?;
        std::fs::remove_dir_all(&d).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &d).map_err(|e| e.to_string())?;
    }
    read_marketplace(&d, "task-x").ok_or_else(|| format!("{name} has no marketplace.json after the update"))
}

pub fn remove(home: &Path, name: &str) -> Result<(), String> {
    std::fs::remove_dir_all(ours(home, name)?).map_err(|e| e.to_string())
}

/// The folder plugins of a scope go to.
pub fn plugins_dir(home: &Path, workspace: Option<&Path>, scope: &str) -> Result<PathBuf, String> {
    match scope {
        "user" | "" => Ok(home.join(".task-x").join("plugins")),
        "workspace" | "project" => workspace.map(|w| w.join(".task-x").join("plugins")).ok_or_else(|| "no workspace is open".into()),
        other => Err(format!("unknown scope '{other}'; use user or workspace")),
    }
}

/// Where a marketplace entry's `source` points: (repository or folder, folder inside it).
fn entry_source(m: &Marketplace, entry: &Value) -> Result<(Source, Option<String>), String> {
    let s = &entry["source"];
    if let Some(rel) = s.as_str() {
        if rel.starts_with("./") || rel == "." || !rel.contains("://") && !rel.contains('@') {
            return Ok((Source::Dir(m.path.clone()), Some(rel.to_owned())));
        }
        return Ok((parse_source(rel)?, None));
    }
    let path = s["path"].as_str().map(str::to_owned);
    match s["source"].as_str() {
        Some("github") => Ok((parse_source(s["repo"].as_str().ok_or("a github source needs repo")?)?, path)),
        Some("url" | "git") => Ok((parse_source(s["url"].as_str().ok_or("a url source needs url")?)?, path)),
        _ => Err(format!("plugin {} has a source task-x cannot read: {s}", entry["name"])),
    }
}

/// Installs a plugin: `name@marketplace`, a git URL, `owner/repo`, or an absolute folder.
pub fn install(home: &Path, workspace: Option<&Path>, spec: &str, scope: &str, force: bool, cancel: &CancellationToken) -> Result<Value, String> {
    let dir = plugins_dir(home, workspace, scope)?;
    let (source, subdir, market, wanted) = match spec.split_once('@').filter(|(n, m)| valid_name(n) && valid_name(m) && !spec.contains("://")) {
        Some((name, market)) => {
            let m = list(home).into_iter().find(|m| m.name == market).ok_or_else(|| format!("no marketplace named {market}; add it first"))?;
            let entry = m.plugins.iter().find(|p| p["name"] == name).cloned().ok_or_else(|| format!("{market} has no plugin named {name}"))?;
            let (src, sub) = entry_source(&m, &entry)?;
            (src, sub, Some(market.to_owned()), Some(name.to_owned()))
        }
        None => (parse_source(spec)?, None, None, None),
    };
    let staging = dir.join(format!(".new-{}", super::store::now_ms()));
    fetch(&source, subdir.as_deref(), &staging, cancel)?;
    let manifest = read_json(&staging.join(".claude-plugin").join("plugin.json")).unwrap_or(Value::Null);
    let looks_like_plugin = ["skills", "agents", "commands", "hooks", ".claude-plugin", ".mcp.json"].iter().any(|d| staging.join(d).exists());
    let folder = match &source {
        Source::Git(u) => u.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', ':']).next().unwrap_or("plugin").to_owned(),
        Source::Dir(d) => d.file_name().and_then(|n| n.to_str()).unwrap_or("plugin").to_owned(),
    };
    let name = wanted.or_else(|| manifest["name"].as_str().map(str::to_owned)).unwrap_or(folder);
    let fail = |e: String| {
        let _ = std::fs::remove_dir_all(&staging);
        e
    };
    if !looks_like_plugin {
        return Err(fail(format!("{spec} has no skills, agents, commands, hooks, or plugin.json: it is not a plugin")));
    }
    if !valid_name(&name) {
        return Err(fail(format!("invalid plugin name '{name}'")));
    }
    let dest = dir.join(&name);
    if dest.exists() {
        if !force {
            return Err(fail(format!("{name} is already installed; update it, or install with force")));
        }
        std::fs::remove_dir_all(&dest).map_err(|e| fail(e.to_string()))?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| fail(e.to_string()))?;
    let record = json!({ "spec": spec, "marketplace": market, "installedMs": super::store::now_ms() });
    std::fs::write(dest.join(SOURCE_FILE), record.to_string()).map_err(|e| e.to_string())?;
    Ok(json!({ "name": name, "path": dest, "scope": if scope.is_empty() { "user" } else { scope }, "version": manifest["version"], "marketplace": market }))
}

fn installed(home: &Path, workspace: Option<&Path>, name: &str, scope: &str) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!("invalid plugin name '{name}'"));
    }
    let d = plugins_dir(home, workspace, scope)?.join(name);
    if d.is_dir() { Ok(d) } else { Err(format!("no plugin named {name} in the {} plugins of task-x", if scope.is_empty() { "user" } else { scope })) }
}

pub fn uninstall(home: &Path, workspace: Option<&Path>, name: &str, scope: &str) -> Result<(), String> {
    std::fs::remove_dir_all(installed(home, workspace, name, scope)?).map_err(|e| e.to_string())
}

/// Installs a plugin again from where it came from (its marketplace may have a newer version).
pub fn update_plugin(home: &Path, workspace: Option<&Path>, name: &str, scope: &str, cancel: &CancellationToken) -> Result<Value, String> {
    let d = installed(home, workspace, name, scope)?;
    let spec = read_json(&d.join(SOURCE_FILE)).and_then(|v| v["spec"].as_str().map(str::to_owned)).ok_or_else(|| format!("{name} was not installed by task-x, so it has no source to update from"))?;
    if let Some(m) = spec.split_once('@').map(|(_, m)| m.to_owned()).filter(|m| valid_name(m)) {
        // A task-x marketplace is pulled first, so the plugin comes from its newest version.
        let _ = update(home, &m, cancel);
    }
    install(home, workspace, &spec, scope, true, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn parses_sources_safely() {
        assert_eq!(parse_source("acme/tools").unwrap(), Source::Git("https://github.com/acme/tools.git".into()));
        assert!(matches!(parse_source("https://gitlab.com/a/b.git").unwrap(), Source::Git(_)));
        for bad in ["", "-u evil", "http://x/y", "file:///etc", "a b", "relative/path/deep", "../up"] {
            assert!(parse_source(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn adds_a_marketplace_and_installs_updates_and_removes_a_plugin() {
        let base = std::env::temp_dir().join(format!("task-x-market-{}", std::process::id()));
        let (home, src, ws) = (base.join("home"), base.join("src"), base.join("ws"));
        write(
            &src.join(".claude-plugin/marketplace.json"),
            r#"{ "name": "acme", "plugins": [ { "name": "lint", "description": "Lint kit", "version": "1.0.0", "source": "./plugins/lint" }, { "name": "bad name", "source": "./x" } ] }"#,
        );
        write(&src.join("plugins/lint/.claude-plugin/plugin.json"), r#"{ "name": "lint", "version": "1.0.0" }"#);
        write(&src.join("plugins/lint/commands/fix.md"), "Fix $ARGUMENTS");
        write(&home.join(".claude/plugins/marketplaces/theirs/.claude-plugin/marketplace.json"), r#"{ "name": "theirs", "plugins": [] }"#);
        let c = CancellationToken::new();
        let m = add(&home, &src.to_string_lossy(), &c).unwrap();
        assert_eq!((m.name.as_str(), m.plugins.len()), ("acme", 1), "invalid entries are dropped");
        assert!(add(&home, &src.to_string_lossy(), &c).is_err(), "the same name twice");
        let names: Vec<(String, String)> = list(&home).into_iter().map(|m| (m.name, m.owner)).collect();
        assert_eq!(names, [("acme".into(), "task-x".into()), ("theirs".into(), "claude".into())]);
        let out = install(&home, Some(&ws), "lint@acme", "user", false, &c).unwrap();
        assert_eq!(out["name"], "lint");
        assert!(home.join(".task-x/plugins/lint/commands/fix.md").is_file());
        assert!(install(&home, Some(&ws), "lint@acme", "user", false, &c).is_err(), "already installed");
        let found = super::super::plugins::load(Some(&ws), Some(&home), &[], &[]);
        assert!(found.iter().any(|p| p.name == "lint" && p.source == "user" && p.commands == 1), "plugins::load finds it");
        // A newer version in the source reaches the plugin through update.
        write(&src.join("plugins/lint/commands/new.md"), "New");
        std::fs::remove_dir_all(home.join(".task-x/marketplaces/acme")).unwrap();
        add(&home, &src.to_string_lossy(), &c).unwrap();
        update_plugin(&home, Some(&ws), "lint", "user", &c).unwrap();
        assert!(home.join(".task-x/plugins/lint/commands/new.md").is_file());
        // A folder install into the workspace, then uninstall.
        let ws_out = install(&home, Some(&ws), &src.join("plugins/lint").to_string_lossy(), "workspace", false, &c).unwrap();
        assert_eq!(ws_out["scope"], "workspace");
        uninstall(&home, Some(&ws), "lint", "workspace").unwrap();
        assert!(!ws.join(".task-x/plugins/lint").exists());
        assert!(uninstall(&home, Some(&ws), "../etc", "user").is_err());
        assert!(install(&home, Some(&ws), &base.join("home").to_string_lossy(), "user", false, &c).is_err(), "not a plugin");
        assert!(install(&home, Some(&ws), "nope@acme", "user", false, &c).is_err());
        remove(&home, "acme").unwrap();
        assert!(remove(&home, "theirs").is_err(), "Claude Code's marketplaces are not ours to remove");
        std::fs::remove_dir_all(base).ok();
    }
}
