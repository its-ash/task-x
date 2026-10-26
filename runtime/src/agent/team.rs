//! Git worktrees and subagents. A subagent works toward one bounded objective with role-limited
//! tools; writing roles get their own worktree, so parallel subagents never edit the same files.
//! Their changes come back as a patch that the parent reviews and applies (or discards).

use crate::tools::{ToolCtx, edit, exec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const BRANCH_PREFIX: &str = "task-x/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Research,
    Implementation,
    Testing,
    Review,
    Documentation,
}

impl Role {
    /// The role names `run_subagent` and `run_plan` accept, for tool schemas and error messages.
    pub const NAMES: &'static str = "research, implementation, testing, review, documentation";

    /// Reads a role the way models write one. The tool schema lists the enum, but several providers
    /// ignore it and models send `Research`, `code`, `docs`, or `qa`; those used to fail the call
    /// with a message the model could not act on.
    pub fn parse(s: &str) -> Option<Self> {
        let key = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match key.as_str() {
            "research" | "researcher" | "investigate" | "investigation" | "analysis" | "analyze" | "explore" => Self::Research,
            "implementation" | "implement" | "implementer" | "code" | "coder" | "coding" | "developer" | "dev" | "engineer" | "feature" => {
                Self::Implementation
            }
            "testing" | "test" | "tests" | "tester" | "qa" => Self::Testing,
            "review" | "reviewer" | "reviews" | "audit" | "critic" => Self::Review,
            "documentation" | "document" | "docs" | "doc" | "writer" | "writing" | "tech-writer" => Self::Documentation,
            _ => return None,
        })
    }

    pub fn writes(self) -> bool {
        matches!(self, Self::Implementation | Self::Testing | Self::Documentation)
    }

    pub fn brief(self) -> &'static str {
        match self {
            Self::Research => {
                "You are a research subagent. Investigate with read-only tools and report findings with file:line references. Do not edit."
            }
            Self::Implementation => {
                "You are an implementation subagent working in an isolated git worktree. Make the requested change, keep it focused, and validate it."
            }
            Self::Testing => {
                "You are a testing subagent working in an isolated git worktree. Add or fix tests for the objective and run them."
            }
            Self::Review => {
                "You are a code review subagent. Review with read-only tools; report concrete defects with file:line, severity, and confidence. Do not edit. End with a fenced ```task-x-findings block: a JSON array of { \"path\", \"line\", \"severity\": \"error\"|\"warning\"|\"info\", \"message\" }."
            }
            Self::Documentation => {
                "You are a documentation subagent working in an isolated git worktree. Update documentation to match the code."
            }
        }
    }
}

/// Lenient so a capitalized or common alias (`Research`, `code`, `docs`, `qa`) still selects a role.
impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown role '{s}'; use one of: {}", Self::NAMES)))
    }
}

fn git(args: &[&str], cwd: &Path, cancel: &CancellationToken) -> Result<String, String> {
    let mut argv = vec!["git".to_string(), "--no-pager".into(), "-c".into(), "core.quotepath=off".into()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let o = exec::run(&argv, cwd, ".", GIT_TIMEOUT, cancel).map_err(|e| e.message)?;
    if o.success { Ok(o.stdout) } else { Err(format!("git {}: {}", args.first().unwrap_or(&""), o.stderr.trim())) }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 48 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Worktrees live outside the workspace (so the index and tools never see them).
pub fn worktree_dir(repo: &Path, name: &str) -> PathBuf {
    let base = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    repo.parent().unwrap_or(repo).join(".task-x-worktrees").join(base).join(name)
}

/// Git config key that stores a worktree's base commit, so its patch leaves out the base patches.
fn base_key(name: &str) -> String {
    format!("branch.{BRANCH_PREFIX}{name}.taskxbase")
}

/// Creates a worktree and applies `base` patches (work of steps this one depends on) as a
/// commit, so the subagent starts from that work and its own patch contains only its changes.
pub fn create_with_base(repo: &Path, name: &str, base: &[String], cancel: &CancellationToken) -> Result<PathBuf, String> {
    let dir = create(repo, name, cancel)?;
    let patches: Vec<&String> = base.iter().filter(|p| !p.trim().is_empty()).collect();
    if patches.is_empty() {
        return Ok(dir);
    }
    let setup = || -> Result<(), String> {
        for (i, p) in patches.iter().enumerate() {
            let file = std::env::temp_dir().join(format!("task-x-base-{name}-{i}-{}.patch", std::process::id()));
            std::fs::write(&file, p).map_err(|e| e.to_string())?;
            let res = git(&["apply", "--whitespace=nowarn", &file.to_string_lossy()], &dir, cancel);
            let _ = std::fs::remove_file(&file);
            res.map_err(|e| format!("could not apply the work of an earlier step: {e}"))?;
        }
        git(&["add", "-A"], &dir, cancel)?;
        let commit = [
            "-c",
            "user.name=task-x",
            "-c",
            "user.email=task-x@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            "task-x: work of earlier steps",
        ];
        git(&commit, &dir, cancel)?;
        let sha = git(&["rev-parse", "HEAD"], &dir, cancel)?;
        git(&["config", &base_key(name), sha.trim()], repo, cancel).map(|_| ())
    };
    setup().inspect_err(|_| {
        let _ = remove(repo, name, true, false, cancel);
    })?;
    Ok(dir)
}

pub fn create(repo: &Path, name: &str, cancel: &CancellationToken) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!("invalid worktree name '{name}'"));
    }
    git(&["rev-parse", "--verify", "HEAD"], repo, cancel)
        .map_err(|_| "the repository needs at least one commit to use worktrees".to_string())?;
    let dir = worktree_dir(repo, name);
    if dir.exists() {
        return Err(format!("worktree '{name}' already exists"));
    }
    let branch = format!("{BRANCH_PREFIX}{name}");
    git(&["worktree", "add", "-b", &branch, &dir.to_string_lossy(), "HEAD"], repo, cancel)?;
    Ok(dir)
}

/// The worktree's uncommitted and committed changes since it branched, as one patch.
pub fn diff(repo: &Path, name: &str, cancel: &CancellationToken) -> Result<String, String> {
    let dir = worktree_dir(repo, name);
    git(&["add", "-A"], &dir, cancel)?;
    let base = git(&["config", "--get", &base_key(name)], repo, cancel)
        .or_else(|_| git(&["merge-base", "HEAD", &format!("{BRANCH_PREFIX}{name}")], repo, cancel))
        .unwrap_or_else(|_| "HEAD".into());
    git(&["diff", "--cached", "--binary", base.trim()], &dir, cancel)
}

/// Repository-relative paths a patch touches (both sides of a rename).
pub fn patch_paths(patch: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in patch.lines() {
        let p = l
            .strip_prefix("--- a/")
            .or_else(|| l.strip_prefix("+++ b/"))
            .or_else(|| l.strip_prefix("rename from "))
            .or_else(|| l.strip_prefix("rename to "));
        if let Some(p) = p.map(str::trim).filter(|p| !p.is_empty() && !out.iter().any(|o| o == p)) {
            out.push(p.to_owned());
        }
    }
    out
}

/// Lines added and removed by a patch.
pub fn patch_stat(patch: &str) -> (usize, usize) {
    patch.lines().fold((0, 0), |(a, d), l| match l.as_bytes().first() {
        Some(b'+') if !l.starts_with("+++") => (a + 1, d),
        Some(b'-') if !l.starts_with("---") => (a, d + 1),
        _ => (a, d),
    })
}

/// Applies a worktree's patch to the main tree and records it in the edit log, so the change shows
/// in the review panel and can be undone like any agent edit. Binary files are applied but not logged.
pub fn merge(ctx: &ToolCtx, repo: &Path, name: &str, cancel: &CancellationToken) -> Result<Value, String> {
    let patch = diff(repo, name, cancel)?;
    if patch.trim().is_empty() {
        return Ok(json!({ "applied": false, "reason": "no changes" }));
    }
    let targets: Vec<(PathBuf, String)> = patch_paths(&patch).iter().filter_map(|p| ctx.resolve(p).ok()).collect();
    let before: Vec<Result<Option<String>, ()>> = targets.iter().map(|(abs, _)| read_text(abs)).collect();
    apply_patch(repo, name, &patch, cancel)?;
    let changes: Vec<edit::Change> = targets
        .into_iter()
        .zip(before)
        .filter_map(|((abs, rel), b)| {
            let (Ok(before), Ok(after)) = (b, read_text(&abs)) else { return None };
            (before != after).then_some(edit::Change { abs, rel, before, after })
        })
        .collect();
    let rec = edit::record(ctx, &format!("merge {name}"), &changes);
    Ok(json!({ "applied": true, "files": patch_paths(&patch), "changed": rec["changed"], "operation": rec["operation"] }))
}

/// File text, `None` when the file does not exist, `Err` when it is not UTF-8 text.
fn read_text(abs: &Path) -> Result<Option<String>, ()> {
    match std::fs::read_to_string(abs) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn apply_patch(repo: &Path, name: &str, patch: &str, cancel: &CancellationToken) -> Result<(), String> {
    let file = std::env::temp_dir().join(format!("task-x-{name}-{}.patch", std::process::id()));
    std::fs::write(&file, patch).map_err(|e| e.to_string())?;
    let f = file.to_string_lossy().to_string();
    let res = git(&["apply", "--check", &f], repo, cancel).and_then(|_| git(&["apply", &f], repo, cancel));
    let _ = std::fs::remove_file(&file);
    res.map(|_| ()).map_err(|e| format!("the worktree patch conflicts with the current working tree; nothing was changed. {e}"))
}

/// Removes a worktree and its branch. Refuses when it has changes that were never applied, unless forced.
pub fn remove(repo: &Path, name: &str, force: bool, applied: bool, cancel: &CancellationToken) -> Result<(), String> {
    let dir = worktree_dir(repo, name);
    if !dir.exists() {
        return Err(format!("no worktree named '{name}'"));
    }
    if !force && !applied && !diff(repo, name, cancel)?.trim().is_empty() {
        return Err(format!("worktree '{name}' has changes that were not applied; apply them or discard with force"));
    }
    git(&["worktree", "remove", "--force", &dir.to_string_lossy()], repo, cancel)?;
    let _ = git(&["config", "--unset", &base_key(name)], repo, cancel);
    let _ = git(&["branch", "-D", &format!("{BRANCH_PREFIX}{name}")], repo, cancel);
    Ok(())
}

pub fn list(repo: &Path, cancel: &CancellationToken) -> Result<Vec<String>, String> {
    let out = git(&["worktree", "list", "--porcelain"], repo, cancel)?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("branch refs/heads/").and_then(|b| b.strip_prefix(BRANCH_PREFIX)).map(str::to_owned))
        .collect())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn roles_are_read_however_the_model_spells_them() {
        use Role::*;
        // The canonical names, and the spellings models send: capitalized, padded, or aliased.
        for (raw, role) in [
            ("research", Research),
            ("Research", Research),
            (" RESEARCH ", Research),
            ("researcher", Research),
            ("investigate", Research),
            ("implementation", Implementation),
            ("Implementation", Implementation),
            ("code", Implementation),
            ("Coder", Implementation),
            ("engineer", Implementation),
            ("testing", Testing),
            ("QA", Testing),
            ("test", Testing),
            ("review", Review),
            ("Reviewer", Review),
            ("docs", Documentation),
            ("Documentation", Documentation),
            ("tech_writer", Documentation),
        ] {
            assert_eq!(Role::parse(raw), Some(role), "{raw}");
        }
        for bad in ["", "  ", "chef", "subagent", "research/implementation"] {
            assert_eq!(Role::parse(bad), None, "{bad:?} is not a role");
        }
    }

    #[test]
    fn a_plan_step_accepts_a_capitalized_role() {
        // `run_plan` deserializes its steps, so the lenient role reading covers them too.
        let step: super::super::PlanStep = serde_json::from_value(json!({
            "id": "edit", "role": "Implementation", "objective": "say hello"
        }))
        .unwrap();
        assert_eq!(step.role, Role::Implementation);
        let err = serde_json::from_value::<super::super::PlanStep>(json!({ "id": "x", "role": "chef", "objective": "cook" })).unwrap_err();
        assert!(err.to_string().contains("unknown role 'chef'") && err.to_string().contains("research"), "{err}");
        // The wire form is unchanged: lowercase, for events and the client.
        assert_eq!(serde_json::to_value(Role::Implementation).unwrap(), json!("implementation"));
    }

    #[test]
    fn create_diff_apply_and_guarded_remove() {
        let base = std::env::temp_dir().join(format!("anvil-team-{}", std::process::id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let c = CancellationToken::new();
        let g = |a: &[&str]| std::process::Command::new("git").args(a).current_dir(&repo).output().unwrap();
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        let wt = create(&repo, "w1", &c).unwrap();
        std::fs::write(wt.join("a.txt"), "two\n").unwrap();
        std::fs::write(wt.join("new.txt"), "n\n").unwrap();
        assert_eq!(list(&repo, &c).unwrap(), ["w1"]);
        assert!(remove(&repo, "w1", false, false, &c).is_err(), "unapplied changes are protected");
        let d = diff(&repo, "w1", &c).unwrap();
        assert!(d.contains("+two") && d.contains("new.txt"));
        apply_patch(&repo, "w1", &d, &c).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two\n");
        remove(&repo, "w1", false, true, &c).unwrap();
        assert!(!wt.exists());
        // A dependent step starts from w1's work, and its patch holds only its own change.
        let w2 = create_with_base(&repo, "w2", std::slice::from_ref(&d), &c).unwrap();
        assert_eq!(std::fs::read_to_string(w2.join("a.txt")).unwrap(), "two\n");
        std::fs::write(w2.join("a.txt"), "three\n").unwrap();
        let d2 = diff(&repo, "w2", &c).unwrap();
        assert!(d2.contains("-two") && d2.contains("+three") && !d2.contains("new.txt"), "{d2}");
        assert_eq!(patch_stat(&d2), (1, 1));
        remove(&repo, "w2", true, false, &c).unwrap();
        std::fs::remove_dir_all(base).ok();
    }
}
