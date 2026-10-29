//! Bounded process execution: argv arrays only (no shell), fixed cwd, timeout, cancellation,
//! and capped output. The whole process group is terminated on timeout or cancel (Unix).

use super::{ToolError, ToolErrorKind};
use regex::Regex;
use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const MAX_STREAM_BYTES: usize = 48 * 1024;
const POLL: Duration = Duration::from_millis(25);

/// Commands that usually take a minute or more: builds, tests, installs, fetches, and dev servers.
/// Every engine starts them in the background. Flags may come before the subcommand
/// (`cargo --manifest-path x test`, `npm --prefix ext install`).
static LONG_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    let args = r"(?:[^\s;&|]+\s+)*?";
    let tools = [
        format!(r"cargo\s+{args}(?:build|test|run|fetch|install|bench|doc|nextest|clippy|check)\b"),
        format!(r"(?:npm|pnpm|yarn|bun)\s+{args}(?:i|install|ci|add|update|run|test|exec|build)\b"),
        r"npx\s".into(),
        format!(r"(?:pip3?|uv\s+pip)\s+{args}install\b"),
        format!(r"uv\s+{args}(?:sync|run|add)\b"),
        format!(r"poetry\s+{args}(?:install|add|update)\b"),
        r"pytest\b".into(),
        format!(r"go\s+{args}(?:build|test|get|install|run|mod\s+download)\b"),
        r"(?:mvn|\./mvnw|gradle|\./gradlew)\b".into(),
        format!(r"(?:flutter|dart)\s+{args}(?:build|test|run|pub|create)\b"),
        r"(?:make|ninja|xcodebuild)\b".into(),
        r"cmake\s+--build\b".into(),
        format!(r"docker\s+{args}(?:build|compose|run|pull)\b"),
        format!(r"swift\s+{args}(?:build|test)\b"),
        format!(r"dotnet\s+{args}(?:build|test|restore|run)\b"),
        format!(r"bundle\s+{args}(?:install|exec)\b"),
        format!(r"composer\s+{args}(?:install|update|require)\b"),
        format!(r"mix\s+{args}(?:deps\.get|compile|test)\b"),
        format!(r"(?:stack|cabal)\s+{args}(?:build|test)\b"),
        format!(r"brew\s+{args}install\b"),
        r"(?:tsc|webpack|jest|vitest|playwright)\b".into(),
        r"(?:vite|next)\s+build\b".into(),
    ];
    // `sh -c "cargo build"` arrives from `run_command` as one line with the shell in front.
    Regex::new(&format!(r#"(?:^|[;&|(\n])\s*(?:(?:ba|z)?sh\s+-[a-z]*c\s+['"]?)?(?:sudo\s+)?(?:\w+=\S*\s+)*(?:{})"#, tools.join("|")))
        .expect("long command regex")
});

/// Whether a command line (a shell string, or argv joined with spaces) is a build, test, install, or
/// other command that usually runs long enough to belong in the background.
pub fn is_long_command(cmd: &str) -> bool {
    LONG_COMMAND.is_match(cmd)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub argv: Vec<String>,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub sandboxed: bool,
}

/// Write access for sandboxed commands: the workspace, temp folders, and common build caches.
/// Reads and network stay allowed so builds and dependency downloads work.
pub struct Sandbox<'a> {
    pub writable_root: &'a Path,
}

/// Whether the OS sandbox can actually be applied on this machine. `sandbox-exec` and `bwrap` exist
/// in environments that forbid them (a nested sandbox, a container, some CI images); wrapping every
/// command there makes each one fail with no output, so the sandbox is probed once and skipped when
/// it does not work. The probe runs the platform's no-op profile with a trivial command.
pub fn sandbox_available() -> bool {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USABLE.get_or_init(|| {
        let probe: Vec<String> = if cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists() {
            [SANDBOX_EXEC, "-p", "(version 1)(allow default)", "/usr/bin/true"].into_iter().map(str::to_owned).collect()
        } else if cfg!(target_os = "linux") {
            match ["/usr/bin/bwrap", "/bin/bwrap"].iter().find(|p| Path::new(p).exists()) {
                Some(bwrap) => [*bwrap, "--ro-bind", "/", "/", "--", "/bin/sh", "-c", "true"].into_iter().map(str::to_owned).collect(),
                None => return false,
            }
        } else {
            return false;
        };
        let never = CancellationToken::new();
        run(&probe, &std::env::temp_dir(), ".", Duration::from_secs(10), &never).is_ok_and(|o| o.success)
    })
}

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(Into::into)
}

const CACHE_DIRS: &[&str] = &[".cargo", ".rustup", ".npm", ".cache", ".pnpm-store", ".yarn", ".gradle", ".m2", "go", "Library/Caches"];

/// Wraps argv in the platform sandbox when one is available; returns the new argv and whether it applies.
fn sandboxed_argv(argv: &[String], sb: &Sandbox) -> (Vec<String>, bool) {
    if !sandbox_available() {
        return (argv.to_vec(), false);
    }
    let root = sb.writable_root.to_string_lossy().to_string();
    let safe = |p: &str| !p.contains(['"', '\\', '\n']);
    if cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists() && safe(&root) {
        let mut allow =
            vec![format!("(subpath \"{root}\")"), "(subpath \"/private/tmp\")".into(), "(subpath \"/private/var/folders\")".into()];
        if let Some(h) = home().map(|h| h.to_string_lossy().to_string()).filter(|h| safe(h)) {
            allow.extend(CACHE_DIRS.iter().map(|d| format!("(subpath \"{h}/{d}\")")));
        }
        let profile = format!(
            "(version 1)(allow default)(deny file-write*)(allow file-write* {} (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\") (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\"))",
            allow.join(" ")
        );
        let mut out = vec![SANDBOX_EXEC.into(), "-p".into(), profile];
        out.extend(argv.iter().cloned());
        return (out, true);
    }
    if cfg!(target_os = "linux") {
        if let Some(bwrap) = ["/usr/bin/bwrap", "/bin/bwrap"].iter().find(|p| Path::new(p).exists()) {
            let mut out: Vec<String> =
                [*bwrap, "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--bind", "/tmp", "/tmp", "--bind", &root, &root]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            if let Some(h) = home() {
                for d in CACHE_DIRS {
                    let p = h.join(d);
                    if p.is_dir() {
                        let p = p.to_string_lossy().to_string();
                        out.extend(["--bind".into(), p.clone(), p]);
                    }
                }
            }
            out.push("--".into());
            out.extend(argv.iter().cloned());
            return (out, true);
        }
    }
    (argv.to_vec(), false)
}

/// Runs a project command inside the sandbox when the platform supports one.
pub fn run_sandboxed(
    argv: &[String],
    cwd: &Path,
    cwd_label: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    sb: &Sandbox,
) -> Result<Output, ToolError> {
    if !SANDBOX.load(std::sync::atomic::Ordering::Relaxed) {
        return run(argv, cwd, cwd_label, timeout, cancel);
    }
    let (wrapped, applied) = sandboxed_argv(argv, sb);
    let mut out = run(&wrapped, cwd, cwd_label, timeout, cancel)?;
    out.argv = argv.to_vec();
    out.sandboxed = applied;
    Ok(out)
}

/// Global switch, set from the client (`initialize.sandbox`); on by default.
pub static SANDBOX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// How long a stream reader may take to see EOF once the command itself has ended. A descendant
/// that inherited the pipe (a detached daemon, a `head` inside a pipeline that outlived its parent)
/// can hold it open indefinitely; the command is over either way, so the output collected so far is
/// used and the wait stops here instead of hanging the tool.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: Vec<u8>,
    cut: bool,
    done: bool,
}

/// One stream's reader and the buffer it fills. The reader publishes every chunk it reads, so the
/// caller can take what has arrived even when EOF never comes.
struct Sink {
    state: Mutex<Captured>,
    changed: Condvar,
}

impl Sink {
    fn push(&self, chunk: &[u8]) {
        let half = MAX_STREAM_BYTES / 2;
        let mut g = self.state.lock().expect("capture lock");
        if g.head.len() < half {
            let take = chunk.len().min(half - g.head.len());
            g.head.extend_from_slice(&chunk[..take]);
            g.tail.extend_from_slice(&chunk[take..]);
        } else {
            g.tail.extend_from_slice(chunk);
        }
        if g.tail.len() > half * 2 {
            let cut = g.tail.len() - half;
            g.tail.drain(..cut);
            g.cut = true;
        }
    }

    fn finish(&self) {
        {
            let half = MAX_STREAM_BYTES / 2;
            let mut g = self.state.lock().expect("capture lock");
            if g.tail.len() > half {
                let cut = g.tail.len() - half;
                g.tail.drain(..cut);
                g.cut = true;
            }
            g.done = true;
        }
        self.changed.notify_all();
    }

    /// The output so far, waiting at most `timeout` for EOF.
    fn take(&self, timeout: Duration) -> (String, bool) {
        let deadline = Instant::now() + timeout;
        let mut g = self.state.lock().expect("capture lock");
        while !g.done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            g = match self.changed.wait_timeout(g, left) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        let mut out = std::mem::take(&mut g.head);
        let cut = g.cut;
        if cut {
            out.extend_from_slice(b"\n... [output truncated] ...\n");
        }
        out.extend_from_slice(&std::mem::take(&mut g.tail));
        (String::from_utf8_lossy(&out).into_owned(), cut)
    }
}

/// Keeps the head and tail of a stream; errors usually appear at the end.
fn capture(mut r: impl Read + Send + 'static) -> Arc<Sink> {
    let sink = Arc::new(Sink { state: Mutex::new(Captured::default()), changed: Condvar::new() });
    let writer = sink.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            writer.push(&buf[..n]);
        }
        writer.finish();
    });
    sink
}

#[cfg(unix)]
fn kill_tree(child: &mut std::process::Child) {
    let _ = Command::new("kill").args(["-KILL", &format!("-{}", child.id())]).stderr(Stdio::null()).status();
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_tree(child: &mut std::process::Child) {
    let _ =
        Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = child.kill();
}

/// Collects both streams, waiting at most `DRAIN_GRACE` in total, not per stream.
fn drain(out: &Sink, err: &Sink) -> ((String, bool), (String, bool)) {
    let deadline = Instant::now() + DRAIN_GRACE;
    let first = out.take(DRAIN_GRACE);
    let rest = deadline.saturating_duration_since(Instant::now());
    let second = err.take(rest);
    (first, second)
}

pub fn run(argv: &[String], cwd: &Path, cwd_label: &str, timeout: Duration, cancel: &CancellationToken) -> Result<Output, ToolError> {
    run_with_input(argv, cwd, cwd_label, timeout, cancel, None)
}

/// Like `run`, optionally writing `input` to the child's stdin (then closing it).
pub fn run_with_input(
    argv: &[String],
    cwd: &Path,
    cwd_label: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    input: Option<&str>,
) -> Result<Output, ToolError> {
    let (prog, args) = argv.split_first().ok_or_else(|| ToolError::invalid("empty command"))?;
    if argv.iter().any(|a| a.contains('\0')) {
        return Err(ToolError::invalid("arguments must not contain NUL"));
    }
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat");
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("could not start {prog}: {e}")))?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        let text = text.to_owned();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let out = capture(child.stdout.take().expect("piped stdout"));
    let err = capture(child.stderr.take().expect("piped stderr"));
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| ToolError::io(e, prog))? {
            break Some(s);
        }
        if cancel.is_cancelled() {
            kill_tree(&mut child);
            let _ = child.wait();
            return Err(ToolError::new(ToolErrorKind::Cancelled, "command cancelled"));
        }
        if started.elapsed() > timeout {
            timed_out = true;
            kill_tree(&mut child);
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(POLL);
    };
    let ((so, t1), (se, t2)) = drain(&out, &err);
    let code = status.and_then(|s| s.code());
    Ok(Output {
        argv: argv.to_vec(),
        cwd: cwd_label.to_owned(),
        exit_code: code,
        success: status.is_some_and(|s| s.success()),
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout: so,
        stderr: se,
        truncated: t1 || t2,
        sandboxed: false,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn builds_tests_and_installs_count_as_long() {
        for long in [
            "cargo build --workspace",
            "sh -c cargo test 2>&1",
            "bash -lc 'npm ci'",
            "cd web && CI=1 npm --prefix x run build",
            "flutter pub get",
            "go test ./...",
        ] {
            assert!(is_long_command(long), "{long}");
        }
        for quick in ["git status", "ls -la", "echo cargo build", "cat Cargo.toml", "npm -v && echo test", "rustc --version"] {
            assert!(!is_long_command(quick), "{quick}");
        }
    }

    /// A quick command whose output pipe is also held by a surviving child (`cmd & `, a pipeline
    /// whose `head` outlived it, a detached daemon). The command is over, so the tool must return its
    /// output at once instead of waiting for a pipe that may never close.
    #[test]
    fn a_pipe_held_open_by_a_child_does_not_hang_the_tool() {
        let tmp = std::env::temp_dir();
        let c = CancellationToken::new();
        let started = Instant::now();
        let o = run(&argv(&["sh", "-c", "echo errors: 3; (sleep 30 &)"]), &tmp, ".", Duration::from_secs(20), &c).unwrap();
        assert!(started.elapsed() < DRAIN_GRACE + Duration::from_secs(2), "returned in {:?}", started.elapsed());
        assert_eq!(o.stdout.trim(), "errors: 3", "the output written before the hand-off is kept");
        assert!(!o.timed_out && o.success, "the command itself succeeded");
    }

    #[test]
    fn runs_captures_and_times_out() {
        let tmp = std::env::temp_dir();
        let c = CancellationToken::new();
        let o = run(&argv(&["sh", "-c", "echo hi; echo err >&2; exit 3"]), &tmp, ".", Duration::from_secs(5), &c).unwrap();
        assert_eq!((o.stdout.as_str(), o.stderr.as_str(), o.exit_code, o.success), ("hi\n", "err\n", Some(3), false));
        let o = run(&argv(&["sh", "-c", "sleep 5 & sleep 5"]), &tmp, ".", Duration::from_millis(200), &c).unwrap();
        assert!(o.timed_out && o.duration_ms < 2_000);
        let o = run(&argv(&["sh", "-c", "yes | head -c 200000"]), &tmp, ".", Duration::from_secs(5), &c).unwrap();
        assert!(o.truncated && o.stdout.len() < MAX_STREAM_BYTES + 100);
        c.cancel();
        assert_eq!(run(&argv(&["sleep", "5"]), &tmp, ".", Duration::from_secs(5), &c).unwrap_err().kind, ToolErrorKind::Cancelled);
        assert!(run(&argv(&["definitely-not-a-binary-xyz"]), &tmp, ".", Duration::from_secs(1), &CancellationToken::new()).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_blocks_writes_outside_the_workspace() {
        let ws = std::env::temp_dir().join(format!("anvil-sb-{}", std::process::id())).canonicalize().unwrap_or_else(|_| {
            let p = std::env::temp_dir().join(format!("anvil-sb-{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            p.canonicalize().unwrap()
        });
        let outside = home().unwrap().join(format!(".anvil-sandbox-probe-{}", std::process::id()));
        let c = CancellationToken::new();
        let sb = Sandbox { writable_root: &ws };
        if !sandbox_available() {
            // Nothing can be wrapped here (a nested sandbox or a container): the command must still
            // run, and the result must say it was not sandboxed.
            let o = run_sandboxed(&argv(&["sh", "-c", "echo ok > inside.txt"]), &ws, ".", Duration::from_secs(10), &c, &sb).unwrap();
            assert!(o.success && !o.sandboxed, "{}", o.stderr);
            std::fs::remove_dir_all(&ws).ok();
            return;
        }
        let inside = run_sandboxed(&argv(&["sh", "-c", "echo ok > inside.txt"]), &ws, ".", Duration::from_secs(10), &c, &sb).unwrap();
        assert!(inside.success && inside.sandboxed, "{}", inside.stderr);
        let script = format!("echo bad > '{}'", outside.display());
        let o = run_sandboxed(&argv(&["sh", "-c", &script]), &ws, ".", Duration::from_secs(10), &c, &sb).unwrap();
        assert!(!o.success, "write outside the workspace must fail");
        assert!(!outside.exists());
        std::fs::remove_dir_all(&ws).ok();
    }
}
