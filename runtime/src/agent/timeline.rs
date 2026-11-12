//! The timeline: snapshots of the whole workspace, kept with the project in a hidden git
//! repository (`.task-x/timeline.git`) that is separate from the project's own `.git`.
//!
//! A task gets a snapshot before each prompt, after each step that may have changed files, and at
//! its end; each task's snapshots form one chain under `refs/timeline/<taskId>`. Snapshots catch
//! every change (edit tools, commands, Claude Code, the user), because they read the files
//! themselves. Only the `KEEP_TASKS` most recently active tasks keep their timeline: older chains
//! are deleted and their data is removed. A restore first snapshots the current state, so it can
//! be undone like any other step.
//!
//! What is left out: files the project's `.gitignore` ignores, dependency and cache folders
//! (`DEFAULT_EXCLUDES`), and files over `MAX_FILE_BYTES`. A restore leaves all of those alone.

use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// The repository, relative to the workspace root.
pub const DIR: &str = ".task-x/timeline.git";
/// Tasks whose timeline is kept; the chains of older tasks are deleted.
pub const KEEP_TASKS: usize = 10;
/// Larger files are not copied into snapshots.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// More changed files than this in one step is a folder that belongs in `.gitignore`.
pub const MAX_FILES: usize = 50_000;
const REFS: &str = "refs/timeline/";
const MAX_LABEL_CHARS: usize = 200;
/// Diff text returned by `diff`.
pub const MAX_DIFF_CHARS: usize = 400_000;
/// Dependency, cache, and tool folders that snapshots skip even without a `.gitignore`.
const DEFAULT_EXCLUDES: &[&str] = &[
    "/.task-x/timeline.git/",
    "/.task-x/exports/",
    "node_modules/",
    "bower_components/",
    "jspm_packages/",
    "__pycache__/",
    ".venv/",
    "venv/",
    ".tox/",
    ".gradle/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".turbo/",
    ".cache/",
    ".parcel-cache/",
    ".pnpm-store/",
    ".dart_tool/",
    ".expo/",
    ".angular/",
    "target/",
    ".DS_Store",
];

/// One repository at a time: git keeps one index, and pruning must not race a snapshot.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStatus {
    pub path: String,
    /// `added`, `modified`, `deleted`, or `typeChanged`.
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// The commit id in the timeline repository.
    pub id: String,
    pub task_id: String,
    /// 0 for the first snapshot of the task, then 1, 2, … in order.
    pub step: usize,
    pub label: String,
    pub time_ms: u64,
    /// The first snapshot: the state before the task; it has no changes of its own.
    pub base: bool,
    /// What changed since the previous snapshot of the task.
    pub files: Vec<FileStatus>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTimeline {
    pub task_id: String,
    pub snapshots: usize,
    pub last_ms: u64,
    pub last_label: String,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    root: PathBuf,
    git_dir: PathBuf,
}

fn valid_task(id: &str) -> Result<(), String> {
    if super::store::valid_id(id) { Ok(()) } else { Err(format!("invalid task id '{id}'")) }
}

fn valid_commit(id: &str) -> Result<(), String> {
    if (4..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit()) { Ok(()) } else { Err(format!("invalid snapshot id '{id}'")) }
}

/// One line, no control characters, at most `MAX_LABEL_CHARS`.
fn clean_label(s: &str) -> String {
    let line: String = s.lines().next().unwrap_or("").chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let line = line.trim();
    let line = if line.is_empty() { "Snapshot" } else { line };
    if line.chars().count() > MAX_LABEL_CHARS { format!("{}…", line.chars().take(MAX_LABEL_CHARS - 1).collect::<String>()) } else { line.to_owned() }
}

fn status_name(code: &str) -> &'static str {
    match code.chars().next() {
        Some('A') => "added",
        Some('D') => "deleted",
        Some('T') => "typeChanged",
        _ => "modified",
    }
}

fn unquote(p: &str) -> String {
    // Git quotes paths with tabs, newlines, or quotes ("a\tb"); the common ones read back plainly.
    match p.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\t", "\t").replace("\\n", "\n").replace("\\\\", "\\"),
        None => p.to_owned(),
    }
}

impl Timeline {
    /// Opens the timeline of the workspace at `root`, creating the repository the first time.
    pub fn open(root: &Path) -> Result<Self, String> {
        let t = Self { root: root.to_path_buf(), git_dir: root.join(DIR) };
        if !t.git_dir.join("HEAD").is_file() {
            std::fs::create_dir_all(&t.git_dir).map_err(|e| format!("could not create {DIR}: {e}"))?;
            t.git(&["init", "--quiet"])?;
            for (k, v) in [("core.logAllRefUpdates", "false"), ("gc.auto", "0"), ("core.autocrlf", "false"), ("commit.gpgSign", "false")] {
                t.git(&["config", k, v])?;
            }
        }
        // `*` keeps the repository out of the project's own git (and out of its own snapshots).
        let ignore = t.git_dir.join(".gitignore");
        if !ignore.is_file() {
            std::fs::write(&ignore, "*\n").map_err(|e| e.to_string())?;
        }
        let info = t.git_dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| e.to_string())?;
        let exclude = format!("# Written by task-x: what timeline snapshots leave out.\n{}\n", DEFAULT_EXCLUDES.join("\n"));
        if std::fs::read_to_string(info.join("exclude")).ok().as_deref() != Some(exclude.as_str()) {
            std::fs::write(info.join("exclude"), exclude).map_err(|e| e.to_string())?;
        }
        Ok(t)
    }

    /// A git command on the timeline repository, with the project folder as its work tree. The
    /// user's global and system git settings (hooks, filters, signing) do not apply.
    fn cmd(&self) -> Command {
        let mut c = Command::new("git");
        c.arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.root.display()))
            .args(["-c", "core.quotepath=off", "-c", "user.name=task-x", "-c", "user.email=task-x@localhost", "-c", "commit.gpgSign=false"])
            .current_dir(&self.root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.git_dir.join("no-global-config"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null());
        c
    }

    fn git_bytes(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let o = self.cmd().args(args).output().map_err(|e| format!("git is not available: {e}"))?;
        if o.status.success() {
            Ok(o.stdout)
        } else {
            Err(format!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&o.stderr).trim()))
        }
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        self.git_bytes(args).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn tip(&self, task_id: &str) -> Option<String> {
        let r = format!("{REFS}{task_id}^{{commit}}");
        self.git(&["rev-parse", "--verify", "-q", &r]).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
    }

    /// Saves the workspace as the next snapshot of `task_id`. `None` when nothing changed since
    /// the task's last snapshot.
    pub fn snapshot(&self, task_id: &str, label: &str) -> Result<Option<Snapshot>, String> {
        valid_task(task_id)?;
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.snapshot_locked(task_id, label)
    }

    fn snapshot_locked(&self, task_id: &str, label: &str) -> Result<Option<Snapshot>, String> {
        let listed = self.git_bytes(&["ls-files", "-z", "--others", "--modified", "--exclude-standard"])?;
        let changed: Vec<String> =
            listed.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        if changed.len() > MAX_FILES {
            return Err(format!(
                "{} files changed since the last snapshot (more than {MAX_FILES}); add the large folders to .gitignore",
                changed.len()
            ));
        }
        let big: Vec<&String> = changed
            .iter()
            .filter(|p| std::fs::symlink_metadata(self.root.join(p)).is_ok_and(|m| m.is_file() && m.len() > MAX_FILE_BYTES))
            .collect();
        self.git(&["add", "-A", "--", "."])?;
        for chunk in big.chunks(200) {
            let mut args = vec!["rm", "--cached", "-q", "--ignore-unmatch", "--"];
            args.extend(chunk.iter().map(|s| s.as_str()));
            self.git(&args)?;
        }
        let tree = self.git(&["write-tree"])?.trim().to_owned();
        let parent = self.tip(task_id);
        if let Some(p) = &parent {
            if self.git(&["rev-parse", &format!("{p}^{{tree}}")])?.trim() == tree {
                return Ok(None);
            }
        }
        let now = super::store::now_ms();
        let msg = format!("{}\n\ntask: {task_id}\ntime-ms: {now}\n", clean_label(label));
        let mut args = vec!["commit-tree", tree.as_str(), "-m", msg.as_str()];
        if let Some(p) = &parent {
            args.extend(["-p", p.as_str()]);
        }
        let id = self.git(&args)?.trim().to_owned();
        self.git(&["update-ref", &format!("{REFS}{task_id}"), &id])?;
        if parent.is_none() {
            // A new task: only the most recent ones keep their timeline.
            let _ = self.prune_locked();
        }
        Ok(self.list_locked(task_id)?.into_iter().find(|s| s.id == id))
    }

    /// The snapshots of a task, oldest first. Empty when it has none (or they were pruned).
    pub fn list(&self, task_id: &str) -> Result<Vec<Snapshot>, String> {
        valid_task(task_id)?;
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.list_locked(task_id)
    }

    fn list_locked(&self, task_id: &str) -> Result<Vec<Snapshot>, String> {
        let Some(tip) = self.tip(task_id) else { return Ok(Vec::new()) };
        let out = self.git(&["log", "--no-renames", "--name-status", "--format=%x1e%H%x1f%P%x1f%B", &tip])?;
        let mut snaps: Vec<Snapshot> = out
            .split('\x1e')
            .filter(|r| !r.trim().is_empty())
            .filter_map(|rec| {
                let mut lines = rec.lines();
                let head = lines.next()?;
                let mut f = head.splitn(3, '\x1f');
                let (id, parents, label) = (f.next()?.to_owned(), f.next()?.to_owned(), f.next().unwrap_or("").to_owned());
                let mut time_ms = 0;
                let mut files = Vec::new();
                for l in lines {
                    if let Some(t) = l.strip_prefix("time-ms: ") {
                        time_ms = t.trim().parse().unwrap_or(0);
                    } else if let Some((code, path)) = l.split_once('\t') {
                        files.push(FileStatus { path: unquote(path), status: status_name(code).into() });
                    }
                }
                // The first snapshot is the state before the task: it has no changes of its own
                // (`git log` lists a root commit's files as added, with `log.showRoot`).
                let base = parents.trim().is_empty();
                if base {
                    files.clear();
                }
                Some(Snapshot { id, task_id: task_id.to_owned(), step: 0, label: clean_label(&label), time_ms, base, files })
            })
            .collect();
        snaps.reverse();
        for (i, s) in snaps.iter_mut().enumerate() {
            s.step = i;
        }
        Ok(snaps)
    }

    /// The tasks with a timeline, most recently active first.
    pub fn tasks(&self) -> Result<Vec<TaskTimeline>, String> {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.tasks_locked()
    }

    fn tasks_locked(&self) -> Result<Vec<TaskTimeline>, String> {
        let out = self.git(&["for-each-ref", "--format=%(refname)%1f%(contents:subject)%1f%(contents:body)%1e", REFS])?;
        let mut list: Vec<TaskTimeline> = out
            .split('\x1e')
            .filter_map(|rec| {
                let mut f = rec.trim_start_matches('\n').splitn(3, '\x1f');
                let task_id = f.next()?.strip_prefix(REFS)?.to_owned();
                let last_label = f.next().unwrap_or("").to_owned();
                let last_ms = f.next().unwrap_or("").lines().find_map(|l| l.strip_prefix("time-ms: ")).and_then(|t| t.trim().parse().ok()).unwrap_or(0);
                let snapshots = self.git(&["rev-list", "--count", &format!("{REFS}{task_id}")]).ok()?.trim().parse().unwrap_or(0);
                Some(TaskTimeline { task_id, snapshots, last_ms, last_label })
            })
            .collect();
        list.sort_by(|a, b| b.last_ms.cmp(&a.last_ms).then_with(|| a.task_id.cmp(&b.task_id)));
        Ok(list)
    }

    /// Deletes the timelines of all but the `KEEP_TASKS` most recently active tasks, and their data.
    fn prune_locked(&self) -> Result<usize, String> {
        let old: Vec<String> = self.tasks_locked()?.into_iter().skip(KEEP_TASKS).map(|t| t.task_id).collect();
        for id in &old {
            self.git(&["update-ref", "-d", &format!("{REFS}{id}")])?;
        }
        if !old.is_empty() {
            // The deleted chains are unreachable now (no reflogs): remove their data.
            self.git(&["gc", "--prune=now", "--quiet"])?;
        }
        Ok(old.len())
    }

    /// Checks that `snapshot` belongs to the task's timeline; returns the full id.
    fn own(&self, task_id: &str, snapshot: &str) -> Result<String, String> {
        valid_task(task_id)?;
        valid_commit(snapshot)?;
        let tip = self.tip(task_id).ok_or_else(|| format!("task {task_id} has no timeline (only the last {KEEP_TASKS} tasks keep one)"))?;
        let id = self.git(&["rev-parse", "--verify", "-q", &format!("{snapshot}^{{commit}}")]).map_err(|_| format!("unknown snapshot {snapshot}"))?;
        let id = id.trim().to_owned();
        if self.cmd().args(["merge-base", "--is-ancestor", &id, &tip]).status().map_err(|e| e.to_string())?.success() {
            Ok(id)
        } else {
            Err(format!("snapshot {snapshot} is not in the timeline of task {task_id}"))
        }
    }

    /// The changes of one snapshot (against the one before it), or between `against` and it.
    pub fn diff(&self, task_id: &str, snapshot: &str, against: Option<&str>) -> Result<Value, String> {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.own(task_id, snapshot)?;
        let base = match against {
            Some(a) => self.own(task_id, a)?,
            None => match self.git(&["rev-parse", "--verify", "-q", &format!("{id}^")]) {
                Ok(p) => p.trim().to_owned(),
                Err(_) => return Ok(json!({ "snapshot": id, "against": Value::Null, "diff": "", "truncated": false })),
            },
        };
        let text = self.git(&["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--no-renames", "--stat", "--patch", &base, &id])?;
        let truncated = text.len() > MAX_DIFF_CHARS;
        let mut cut = MAX_DIFF_CHARS.min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        Ok(json!({ "snapshot": id, "against": base, "diff": &text[..cut], "truncated": truncated }))
    }

    /// Puts the workspace back to `snapshot`: files it has are written, files added since are
    /// removed. Ignored, skipped, and large files are left alone. The current state is saved first
    /// (a new snapshot when it differs from the newest one); restoring `backup` undoes this.
    pub fn restore(&self, task_id: &str, snapshot: &str) -> Result<Value, String> {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.own(task_id, snapshot)?;
        let step = self.list_locked(task_id)?.iter().find(|s| s.id == id).map_or(0, |s| s.step);
        self.snapshot_locked(task_id, &format!("Before restoring step {step}"))?;
        let backup = self.tip(task_id).ok_or("the timeline disappeared")?;
        let changed: Vec<String> = self.git(&["diff", "--name-only", "--no-renames", &backup, &id])?.lines().map(unquote).collect();
        self.git(&["read-tree", "--reset", "-u", &id])?;
        Ok(json!({ "taskId": task_id, "restored": id, "step": step, "backup": backup, "files": changed }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::index::tests::temp_repo;
    use std::fs;

    fn read(root: &Path, p: &str) -> Option<String> {
        fs::read_to_string(root.join(p)).ok()
    }

    #[test]
    fn snapshots_list_diff_and_restore() {
        let root = temp_repo(&[("src/a.rs", "fn a() {}\n"), ("README.md", "hi\n")]);
        let t = Timeline::open(&root).unwrap();
        let base = t.snapshot("task1", "Before: change a").unwrap().unwrap();
        assert!(base.base && base.files.is_empty() && base.step == 0);
        assert!(t.snapshot("task1", "Step 1: read_file").unwrap().is_none(), "nothing changed, no snapshot");

        fs::write(root.join("src/a.rs"), "fn a() { b() }\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();
        fs::remove_file(root.join("README.md")).unwrap();
        let s1 = t.snapshot("task1", "Step 2: replace_text, run_command").unwrap().unwrap();
        assert_eq!(s1.step, 1);
        let mut files = s1.files.clone();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            files,
            [
                FileStatus { path: "README.md".into(), status: "deleted".into() },
                FileStatus { path: "src/a.rs".into(), status: "modified".into() },
                FileStatus { path: "src/b.rs".into(), status: "added".into() },
            ]
        );
        let d = t.diff("task1", &s1.id, None).unwrap();
        assert!(d["diff"].as_str().unwrap().contains("+fn a() { b() }"), "{d}");
        assert_eq!(t.diff("task1", &base.id, None).unwrap()["diff"], "", "the base has no changes of its own");

        // Back to before the task: the new file goes, the deleted one returns.
        let r = t.restore("task1", &base.id).unwrap();
        assert_eq!((read(&root, "src/a.rs").as_deref(), read(&root, "README.md").as_deref()), (Some("fn a() {}\n"), Some("hi\n")));
        assert!(!root.join("src/b.rs").exists());
        assert_eq!(r["files"].as_array().unwrap().len(), 3);
        // The restore is itself undoable: the state before it is the newest snapshot (here the
        // last step, since nothing changed after it).
        assert_eq!(r["backup"], s1.id.as_str());
        t.restore("task1", r["backup"].as_str().unwrap()).unwrap();
        assert_eq!(read(&root, "src/b.rs").as_deref(), Some("fn b() {}\n"));
        // A change made after the last snapshot is saved before a restore, so it is not lost.
        fs::write(root.join("src/b.rs"), "fn b() { 1 }\n").unwrap();
        let r = t.restore("task1", &base.id).unwrap();
        let list = t.list("task1").unwrap();
        assert_eq!(list.last().unwrap().label, "Before restoring step 0");
        assert_eq!(list.last().unwrap().id, r["backup"]);
        t.restore("task1", r["backup"].as_str().unwrap()).unwrap();
        assert_eq!(read(&root, "src/b.rs").as_deref(), Some("fn b() { 1 }\n"));

        assert!(t.restore("task2", &base.id).unwrap_err().contains("no timeline"));
        assert!(t.diff("task1", "zz", None).unwrap_err().contains("invalid snapshot"));
        assert!(t.snapshot("../x", "x").is_err());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ignored_and_large_files_stay_out() {
        let root = temp_repo(&[(".gitignore", "secret.txt\n"), ("a.txt", "a\n"), ("node_modules/x/index.js", "x\n"), ("secret.txt", "s\n")]);
        let t = Timeline::open(&root).unwrap();
        fs::write(root.join("big.bin"), vec![0u8; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        let base = t.snapshot("t", "Before").unwrap().unwrap();
        let tree = t.git(&["ls-tree", "-r", "--name-only", &base.id]).unwrap();
        assert!(tree.contains("a.txt") && tree.contains(".gitignore"), "{tree}");
        for left_out in ["node_modules", "secret.txt", "big.bin", "timeline.git"] {
            assert!(!tree.contains(left_out), "{left_out} must not be in a snapshot: {tree}");
        }
        // A restore leaves what snapshots skip alone.
        fs::write(root.join("a.txt"), "changed\n").unwrap();
        t.snapshot("t", "Step 1").unwrap().unwrap();
        t.restore("t", &base.id).unwrap();
        assert_eq!(read(&root, "a.txt").as_deref(), Some("a\n"));
        assert!(root.join("big.bin").exists() && root.join("secret.txt").exists() && root.join("node_modules/x/index.js").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_project_git_does_not_see_the_timeline() {
        let root = temp_repo(&[("a.txt", "a\n")]);
        let ok = Command::new("git").args(["init", "-q"]).current_dir(&root).status().is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        let t = Timeline::open(&root).unwrap();
        t.snapshot("t", "Before").unwrap();
        let status = Command::new("git").args(["status", "--porcelain", "--untracked-files=all"]).current_dir(&root).output().unwrap();
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(status.contains("a.txt") && !status.contains("timeline"), "{status}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_the_last_ten_tasks_keep_a_timeline() {
        let root = temp_repo(&[("a.txt", "0\n")]);
        let t = Timeline::open(&root).unwrap();
        for i in 0..KEEP_TASKS + 2 {
            fs::write(root.join("a.txt"), format!("{i}\n")).unwrap();
            t.snapshot(&format!("task{i:02}"), "Before").unwrap().unwrap();
            // Distinct times, so the order is certain.
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let tasks = t.tasks().unwrap();
        assert_eq!(tasks.len(), KEEP_TASKS);
        assert_eq!(tasks[0].task_id, "task11", "most recent first");
        assert!(!tasks.iter().any(|x| x.task_id == "task00" || x.task_id == "task01"), "the oldest two were deleted");
        assert!(t.list("task00").unwrap().is_empty());
        // A task that is still going keeps its place: a new snapshot makes it recent again.
        fs::write(root.join("a.txt"), "again\n").unwrap();
        t.snapshot("task02", "Step 1").unwrap().unwrap();
        assert_eq!(t.tasks().unwrap()[0].task_id, "task02");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn labels_are_one_clean_line() {
        assert_eq!(clean_label("Before: fix it\nmore"), "Before: fix it");
        assert_eq!(clean_label("  \t "), "Snapshot");
        assert_eq!(clean_label(&"x".repeat(500)).chars().count(), MAX_LABEL_CHARS);
    }
}
