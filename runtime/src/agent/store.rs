//! Task persistence: one JSON file per task, written atomically after every step so a task can
//! resume after a crash, restart, or cancellation.

use super::prompt::Enriched;
use crate::provider::{Message, Usage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const RECORD_VERSION: u32 = 1;
/// Largest task record the list reads; a bigger file is skipped instead of parsed.
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Running,
    Completed,
    Cancelled,
    MaxIterations,
    BudgetExceeded,
    DeadlineExceeded,
    ProviderError,
    Stuck,
    Failed,
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

    /// Marks the records a crashed or killed runtime left as `running` as failed. Nothing runs before
    /// this at startup, so a `running` record is always stale; without this the task list and the
    /// status bar would show phantom work forever. Returns how many records were updated.
    pub fn fail_interrupted(&self) -> usize {
        let Some(dir) = &self.dir else { return 0 };
        let mut updated = 0;
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if !e.path().extension().is_some_and(|x| x == "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let Ok(mut r) = serde_json::from_slice::<TaskRecord>(&bytes) else { continue };
            if r.status != Status::Running {
                continue;
            }
            r.status = Status::Failed;
            r.error = Some("the runtime stopped before this task finished".into());
            r.updated_ms = now_ms();
            if self.save(&r).is_ok() {
                updated += 1;
            }
        }
        updated
    }
}

pub fn enrichment_json(e: &Enriched) -> serde_json::Value {
    serde_json::to_value(e).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: Status) -> TaskRecord {
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
        }
    }

    #[test]
    fn interrupted_runs_are_marked_failed() {
        let dir = std::env::temp_dir().join(format!("anvil-store-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::new(Some(&dir));
        store.save(&record("a", Status::Running)).unwrap();
        store.save(&record("b", Status::Running)).unwrap();
        store.save(&record("c", Status::Completed)).unwrap();
        assert_eq!(store.fail_interrupted(), 2);
        let a = store.load("a").unwrap();
        assert_eq!(a.status, Status::Failed);
        assert!(a.error.as_deref().unwrap().contains("stopped before this task finished"));
        assert_eq!(store.load("c").unwrap().status, Status::Completed, "finished tasks are left alone");
        assert_eq!(store.fail_interrupted(), 0, "a second start finds nothing stale");
        std::fs::remove_dir_all(dir).ok();
    }
}
