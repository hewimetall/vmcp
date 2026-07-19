//! Native MCP Tasks engine (SEP-1686) with durable SQLite storage.
//!
//! Modelled after mcp-presentation's embedded TaskStore (`state/tasks.db`):
//! WAL SQLite is the source of truth across restarts; an in-process [`Notify`]
//! map wakes `tasks/result` waiters without busy-spinning.
//!
//! The only task-augmentable MCP surface is [`RUN_TASK_TOOL`] (`run_task`), which
//! proxies to upstream tools that advertise `execution.taskSupport` (or a
//! sidecar override). GraphQL remains the sync path — the gateway awaits
//! upstream there; task-aware clients use `run_task` + `task`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::Mutex;
use rmcp::model::{
    CancelTaskResult, CreateTaskResult, GetTaskResult, ListTasksResult, Task, TaskStatus,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

use vmcp_upstream::UpstreamPool;

/// MCP tool name for the task-capable upstream proxy.
pub const RUN_TASK_TOOL: &str = "run_task";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    task_id          TEXT PRIMARY KEY,
    owner            TEXT NOT NULL,
    server           TEXT NOT NULL,
    tool             TEXT NOT NULL,
    status           TEXT NOT NULL,
    status_message   TEXT,
    result_json      TEXT,
    ttl_ms           INTEGER,
    poll_interval_ms INTEGER,
    cancel           INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    last_updated_at  TEXT NOT NULL,
    created_unix_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_owner_created
  ON tasks(owner, created_unix_ms);
CREATE INDEX IF NOT EXISTS idx_tasks_status
  ON tasks(status);
"#;

/// Error raised by store lookups, mapped to JSON-RPC codes by the caller.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("task {0} is already in a terminal status")]
    AlreadyTerminal(String),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn status_to_str(s: &TaskStatus) -> &'static str {
    match s {
        TaskStatus::Working => "working",
        TaskStatus::InputRequired => "input_required",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn status_from_str(s: &str) -> TaskStatus {
    match s {
        "working" => TaskStatus::Working,
        "input_required" => TaskStatus::InputRequired,
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::Working,
    }
}

fn is_terminal_status(s: &TaskStatus) -> bool {
    matches!(
        s,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

fn open_db(path: &Path) -> Result<Connection, TaskError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(TaskError::Io)?;
        }
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=30000;
         PRAGMA foreign_keys=ON;",
    )?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

#[derive(Debug, Clone)]
struct TaskRow {
    task_id: String,
    owner: String,
    status: TaskStatus,
    status_message: Option<String>,
    result_json: Option<String>,
    ttl_ms: Option<u64>,
    poll_interval_ms: Option<u64>,
    cancel: bool,
    created_at: String,
    last_updated_at: String,
    #[allow(dead_code)]
    created_unix_ms: i64,
}

impl TaskRow {
    fn to_task(&self) -> Task {
        let mut t = Task::new(
            self.task_id.clone(),
            self.status.clone(),
            self.created_at.clone(),
            self.last_updated_at.clone(),
        );
        t.status_message = self.status_message.clone();
        t.ttl = self.ttl_ms;
        t.poll_interval = self.poll_interval_ms;
        t
    }
}

/// Durable SEP-1686 task registry backed by embedded SQLite.
pub struct TaskStore {
    conn: Mutex<Connection>,
    /// Wake `tasks/result` waiters when a task reaches a terminal status.
    waiters: DashMap<String, Arc<Notify>>,
    default_ttl_ms: u64,
    default_poll_ms: u64,
    /// Soft cancel flags observed by in-flight workers (also mirrored in SQLite).
    cancel_flags: DashMap<String, AtomicBool>,
}

impl TaskStore {
    pub fn open(
        path: impl AsRef<Path>,
        default_ttl_ms: u64,
        default_poll_ms: u64,
    ) -> Result<Self, TaskError> {
        Ok(Self {
            conn: Mutex::new(open_db(path.as_ref())?),
            waiters: DashMap::new(),
            default_ttl_ms,
            default_poll_ms,
            cancel_flags: DashMap::new(),
        })
    }

    fn read_row(conn: &Connection, task_id: &str) -> Result<Option<TaskRow>, TaskError> {
        let row = conn
            .query_row(
                "SELECT task_id, owner, status, status_message, result_json,
                        ttl_ms, poll_interval_ms, cancel, created_at, last_updated_at,
                        created_unix_ms
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok(TaskRow {
                        task_id: r.get(0)?,
                        owner: r.get(1)?,
                        status: status_from_str(&r.get::<_, String>(2)?),
                        status_message: r.get(3)?,
                        result_json: r.get(4)?,
                        ttl_ms: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                        poll_interval_ms: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                        cancel: r.get::<_, i64>(7)? != 0,
                        created_at: r.get(8)?,
                        last_updated_at: r.get(9)?,
                        created_unix_ms: r.get(10)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn wake(&self, task_id: &str) {
        if let Some(n) = self.waiters.get(task_id) {
            n.notify_waiters();
        }
    }

    /// Create a fresh `Working` task. Returns its id.
    pub fn create(
        &self,
        owner: impl Into<String>,
        server: impl Into<String>,
        tool: impl Into<String>,
    ) -> Result<String, TaskError> {
        let id = Uuid::new_v4().to_string();
        let owner = owner.into();
        let server = server.into();
        let tool = tool.into();
        let now = now_iso();
        let unix_ms = now_unix_ms();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO tasks(
                task_id, owner, server, tool, status, status_message,
                ttl_ms, poll_interval_ms, cancel, created_at, last_updated_at, created_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, 'working', ?5, ?6, ?7, 0, ?8, ?8, ?9)",
            params![
                id,
                owner,
                server,
                tool,
                "The operation is now in progress.",
                self.default_ttl_ms as i64,
                self.default_poll_ms as i64,
                now,
                unix_ms,
            ],
        )?;
        self.cancel_flags.insert(id.clone(), AtomicBool::new(false));
        Ok(id)
    }

    fn entry_for_owner(&self, task_id: &str, owner: &str) -> Result<TaskRow, TaskError> {
        let conn = self.conn.lock();
        let row = Self::read_row(&conn, task_id)?
            .ok_or_else(|| TaskError::NotFound(task_id.to_string()))?;
        if row.owner != owner {
            return Err(TaskError::NotFound(task_id.to_string()));
        }
        Ok(row)
    }

    pub fn is_cancelled(&self, task_id: &str) -> bool {
        if let Some(f) = self.cancel_flags.get(task_id) {
            if f.load(Ordering::SeqCst) {
                return true;
            }
        }
        let conn = self.conn.lock();
        Self::read_row(&conn, task_id)
            .ok()
            .flatten()
            .map(|r| {
                r.cancel
                    || is_terminal_status(&r.status) && matches!(r.status, TaskStatus::Cancelled)
            })
            .unwrap_or(true)
    }

    fn finish(
        &self,
        task_id: &str,
        status: TaskStatus,
        message: Option<String>,
        result: Option<Value>,
    ) -> Result<(), TaskError> {
        let conn = self.conn.lock();
        let Some(row) = Self::read_row(&conn, task_id)? else {
            return Ok(());
        };
        if is_terminal_status(&row.status) {
            return Ok(());
        }
        let result_json = result.as_ref().and_then(|v| serde_json::to_string(v).ok());
        let now = now_iso();
        conn.execute(
            "UPDATE tasks SET
                status = ?2,
                status_message = COALESCE(?3, status_message),
                result_json = COALESCE(?4, result_json),
                last_updated_at = ?5
             WHERE task_id = ?1",
            params![task_id, status_to_str(&status), message, result_json, now],
        )?;
        drop(conn);
        self.wake(task_id);
        Ok(())
    }

    pub fn complete(&self, task_id: &str, result: Value, message: Option<String>) {
        let _ = self.finish(task_id, TaskStatus::Completed, message, Some(result));
    }

    pub fn fail(&self, task_id: &str, message: impl Into<String>, result: Option<Value>) {
        let _ = self.finish(task_id, TaskStatus::Failed, Some(message.into()), result);
    }

    pub fn get(&self, task_id: &str, owner: &str) -> Result<GetTaskResult, TaskError> {
        let row = self.entry_for_owner(task_id, owner)?;
        Ok(GetTaskResult {
            meta: None,
            task: row.to_task(),
        })
    }

    pub fn cancel(&self, task_id: &str, owner: &str) -> Result<CancelTaskResult, TaskError> {
        let row = self.entry_for_owner(task_id, owner)?;
        if is_terminal_status(&row.status) {
            return Err(TaskError::AlreadyTerminal(task_id.to_string()));
        }
        if let Some(f) = self.cancel_flags.get(task_id) {
            f.store(true, Ordering::SeqCst);
        } else {
            self.cancel_flags
                .insert(task_id.to_string(), AtomicBool::new(true));
        }
        let now = now_iso();
        {
            let conn = self.conn.lock();
            conn.execute(
                "UPDATE tasks SET status = 'cancelled', cancel = 1,
                    status_message = ?2, last_updated_at = ?3
                 WHERE task_id = ?1",
                params![task_id, "The task was cancelled by request.", now],
            )?;
        }
        self.wake(task_id);
        let row = self.entry_for_owner(task_id, owner)?;
        Ok(CancelTaskResult {
            meta: None,
            task: row.to_task(),
        })
    }

    pub fn list(&self, owner: &str) -> ListTasksResult {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT task_id, owner, status, status_message, result_json,
                    ttl_ms, poll_interval_ms, cancel, created_at, last_updated_at,
                    created_unix_ms
             FROM tasks WHERE owner = ?1
             ORDER BY created_unix_ms ASC",
        ) {
            Ok(s) => s,
            Err(_) => return ListTasksResult::new(vec![]),
        };
        let rows = stmt
            .query_map(params![owner], |r| {
                Ok(TaskRow {
                    task_id: r.get(0)?,
                    owner: r.get(1)?,
                    status: status_from_str(&r.get::<_, String>(2)?),
                    status_message: r.get(3)?,
                    result_json: r.get(4)?,
                    ttl_ms: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    poll_interval_ms: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    cancel: r.get::<_, i64>(7)? != 0,
                    created_at: r.get(8)?,
                    last_updated_at: r.get(9)?,
                    created_unix_ms: r.get(10)?,
                })
            })
            .ok();
        let mut tasks = Vec::new();
        if let Some(rows) = rows {
            for row in rows.flatten() {
                tasks.push(row.to_task());
            }
        }
        ListTasksResult::new(tasks)
    }

    /// Block until the task is terminal, then return the stored `CallToolResult` JSON.
    pub async fn await_result(&self, task_id: &str, owner: &str) -> Result<Value, TaskError> {
        loop {
            let row = self.entry_for_owner(task_id, owner)?;
            if is_terminal_status(&row.status) {
                if let Some(raw) = row.result_json {
                    return Ok(serde_json::from_str(&raw).unwrap_or(Value::Null));
                }
                return Ok(synthetic_terminal_payload(
                    &row.status,
                    row.status_message.as_deref(),
                ));
            }
            let notify = self
                .waiters
                .entry(task_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone();
            let poll =
                Duration::from_millis(row.poll_interval_ms.unwrap_or(self.default_poll_ms).max(50));
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(poll) => {}
            }
        }
    }

    /// Drop expired tasks (ttl elapsed since creation).
    pub fn gc(&self) -> usize {
        let now = now_unix_ms();
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM tasks
             WHERE ttl_ms IS NOT NULL
               AND created_unix_ms + ttl_ms <= ?1",
            params![now],
        )
        .unwrap_or(0)
    }

    #[cfg(test)]
    fn status_of(&self, task_id: &str) -> Option<TaskStatus> {
        let conn = self.conn.lock();
        Self::read_row(&conn, task_id)
            .ok()
            .flatten()
            .map(|r| r.status)
    }
}

fn synthetic_terminal_payload(status: &TaskStatus, message: Option<&str>) -> Value {
    let text = message
        .map(str::to_string)
        .unwrap_or_else(|| format!("task ended with status {status:?}"));
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !matches!(status, TaskStatus::Completed),
    })
}

/// `(server, tool)` key for the run_task allowlist.
pub type TaskToolKey = (String, String);

/// Runs allowlisted upstream tools as SEP-1686 tasks (SQLite-backed).
pub struct TaskRunner {
    store: Arc<TaskStore>,
    pool: Arc<UpstreamPool>,
    sem: Arc<Semaphore>,
    /// Only these upstream tools may be invoked via `run_task`.
    allowed: Arc<std::sync::RwLock<HashSet<TaskToolKey>>>,
    db_path: PathBuf,
}

impl TaskRunner {
    pub fn new(
        pool: Arc<UpstreamPool>,
        db_path: PathBuf,
        allowlist: HashSet<TaskToolKey>,
        max_concurrent: usize,
        default_ttl_ms: u64,
        default_poll_ms: u64,
    ) -> Result<Self, TaskError> {
        let store = Arc::new(TaskStore::open(&db_path, default_ttl_ms, default_poll_ms)?);
        Ok(Self {
            store,
            pool,
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            allowed: Arc::new(std::sync::RwLock::new(allowlist)),
            db_path,
        })
    }

    pub fn store(&self) -> Arc<TaskStore> {
        self.store.clone()
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn replace_allowlist(&self, next: HashSet<TaskToolKey>) {
        if let Ok(mut g) = self.allowed.write() {
            *g = next;
        }
    }

    pub fn is_allowed(&self, server: &str, tool: &str) -> bool {
        self.allowed
            .read()
            .map(|g| g.contains(&(server.to_string(), tool.to_string())))
            .unwrap_or(false)
    }

    pub fn allowed_tools(&self) -> Vec<TaskToolKey> {
        self.allowed
            .read()
            .map(|g| {
                let mut v: Vec<_> = g.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// Synchronous path of `run_task` (no `task` augmentation).
    pub async fn run_now(
        &self,
        server: &str,
        tool: &str,
        args: Value,
    ) -> anyhow::Result<rmcp::model::CallToolResult> {
        if !self.is_allowed(server, tool) {
            anyhow::bail!(
                "tool '{server}.{tool}' is not task-capable; only upstream tools with \
                 execution.taskSupport (or sidecar task_support) may be invoked via run_task"
            );
        }
        self.pool.call(server, tool, args).await
    }

    /// Task-augmented path: enqueue and return `CreateTaskResult` immediately.
    pub fn enqueue(
        &self,
        owner: String,
        server: String,
        tool: String,
        args: Value,
    ) -> Result<CreateTaskResult, TaskError> {
        if !self.is_allowed(&server, &tool) {
            return Err(TaskError::NotFound(format!(
                "{server}.{tool} is not in the run_task allowlist"
            )));
        }
        let task_id = self.store.create(&owner, &server, &tool)?;
        let store = self.store.clone();
        let pool = self.pool.clone();
        let sem = self.sem.clone();
        let id = task_id.clone();
        let server_bg = server;
        let tool_bg = tool;

        tokio::spawn(async move {
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => {
                    store.fail(&id, "task semaphore closed", None);
                    return;
                }
            };
            if store.is_cancelled(&id) {
                return;
            }
            match pool.call(&server_bg, &tool_bg, args).await {
                Ok(result) => {
                    let is_error = result.is_error.unwrap_or(false);
                    let payload = serde_json::to_value(&result).unwrap_or(Value::Null);
                    if is_error {
                        store.fail(&id, "upstream tool reported isError=true", Some(payload));
                    } else {
                        store.complete(&id, payload, Some("Upstream tool completed.".into()));
                    }
                }
                Err(e) => {
                    store.fail(&id, format!("upstream call failed: {e:#}"), None);
                }
            }
        });

        let task = self
            .store
            .get(&task_id, &owner)
            .map(|g| g.task)
            .unwrap_or_else(|_| {
                Task::new(task_id.clone(), TaskStatus::Working, now_iso(), now_iso())
            });
        Ok(CreateTaskResult::new(task))
    }
}

/// Collect `(server, tool)` pairs that are task-capable from the live pool.
pub fn collect_task_allowlist(pool: &UpstreamPool) -> HashSet<TaskToolKey> {
    let mut out = HashSet::new();
    for (server, tools) in pool.all_resolved() {
        for t in tools {
            if t.task_support.is_task() {
                out.insert((server.clone(), t.name.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn tmp_store() -> (tempfile::TempDir, TaskStore) {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let store = TaskStore::open(&db, 60_000, 1_000).unwrap();
        (dir, store)
    }

    #[test]
    fn create_starts_working_and_lists_by_owner() {
        let (_dir, s) = tmp_store();
        let id = s.create("alice", "pres", "build").unwrap();
        assert_eq!(s.status_of(&id), Some(TaskStatus::Working));
        assert_eq!(s.list("alice").tasks.len(), 1);
        assert_eq!(s.list("bob").tasks.len(), 0);
        assert!(matches!(s.get(&id, "bob"), Err(TaskError::NotFound(_))));
        let g = s.get(&id, "alice").unwrap();
        assert_eq!(g.task.task_id, id);
        assert_eq!(g.task.poll_interval, Some(1_000));
        assert_eq!(g.task.ttl, Some(60_000));
    }

    #[test]
    fn complete_is_terminal_and_idempotent() {
        let (_dir, s) = tmp_store();
        let id = s.create("a", "s", "t").unwrap();
        s.complete(&id, serde_json::json!({"ok": true}), None);
        assert_eq!(s.status_of(&id), Some(TaskStatus::Completed));
        s.fail(&id, "late", None);
        assert_eq!(s.status_of(&id), Some(TaskStatus::Completed));
    }

    #[test]
    fn cancel_rejects_when_terminal() {
        let (_dir, s) = tmp_store();
        let id = s.create("a", "s", "t").unwrap();
        s.complete(&id, Value::Null, None);
        assert!(matches!(
            s.cancel(&id, "a"),
            Err(TaskError::AlreadyTerminal(_))
        ));
    }

    #[test]
    fn cancel_flips_working_to_cancelled() {
        let (_dir, s) = tmp_store();
        let id = s.create("a", "s", "t").unwrap();
        let res = s.cancel(&id, "a").unwrap();
        assert_eq!(res.task.status, TaskStatus::Cancelled);
        assert!(s.is_cancelled(&id));
    }

    #[tokio::test]
    async fn await_result_wakes_on_completion() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let s = Arc::new(TaskStore::open(&db, 60_000, 1_000).unwrap());
        let id = s.create("a", "s", "t").unwrap();
        let s2 = s.clone();
        let id2 = id.clone();
        let waiter = tokio::spawn(async move { s2.await_result(&id2, "a").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.complete(
            &id,
            serde_json::json!({"content": [], "isError": false}),
            None,
        );
        let payload = waiter.await.unwrap().unwrap();
        assert_eq!(payload["isError"], serde_json::json!(false));
    }

    #[test]
    fn gc_removes_expired_only() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let s = TaskStore::open(&db, 0, 1_000).unwrap(); // ttl 0
        let id = s.create("a", "s", "t").unwrap();
        // Force created_unix_ms into the past via direct update isn't needed —
        // ttl_ms=0 means created + 0 <= now immediately.
        assert_eq!(s.gc(), 1);
        assert!(s.status_of(&id).is_none());
    }

    #[test]
    fn survives_reopen() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let id = {
            let s = TaskStore::open(&db, 60_000, 1_000).unwrap();
            let id = s.create("a", "s", "t").unwrap();
            s.complete(&id, serde_json::json!({"ok": 1}), Some("done".into()));
            id
        };
        let s2 = TaskStore::open(&db, 60_000, 1_000).unwrap();
        let g = s2.get(&id, "a").unwrap();
        assert_eq!(g.task.status, TaskStatus::Completed);
    }

    #[test]
    fn open_db_fails_when_parent_is_file() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let err = TaskStore::open(blocker.join("tasks.db"), 60_000, 1_000);
        assert!(err.is_err());
    }

    #[test]
    fn open_db_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("nested").join("deep").join("tasks.db");
        let store = TaskStore::open(&db, 60_000, 1_000).unwrap();
        let id = store.create("a", "s", "t").unwrap();
        assert_eq!(store.status_of(&id), Some(TaskStatus::Working));
        assert!(db.exists());
    }

    #[test]
    fn is_cancelled_true_for_missing_and_after_db_only_cancel() {
        let (_dir, s) = tmp_store();
        assert!(s.is_cancelled("missing-id"));
        let id = s.create("a", "s", "t").unwrap();
        // Drop in-memory cancel flag so is_cancelled must read SQLite.
        s.cancel_flags.remove(&id);
        assert!(!s.is_cancelled(&id));
        s.cancel_flags.remove(&id);
        let _ = s.cancel(&id, "a").unwrap();
        s.cancel_flags.remove(&id);
        assert!(s.is_cancelled(&id));
    }

    #[test]
    fn finish_missing_task_is_noop_and_fail_sets_failed() {
        let (_dir, s) = tmp_store();
        s.complete("nope", Value::Null, None);
        let id = s.create("a", "s", "t").unwrap();
        s.fail(&id, "boom", Some(serde_json::json!({"isError": true})));
        assert_eq!(s.status_of(&id), Some(TaskStatus::Failed));
    }

    #[tokio::test]
    async fn await_result_uses_synthetic_payload_when_no_result_json() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let s = Arc::new(TaskStore::open(&db, 60_000, 50).unwrap());
        let id = s.create("a", "s", "t").unwrap();
        // Cancel writes status but no CallToolResult payload.
        s.cancel(&id, "a").unwrap();
        let payload = s.await_result(&id, "a").await.unwrap();
        assert_eq!(payload["isError"], serde_json::json!(true));
        assert!(payload["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cancelled"));
    }

    #[test]
    fn status_helpers_round_trip() {
        for (s, label) in [
            (TaskStatus::Working, "working"),
            (TaskStatus::InputRequired, "input_required"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status_to_str(&s), label);
            assert_eq!(status_from_str(label), s);
        }
        assert_eq!(status_from_str("unknown"), TaskStatus::Working);
        let synth = synthetic_terminal_payload(&TaskStatus::Completed, Some("ok"));
        assert_eq!(synth["isError"], serde_json::json!(false));
        let synth2 = synthetic_terminal_payload(&TaskStatus::Failed, None);
        assert_eq!(synth2["isError"], serde_json::json!(true));
    }

    fn runner_with_allowlist(keys: HashSet<TaskToolKey>) -> (tempfile::TempDir, TaskRunner) {
        let dir = tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let bus = vmcp_notify::Bus::new(16);
        let pool = Arc::new(UpstreamPool::empty_for_test(bus));
        pool.insert_synthetic_for_test(
            "mock",
            None,
            vec![vmcp_upstream::ResolvedTool {
                server: "mock".into(),
                name: "delay_read".into(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
                task_support: vmcp_registry::TaskSupportHint::Optional,
            }],
        );
        let runner = TaskRunner::new(pool, db, keys, 2, 60_000, 100).unwrap();
        (dir, runner)
    }

    #[test]
    fn runner_allowlist_helpers() {
        let mut keys = HashSet::new();
        keys.insert(("mock".into(), "delay_read".into()));
        let (_dir, runner) = runner_with_allowlist(keys);
        assert!(runner.is_allowed("mock", "delay_read"));
        assert!(!runner.is_allowed("mock", "other"));
        assert_eq!(
            runner.allowed_tools(),
            vec![("mock".into(), "delay_read".into())]
        );
        assert!(runner.db_path().ends_with("tasks.db"));
        runner.replace_allowlist(HashSet::new());
        assert!(!runner.is_allowed("mock", "delay_read"));
        assert!(runner.allowed_tools().is_empty());
        let _ = runner.store();
    }

    #[tokio::test]
    async fn runner_run_now_rejects_non_allowlisted() {
        let (_dir, runner) = runner_with_allowlist(HashSet::new());
        let err = runner
            .run_now("mock", "delay_read", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not task-capable"));
    }

    #[tokio::test]
    async fn runner_enqueue_rejects_then_fails_missing_upstream_client() {
        let mut keys = HashSet::new();
        keys.insert(("mock".into(), "delay_read".into()));
        let (_dir, runner) = runner_with_allowlist(keys);
        let err = runner.enqueue("anon".into(), "nope".into(), "x".into(), Value::Null);
        assert!(matches!(err, Err(TaskError::NotFound(_))));

        let created = runner
            .enqueue(
                "anon".into(),
                "mock".into(),
                "delay_read".into(),
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(created.task.status, TaskStatus::Working);
        // Synthetic pool has no live client → background call fails → Failed.
        let payload = runner
            .store()
            .await_result(&created.task.task_id, "anon")
            .await
            .unwrap();
        assert_eq!(payload["isError"], serde_json::json!(true));
    }

    #[test]
    fn collect_task_allowlist_from_pool() {
        let bus = vmcp_notify::Bus::new(8);
        let pool = UpstreamPool::empty_for_test(bus);
        pool.insert_synthetic_for_test(
            "p",
            None,
            vec![
                vmcp_upstream::ResolvedTool {
                    server: "p".into(),
                    name: "build".into(),
                    description: None,
                    input_schema: serde_json::json!({}),
                    read_only: false,
                    task_support: vmcp_registry::TaskSupportHint::Optional,
                },
                vmcp_upstream::ResolvedTool {
                    server: "p".into(),
                    name: "ping".into(),
                    description: None,
                    input_schema: serde_json::json!({}),
                    read_only: true,
                    task_support: vmcp_registry::TaskSupportHint::Forbidden,
                },
                vmcp_upstream::ResolvedTool {
                    server: "p".into(),
                    name: "sync".into(),
                    description: None,
                    input_schema: serde_json::json!({}),
                    read_only: false,
                    task_support: vmcp_registry::TaskSupportHint::Required,
                },
            ],
        );
        let allow = collect_task_allowlist(&pool);
        assert_eq!(allow.len(), 2);
        assert!(allow.contains(&("p".into(), "build".into())));
        assert!(allow.contains(&("p".into(), "sync".into())));
        assert!(!allow.contains(&("p".into(), "ping".into())));
    }
}
