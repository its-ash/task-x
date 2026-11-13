//! Task persistence: one JSON file per task, written atomically after every step so a task can
//! resume after a crash, restart, or cancellation.

use super::prompt::Enriched;
use crate::provider::{Message, Usage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const RECORD_VERSION: u32 = 1;
/// The `error` of an interrupted task.
const INTERRUPTED: &str = "the runtime stopped before this task finished; send an empty prompt with its taskId to continue it";
/// Largest task record the list reads; a bigger file is skipped instead of parsed.
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Running,
    /// Idle until a wake-up (`schedule_wakeup`, `wait_until`) fires or the user sends a message.
    Sleeping,
    Completed,
    Cancelled,
    MaxIterations,
    BudgetExceeded,
    DeadlineExceeded,
    ProviderError,
    Stuck,
    Failed,
    /// The runtime stopped (VS Code reloaded or quit, the runtime crashed) before the task ended.
    /// Its conversation is saved: `agent/run` with its `taskId` and an empty prompt continues it.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationRun {
    pub tool: String,
    pub command: Option<String>,
    pub success: bool,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub status: Status,
    pub chat_mode: super::ChatMode,
    pub provider_kind: String,
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub iterations: u32,
    pub tool_calls: u32,
    pub changed_files: BTreeSet<String>,
    pub operations: Vec<usize>,
    pub validations: Vec<ValidationRun>,
    pub final_text: String,
    pub error: Option<String>,
    #[serde(default)]
    pub enrichment: Option<serde_json::Value>,
    #[serde(default)]
    pub metrics: super::Metrics,
    /// Detected checks (format, lint, test, build) that did not run although files changed.
    #[serde(default)]
    pub skipped_checks: Vec<String>,
    /// One per user prompt: where the conversation and the edit list stood before it, for rewind.
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
    /// The task this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

/// The state before one user prompt: `messages` and `operations` are lengths to cut back to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub messages: usize,
    pub operations: usize,
    /// The start of the prompt, for the list the user picks from.
    pub prompt: String,
    pub created_ms: u64,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("t{}{:04}", now_ms(), N.fetch_add(1, Ordering::SeqCst) % 10_000)
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Clone)]
pub struct Store {
    dir: Option<PathBuf>,
}

impl Store {
    pub fn new(data_dir: Option<&Path>) -> Self {
        Self { dir: data_dir.map(|d| d.join("tasks")) }
    }

    /// The runtime's data folder (the parent of `tasks`), for the semantic and docs indexes.
    pub fn data_dir(&self) -> Option<PathBuf> {
        self.dir.as_ref().and_then(|d| d.parent()).map(Path::to_path_buf)
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        valid_id(id).then(|| self.dir.as_ref().map(|d| d.join(format!("{id}.json")))).flatten()
    }

    pub fn save(&self, r: &TaskRecord) -> std::io::Result<()> {
        let Some(p) = self.path(&r.id) else { return Ok(()) };
        std::fs::create_dir_all(p.parent().expect("tasks dir"))?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(r).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, p)
    }

    pub fn load(&self, id: &str) -> Option<TaskRecord> {
        let p = self.path(id)?;
        let r: TaskRecord = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
        (r.version <= RECORD_VERSION).then_some(r)
    }

    /// Newest first: id, title, status, updated time.
    pub fn list(&self, limit: usize) -> Vec<serde_json::Value> {
        let Some(dir) = &self.dir else { return Vec::new() };
        let mut out: Vec<TaskRecord> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            // A damaged or oversized record must not stall the client's task list.
            .filter(|e| e.metadata().is_ok_and(|m| m.len() <= MAX_RECORD_BYTES))
            .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.updated_ms));
        out.into_iter()
            .take(limit)
            .map(|r| serde_json::json!({ "id": r.id, "title": r.title, "status": r.status, "chatMode": r.chat_mode, "updatedMs": r.updated_ms, "changedFiles": r.changed_files }))
            .collect()
    }

    pub fn delete(&self, id: &str) -> bool {
        self.path(id).is_some_and(|p| std::fs::remove_file(p).is_ok())
    }

    /// The saved records, read one by one (unreadable files are skipped).
    fn records(&self) -> impl Iterator<Item = TaskRecord> + '_ {
        self.dir.iter().flat_map(std::fs::read_dir).flatten().flatten().filter_map(|e| {
            if !e.path().extension().is_some_and(|x| x == "json") {
                return None;
            }
            serde_json::from_slice::<TaskRecord>(&std::fs::read(e.path()).ok()?).ok()
        })
    }

    /// Ids of the tasks saved as running or sleeping.
    pub fn active_ids(&self) -> Vec<String> {
        self.records().filter(|r| matches!(r.status, Status::Running | Status::Sleeping)).map(|r| r.id).collect()
    }

    fn mark_interrupted(&self, mut r: TaskRecord) -> bool {
        r.status = Status::Interrupted;
        r.error = Some(INTERRUPTED.into());
        r.updated_ms = now_ms();
        self.save(&r).is_ok()
    }

    /// Marks the records a crashed or killed runtime left as `running` (or `sleeping`) as
    /// interrupted. Nothing runs before this at startup, so such a record is always stale; without
    /// this the task list and the status bar would show phantom work forever. Returns how many
    /// records were updated.
    pub fn interrupt_stale(&self) -> usize {
        // A sleeping task's wake-ups lived in the runtime that stopped: they will never fire.
        let stale: Vec<TaskRecord> = self.records().filter(|r| matches!(r.status, Status::Running | Status::Sleeping)).collect();
        stale.into_iter().filter(|r| self.mark_interrupted(r.clone())).count()
    }

    /// Marks these tasks, stopped because the runtime is shutting down, as interrupted instead of
    /// cancelled: the user did not stop them. Tasks that ended on their own are left alone.
    pub fn interrupt(&self, ids: &[String]) -> usize {
        ids.iter()
            .filter_map(|id| self.load(id))
            .filter(|r| matches!(r.status, Status::Running | Status::Sleeping | Status::Cancelled))
            .filter(|r| self.mark_interrupted(r.clone()))
            .count()
    }
}

pub fn enrichment_json(e: &Enriched) -> serde_json::Value {
    serde_json::to_value(e).unwrap_or_default()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn record(id: &str, status: Status) -> TaskRecord {
        TaskRecord {
            version: RECORD_VERSION,
            id: id.into(),
            title: "t".into(),
            created_ms: 0,
            updated_ms: 0,
            status,
            chat_mode: super::super::ChatMode::Agent,
            provider_kind: "OpenAiCompatible".into(),
            model: "m".into(),
            system: "s".into(),
            messages: Vec::new(),
            usage: Usage::default(),
            iterations: 0,
            tool_calls: 0,
            changed_files: Default::default(),
            operations: Vec::new(),
            validations: Vec::new(),
            final_text: String::new(),
            error: None,
            enrichment: None,
            metrics: Default::default(),
            skipped_checks: Vec::new(),
            checkpoints: Vec::new(),
            forked_from: None,
        }
    }

    #[test]
    fn stale_runs_are_marked_interrupted() {
        let dir = std::env::temp_dir().join(format!("anvil-store-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::new(Some(&dir));
        store.save(&record("a", Status::Running)).unwrap();
        store.save(&record("b", Status::Sleeping)).unwrap();
        store.save(&record("c", Status::Completed)).unwrap();
        assert_eq!(store.active_ids().len(), 2);
        assert_eq!(store.interrupt_stale(), 2);
        assert_eq!(store.load("b").unwrap().status, Status::Interrupted, "a sleeping task's wake-ups died with the runtime");
        let a = store.load("a").unwrap();
        assert_eq!(a.status, Status::Interrupted);
        assert!(a.error.as_deref().unwrap().contains("stopped before this task finished"));
        assert_eq!(store.load("c").unwrap().status, Status::Completed, "finished tasks are left alone");
        assert_eq!(store.interrupt_stale(), 0, "a second start finds nothing stale");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tasks_stopped_by_a_shutdown_are_interrupted_not_cancelled() {
        let dir = std::env::temp_dir().join(format!("anvil-store-shutdown-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::new(Some(&dir));
        store.save(&record("a", Status::Cancelled)).unwrap();
        store.save(&record("b", Status::Completed)).unwrap();
        store.save(&record("c", Status::Cancelled)).unwrap();
        assert_eq!(store.interrupt(&["a".into(), "b".into(), "missing".into()]), 1);
        assert_eq!(store.load("a").unwrap().status, Status::Interrupted);
        assert_eq!(store.load("b").unwrap().status, Status::Completed, "a task that ended on its own stays as it ended");
        assert_eq!(store.load("c").unwrap().status, Status::Cancelled, "only the tasks running at shutdown change");
        std::fs::remove_dir_all(dir).ok();
    }
}
