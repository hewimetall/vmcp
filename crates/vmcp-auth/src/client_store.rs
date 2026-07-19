//! Durable DCR client store (RFC 7591) backed by SQLite.
//!
//! Same pattern as `vmcp-server::tasks::TaskStore`: WAL SQLite is the source of
//! truth across gateway restarts; [`AuthState`]'s DashMap is a hot cache hydrated
//! at boot. Cursor (and other MCP hosts) keep `client_id` locally after DCR —
//! without this store a restart made `/authorize` return `unknown client_id`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection};

use crate::types::{slugify_client_base, valid_client_display_name, ClientInfo};

/// Fresh-install schema. Pre-1.0 databases without `name` are not migrated —
/// operators must delete `auth.clients_db_path` and re-register clients.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id            TEXT PRIMARY KEY,
    redirect_uris_json   TEXT NOT NULL,
    client_name          TEXT,
    name                 TEXT NOT NULL,
    grant_types_json     TEXT NOT NULL,
    response_types_json  TEXT NOT NULL,
    scope                TEXT,
    issued_at_unix_ms    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_oauth_clients_issued
  ON oauth_clients(issued_at_unix_ms);
"#;

const NAME_INDEX_SCHEMA: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_oauth_clients_name
  ON oauth_clients(name) WHERE name IS NOT NULL AND name != '';
"#;

/// Error raised by the DCR client store.
#[derive(Debug, thiserror::Error)]
pub enum ClientStoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("name already taken: {0}")]
    NameTaken(String),
    #[error("client not found: {0}")]
    NotFound(String),
    #[error(
        "legacy oauth_clients schema at {path} is missing required `name` column; delete auth.clients_db_path and re-register clients"
    )]
    LegacySchemaMissingName { path: PathBuf },
}

fn open_db(path: &Path) -> Result<Connection, ClientStoreError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=30000;
         PRAGMA foreign_keys=ON;",
    )?;
    conn.execute_batch(SCHEMA)?;
    if !table_has_column(&conn, "name")? {
        return Err(ClientStoreError::LegacySchemaMissingName {
            path: path.to_path_buf(),
        });
    }
    conn.execute_batch(NAME_INDEX_SCHEMA)?;
    Ok(conn)
}

fn table_has_column(conn: &Connection, column: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare("PRAGMA table_info(oauth_clients)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dt_to_ms(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

/// SQLite-backed registry of OAuth dynamic clients.
pub struct ClientStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl ClientStore {
    /// Open (or create) the clients database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ClientStoreError> {
        let path = path.as_ref().to_path_buf();
        Ok(Self {
            conn: Mutex::new(open_db(&path)?),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert or replace a registered client (write-through from DCR).
    pub fn upsert(&self, info: &ClientInfo) -> Result<(), ClientStoreError> {
        let redirect_uris = serde_json::to_string(&info.redirect_uris)?;
        let grant_types = serde_json::to_string(&info.grant_types)?;
        let response_types = serde_json::to_string(&info.response_types)?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO oauth_clients (
                client_id, redirect_uris_json, client_name, name,
                grant_types_json, response_types_json, scope, issued_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(client_id) DO UPDATE SET
                redirect_uris_json = excluded.redirect_uris_json,
                client_name = excluded.client_name,
                name = excluded.name,
                grant_types_json = excluded.grant_types_json,
                response_types_json = excluded.response_types_json,
                scope = excluded.scope,
                issued_at_unix_ms = excluded.issued_at_unix_ms",
            params![
                info.client_id,
                redirect_uris,
                info.client_name,
                info.name,
                grant_types,
                response_types,
                info.scope,
                dt_to_ms(info.issued_at),
            ],
        )?;
        Ok(())
    }

    /// Rename a client. Enforces uniqueness and name validation.
    pub fn set_name(&self, client_id: &str, name: &str) -> Result<(), ClientStoreError> {
        if !valid_client_display_name(name) {
            return Err(ClientStoreError::InvalidName(name.into()));
        }
        let conn = self.conn.lock();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM oauth_clients WHERE client_id = ?1",
                params![client_id],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !exists {
            return Err(ClientStoreError::NotFound(client_id.into()));
        }
        let taken: bool = conn
            .query_row(
                "SELECT 1 FROM oauth_clients WHERE name = ?1 AND client_id != ?2",
                params![name, client_id],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if taken {
            return Err(ClientStoreError::NameTaken(name.into()));
        }
        let n = conn.execute(
            "UPDATE oauth_clients SET name = ?1 WHERE client_id = ?2",
            params![name, client_id],
        )?;
        if n == 0 {
            return Err(ClientStoreError::NotFound(client_id.into()));
        }
        Ok(())
    }

    /// Load one client by id.
    pub fn get(&self, client_id: &str) -> Result<Option<ClientInfo>, ClientStoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT client_id, redirect_uris_json, client_name, name,
                    grant_types_json, response_types_json, scope, issued_at_unix_ms
             FROM oauth_clients WHERE client_id = ?1",
        )?;
        let mut rows = stmt.query(params![client_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_info(row)?)),
            None => Ok(None),
        }
    }

    /// All registered clients (boot hydration / admin list fallback).
    pub fn list(&self) -> Result<Vec<ClientInfo>, ClientStoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT client_id, redirect_uris_json, client_name, name,
                    grant_types_json, response_types_json, scope, issued_at_unix_ms
             FROM oauth_clients
             ORDER BY issued_at_unix_ms ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row_to_info(row)?);
        }
        Ok(out)
    }
}

fn row_to_info(row: &rusqlite::Row<'_>) -> Result<ClientInfo, ClientStoreError> {
    // Columns: 0 client_id, 1 redirect_uris, 2 client_name, 3 name, 4 grant_types,
    // 5 response_types, 6 scope, 7 issued_at
    let client_id: String = row.get(0)?;
    let redirect_uris_json: String = row.get(1)?;
    let client_name: Option<String> = row.get(2)?;
    let name: Option<String> = row.get(3)?;
    let grant_types_json: String = row.get(4)?;
    let response_types_json: String = row.get(5)?;
    let scope: Option<String> = row.get(6)?;
    let issued_ms: i64 = row.get(7)?;
    let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
        // Defensive: schema requires NOT NULL; fall back to client_id slug.
        slugify_client_base(Some(&client_id))
    });
    Ok(ClientInfo {
        client_id,
        redirect_uris: serde_json::from_str(&redirect_uris_json)?,
        client_name,
        name,
        grant_types: serde_json::from_str(&grant_types_json)?,
        response_types: serde_json::from_str(&response_types_json)?,
        scope,
        issued_at: ms_to_dt(issued_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{next_unique_name, valid_client_display_name};
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn sample(id: &str, name: &str) -> ClientInfo {
        ClientInfo {
            client_id: id.into(),
            redirect_uris: vec!["http://127.0.0.1:9999/callback".into()],
            client_name: Some("Cursor".into()),
            name: name.into(),
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            scope: Some("mcp:use".into()),
            issued_at: Utc::now(),
        }
    }

    #[test]
    fn upsert_get_list_round_trip() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("clients.db");
        let store = ClientStore::open(&db).unwrap();
        let info = sample("vmcp-aaaa", "cursor");
        store.upsert(&info).unwrap();
        let got = store.get("vmcp-aaaa").unwrap().expect("present");
        assert_eq!(got.client_id, "vmcp-aaaa");
        assert_eq!(got.redirect_uris, info.redirect_uris);
        assert_eq!(got.client_name.as_deref(), Some("Cursor"));
        assert_eq!(got.name, "cursor");
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.get("missing").unwrap().is_none());
    }

    #[test]
    fn survives_reopen() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("clients.db");
        {
            let store = ClientStore::open(&db).unwrap();
            store.upsert(&sample("vmcp-persist", "cursor")).unwrap();
        }
        let store2 = ClientStore::open(&db).unwrap();
        let got = store2.get("vmcp-persist").unwrap().expect("reloaded");
        assert_eq!(got.client_name.as_deref(), Some("Cursor"));
        assert_eq!(got.name, "cursor");
        assert_eq!(got.scope.as_deref(), Some("mcp:use"));
    }

    #[test]
    fn open_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("nested").join("deep").join("clients.db");
        let store = ClientStore::open(&db).unwrap();
        store.upsert(&sample("vmcp-nested", "nested")).unwrap();
        assert!(db.exists());
        assert_eq!(store.path(), db.as_path());
    }

    #[test]
    fn open_fails_when_parent_is_file() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let err = ClientStore::open(blocker.join("clients.db"));
        assert!(err.is_err());
    }

    #[test]
    fn upsert_replaces_existing_row() {
        let dir = tempdir().unwrap();
        let store = ClientStore::open(dir.path().join("clients.db")).unwrap();
        let mut info = sample("vmcp-replace", "cursor");
        store.upsert(&info).unwrap();
        info.client_name = Some("Renamed".into());
        info.name = "renamed".into();
        info.redirect_uris.push("http://localhost/other".into());
        store.upsert(&info).unwrap();
        let got = store.get("vmcp-replace").unwrap().unwrap();
        assert_eq!(got.client_name.as_deref(), Some("Renamed"));
        assert_eq!(got.name, "renamed");
        assert_eq!(got.redirect_uris.len(), 2);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn set_name_enforces_unique_and_valid() {
        let dir = tempdir().unwrap();
        let store = ClientStore::open(dir.path().join("clients.db")).unwrap();
        store.upsert(&sample("vmcp-a", "alpha")).unwrap();
        store.upsert(&sample("vmcp-b", "beta")).unwrap();

        store.set_name("vmcp-a", "alpha-renamed").unwrap();
        assert_eq!(store.get("vmcp-a").unwrap().unwrap().name, "alpha-renamed");

        let taken = store.set_name("vmcp-a", "beta");
        assert!(matches!(taken, Err(ClientStoreError::NameTaken(_))));

        let bad = store.set_name("vmcp-a", "Bad Name!");
        assert!(matches!(bad, Err(ClientStoreError::InvalidName(_))));

        let missing = store.set_name("vmcp-missing", "ok");
        assert!(matches!(missing, Err(ClientStoreError::NotFound(_))));
    }

    #[test]
    fn legacy_schema_without_name_is_not_migrated() {
        // 1.0 drops ALTER/backfill: pre-1.0 DBs must be wiped by the operator.
        let dir = tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE oauth_clients (
                    client_id TEXT PRIMARY KEY,
                    redirect_uris_json TEXT NOT NULL,
                    client_name TEXT,
                    grant_types_json TEXT NOT NULL,
                    response_types_json TEXT NOT NULL,
                    scope TEXT,
                    issued_at_unix_ms INTEGER NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO oauth_clients VALUES ('vmcp-1', '[]', 'Cursor', '[]', '[]', NULL, 1)",
                [],
            )
            .unwrap();
        }
        let err = match ClientStore::open(&db) {
            Ok(_) => panic!("legacy DB should fail fast"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            ClientStoreError::LegacySchemaMissingName { .. }
        ));
        assert!(
            err.to_string().contains(
                "missing required `name` column; delete auth.clients_db_path and re-register clients"
            ),
            "error should tell operators how to recover: {err}"
        );
    }

    #[test]
    fn name_helpers() {
        assert!(valid_client_display_name("cursor"));
        assert!(valid_client_display_name("cursor-2"));
        assert!(!valid_client_display_name(""));
        assert!(!valid_client_display_name("Cursor"));
        assert!(!valid_client_display_name("has space"));
        assert_eq!(slugify_client_base(Some("Cursor IDE")), "cursor-ide");
        assert_eq!(slugify_client_base(None), "client");
        let mut used = HashSet::new();
        used.insert("cursor".into());
        assert_eq!(next_unique_name(&used, "cursor"), "cursor-2");
    }
}
