//! The commands task-x runs for the agent (`run_command`, `run_tests`, `check_build`, ...), kept
//! with their live output so a client can show each one as a terminal: `procs/list`,
//! `procs/output`, `procs/stop`, `procs/clear`, and the notifications `procs/started`,
//! `procs/output`, and `procs/ended`.
//!
//! The registry keeps the newest `MAX_KEPT` commands and, per command, the last `MAX_LOG_BYTES` of
//! its output (stdout and stderr in the order they arrive). Notifications go to the sink the server
//! sets; with none (the terminal chat, tests) nothing is sent.

use serde::Serialize;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Commands kept in the list; the oldest finished ones go first.
const MAX_KEPT: usize = 100;
/// Output kept per command for a terminal opened later (`procs/output`).
const MAX_LOG_BYTES: usize = 512 * 1024;
/// Output held between two `procs/output` notifications; a flood keeps only its newest part.
const MAX_PENDING_BYTES: usize = 64 * 1024;
const MAX_COMMAND_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Running,
    /// Ended with exit code 0.
    Exited,
    /// Ended with another exit code, or by a signal.
    Failed,
    /// Stopped by the user (`procs/stop`) or with its task.
    Stopped,
    TimedOut,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub command: String,
    pub cwd: String,
    pub pid: u32,
    pub started_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

struct State {
    info: Info,
    log: Vec<u8>,
    /// The log lost its start (more than `MAX_LOG_BYTES` arrived).
    cut: bool,
    /// Output not sent yet.
    pending: Vec<u8>,
    /// Bytes dropped from `pending` since the last notification.
    skipped: usize,
    /// Bytes of output accounted for so far (sent, or skipped): `procs/output` notifications carry
    /// their range, and the `procs/output` answer its end, so a client can join them exactly.
    sent: u64,
}

/// One command in the list.
pub struct Proc {
    state: Mutex<State>,
    stop: CancellationToken,
}

type Sink = Arc<dyn Fn(&str, Value) + Send + Sync>;
static SINK: RwLock<Option<Sink>> = RwLock::new(None);
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry { next: 1, procs: Vec::new() }));

struct Registry {
    next: u64,
    procs: Vec<Arc<Proc>>,
}

/// Where `procs/*` notifications go (the JSON-RPC client).
pub fn set_sink(sink: Sink) {
    *SINK.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
}

fn emit(method: &str, params: Value) {
    let sink = SINK.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    if let Some(s) = sink {
        s(method, params);
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How a command is shown: the script of `sh -c '…'`, else argv with the arguments that need it quoted.
pub fn display(argv: &[String]) -> String {
    let shell = argv.first().is_some_and(|p| matches!(p.rsplit('/').next(), Some("sh" | "bash" | "zsh")));
    let text = if shell && argv.len() == 3 && matches!(argv[1].as_str(), "-c" | "-lc" | "-ic") {
        argv[2].clone()
    } else {
        argv.iter()
            .map(|a| {
                if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
                    a.clone()
                } else {
                    format!("'{}'", a.replace('\'', r"'\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    if text.chars().count() > MAX_COMMAND_CHARS { format!("{}…", text.chars().take(MAX_COMMAND_CHARS).collect::<String>()) } else { text }
}

/// Adds a started command to the list. `stop` ends this command only (`procs/stop`).
pub fn start(command: &str, cwd: &str, pid: u32, task_id: Option<&str>, stop: CancellationToken) -> Arc<Proc> {
    let p = {
        let mut r = registry();
        let id = format!("p{}", r.next);
        r.next += 1;
        let info = Info {
            id,
            task_id: task_id.map(str::to_owned),
            command: command.to_owned(),
            cwd: cwd.to_owned(),
            pid,
            started_ms: now_ms(),
            ended_ms: None,
            status: Status::Running,
            exit_code: None,
        };
        let p = Arc::new(Proc { state: Mutex::new(State { info, log: Vec::new(), cut: false, pending: Vec::new(), skipped: 0, sent: 0 }), stop });
        r.procs.push(p.clone());
        while r.procs.len() > MAX_KEPT {
            match r.procs.iter().position(|p| p.info().status != Status::Running) {
                Some(i) => drop(r.procs.remove(i)),
                None => break,
            }
        }
        p
    };
    emit("procs/started", json!({ "proc": p.info() }));
    p
}

/// The text that can be sent now: whole UTF-8 characters only, unless `all`.
fn take_text(pending: &mut Vec<u8>, all: bool) -> String {
    let n = match std::str::from_utf8(pending) {
        Err(e) if !all && e.error_len().is_none() => e.valid_up_to(),
        _ => pending.len(),
    };
    let bytes: Vec<u8> = pending.drain(..n).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Proc {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn info(&self) -> Info {
        self.lock().info.clone()
    }

    /// Output read from the command (either stream).
    pub fn push(&self, chunk: &[u8]) {
        let mut s = self.lock();
        s.log.extend_from_slice(chunk);
        // Trimmed in steps, not on every chunk; align to a UTF-8 character boundary.
        if s.log.len() > MAX_LOG_BYTES + MAX_LOG_BYTES / 4 {
            let mut cut = s.log.len() - MAX_LOG_BYTES;
            while cut < s.log.len() && (s.log[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            s.log.drain(..cut);
            s.cut = true;
        }
        if s.info.status != Status::Running {
            return;
        }
        s.pending.extend_from_slice(chunk);
        if s.pending.len() > MAX_PENDING_BYTES {
            let mut cut = s.pending.len() - MAX_PENDING_BYTES;
            while cut < s.pending.len() && (s.pending[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            s.pending.drain(..cut);
            s.skipped += cut;
        }
    }

    /// Sends the output that arrived since the last call (`procs/output`).
    pub fn flush(&self) {
        self.send(false);
    }

    /// Sends the pending output with its byte range (`from`..`to`). The `procs/output` answer gives
    /// the end of what it holds (`to`): a client appends the notifications from there on, and every
    /// byte shows once, whichever of the two arrives first.
    fn send(&self, all: bool) {
        let payload = {
            let mut s = self.lock();
            if s.pending.is_empty() && s.skipped == 0 {
                return;
            }
            let from = s.sent;
            let mut data = String::new();
            if s.skipped > 0 {
                data.push_str(&format!("\n[… {} bytes of output skipped …]\n", s.skipped));
                s.sent += s.skipped as u64;
                s.skipped = 0;
            }
            let before = s.pending.len();
            data.push_str(&take_text(&mut s.pending, all));
            s.sent += (before - s.pending.len()) as u64;
            (!data.is_empty()).then(|| json!({ "id": s.info.id, "data": data, "from": from, "to": s.sent }))
        };
        if let Some(payload) = payload {
            emit("procs/output", payload);
        }
    }

    /// The command ended: sends the rest of its output, then `procs/ended`.
    pub fn end(&self, status: Status, exit_code: Option<i32>) {
        self.send(true);
        let info = {
            let mut s = self.lock();
            s.info.status = status;
            s.info.exit_code = exit_code;
            s.info.ended_ms = Some(now_ms());
            s.pending.clear();
            s.info.clone()
        };
        emit("procs/ended", json!({ "proc": info }));
    }

    /// Whether `procs/stop` asked this command to end.
    pub fn stop_requested(&self) -> bool {
        self.stop.is_cancelled()
    }
}

fn find(id: &str) -> Option<Arc<Proc>> {
    registry().procs.iter().find(|p| p.lock().info.id == id).cloned()
}

/// `procs/list`: every kept command, oldest first.
pub fn list() -> Value {
    let procs: Vec<Info> = registry().procs.iter().map(|p| p.info()).collect();
    json!({ "procs": procs })
}

/// `procs/output`: the kept output of one command, for a terminal opened after it started: what
/// was accounted for up to `to` (the notifications from `to` on carry the rest).
pub fn output(id: &str) -> Result<Value, String> {
    let p = find(id).ok_or_else(|| format!("no command {id}"))?;
    let s = p.lock();
    let kept = s.log.len().saturating_sub(s.pending.len() + s.skipped.min(s.log.len()));
    Ok(json!({ "proc": s.info, "data": String::from_utf8_lossy(&s.log[..kept]), "truncated": s.cut, "to": s.sent }))
}

/// `procs/stop`: ends a running command and everything it started. False when it already ended.
pub fn stop(id: &str) -> Result<bool, String> {
    let p = find(id).ok_or_else(|| format!("no command {id}"))?;
    if p.info().status != Status::Running {
        return Ok(false);
    }
    p.stop.cancel();
    Ok(true)
}

/// `procs/clear`: drops the commands that ended. Returns how many.
pub fn clear() -> usize {
    let mut r = registry();
    let before = r.procs.len();
    r.procs.retain(|p| p.info().status == Status::Running);
    before - r.procs.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shows_scripts_and_quotes_arguments() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(display(&v(&["sh", "-c", "npm run dev | tee log"])), "npm run dev | tee log");
        assert_eq!(display(&v(&["cargo", "test", "--", "a b"])), "cargo test -- 'a b'");
        assert_eq!(display(&v(&["echo", "it's"])), r"echo 'it'\''s'");
    }

    #[test]
    fn utf8_is_sent_in_whole_characters() {
        let mut p = "aé".as_bytes().to_vec();
        p.pop();
        assert_eq!(take_text(&mut p, false), "a");
        assert_eq!(p.len(), 1, "the first byte of é waits for the rest");
        p.push(0xA9);
        assert_eq!(take_text(&mut p, false), "é");
    }

    #[test]
    fn keeps_output_lists_stops_and_clears() {
        let p = start("echo hi", ".", 1, Some("t-procs"), CancellationToken::new());
        let id = p.info().id;
        p.push(b"hello\n");
        let early = output(&id).unwrap();
        assert_eq!((early["data"].as_str(), early["to"].as_u64()), (Some(""), Some(0)), "output not sent yet comes with the next notification");
        p.flush();
        let sent = output(&id).unwrap();
        assert_eq!((sent["data"].as_str(), sent["to"].as_u64()), (Some("hello\n"), Some(6)), "the answer says where the notifications go on");
        assert!(list()["procs"].as_array().unwrap().iter().any(|v| v["id"] == id.as_str() && v["status"] == "running"));
        assert!(stop(&id).unwrap() && p.stop_requested());
        p.end(Status::Stopped, None);
        assert!(!stop(&id).unwrap(), "an ended command cannot be stopped again");
        assert_eq!(output(&id).unwrap()["proc"]["status"], "stopped");
        assert!(output("p-none").is_err() && stop("p-none").is_err(), "unknown ids are errors");
        // `clear` is not called here: the list is shared with the exec tests running beside this one.
    }
}
