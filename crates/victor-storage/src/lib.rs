//! Trwały backend SQLite dla kontraktów V.I.C.T.O.R.
//!
//! Schemat przechowuje pola domenowe osobno, żeby indeksy i migracje nie
//! zależały od kształtu JSON-a. InMemoryContextStore pozostaje właściwy dla
//! szybkich testów; ten backend służy do restartów i atomowego wyniku narzędzia.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use victor_context::{
    ContextContent, ContextError, ContextItem, ContextItemId, ContextItemKind, ContextScope,
    ContextSource, ContextStore, MemoryTier, Origin, Retention, ScopeKind, Sensitivity,
};
use victor_skills::{
    SkillDecisionAudit, SkillDecisionAuditSink, SkillDecisionAuditStore, SkillId, SkillLifecycle,
};
use victor_tools::{AuditOutcome, ToolAuditEvent, ToolError, ToolResultStore};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("błąd SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("nieprawidłowe dane kontekstu: {0}")]
    Invalid(String),
    #[error("blokada SQLite jest zatruta")]
    Poisoned,
}

pub struct SqliteContextStore {
    connection: Mutex<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRegistrationRecord {
    pub plugin_id: String,
    pub plugin_version: String,
    pub manifest_json: String,
    pub manifest_hash: String,
    pub command: Vec<String>,
    pub executable_path: String,
    pub executable_hash: String,
    pub working_dir: String,
    pub environment: BTreeMap<String, String>,
    pub sandbox_required: bool,
    pub sandbox_executable: Option<String>,
    pub request_timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_restart_attempts: u32,
    pub launch_spec_hash: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLifecycleAuditRecord {
    pub plugin_id: String,
    pub action: String,
    pub status: String,
    pub actor: String,
    pub reason: String,
    pub at_unix_ms: u64,
}

pub const IDEMPOTENCY_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyRecord {
    pub key: String,
    pub principal: String,
    pub scope: String,
    pub request_hash: String,
    pub response_json: String,
    pub status: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

impl SqliteContextStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS context_items (
                 scope_type TEXT NOT NULL,
                 scope_id TEXT NOT NULL,
                 item_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 tier TEXT NOT NULL,
                 sensitivity TEXT NOT NULL,
                 content_kind TEXT NOT NULL,
                 content_text TEXT,
                 content_uri TEXT,
                 content_bytes INTEGER,
                 content_sha256 TEXT,
                 source_origin TEXT NOT NULL,
                 source_label TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 retention_kind TEXT NOT NULL,
                 retention_value TEXT,
                 PRIMARY KEY (scope_type, scope_id, item_id)
             );
             CREATE INDEX IF NOT EXISTS idx_context_scope
                 ON context_items(scope_type, scope_id);
             CREATE INDEX IF NOT EXISTS idx_context_tier
                 ON context_items(tier);
             CREATE INDEX IF NOT EXISTS idx_context_created
                 ON context_items(created_at);
             CREATE TABLE IF NOT EXISTS tool_audit (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL,
                 tool_id TEXT NOT NULL,
                 tool_version TEXT NOT NULL,
                 plan_hash TEXT NOT NULL,
                 started_at INTEGER NOT NULL,
                 finished_at INTEGER NOT NULL,
                 outcome TEXT NOT NULL,
                 summary TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_tool_audit_task
                 ON tool_audit(task_id, finished_at);
             CREATE INDEX IF NOT EXISTS idx_tool_audit_plan
                 ON tool_audit(plan_hash);
             CREATE TABLE IF NOT EXISTS skill_decisions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 skill_id TEXT NOT NULL,
                 version TEXT NOT NULL,
                 manifest_hash TEXT NOT NULL,
                 from_state TEXT NOT NULL,
                 to_state TEXT NOT NULL,
                 actor TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 at_unix_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_skill_decisions_skill
                 ON skill_decisions(skill_id, at_unix_ms);
             CREATE TABLE IF NOT EXISTS skill_registry (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 snapshot TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS plugin_registrations (
                 plugin_id TEXT PRIMARY KEY,
                 plugin_version TEXT NOT NULL,
                 manifest_json TEXT NOT NULL,
                 manifest_hash TEXT NOT NULL,
                 command_json TEXT NOT NULL,
                 executable_path TEXT NOT NULL,
                 executable_hash TEXT NOT NULL,
                 working_dir TEXT NOT NULL,
                 environment_json TEXT NOT NULL,
                 sandbox_required INTEGER NOT NULL CHECK (sandbox_required IN (0, 1)),
                 sandbox_executable TEXT,
                 request_timeout_ms INTEGER NOT NULL,
                 max_frame_bytes INTEGER NOT NULL,
                 max_restart_attempts INTEGER NOT NULL,
                 launch_spec_hash TEXT NOT NULL,
                 status TEXT NOT NULL CHECK (status IN ('active', 'disabled'))
             );
             CREATE TABLE IF NOT EXISTS plugin_lifecycle_audit (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 plugin_id TEXT NOT NULL,
                 action TEXT NOT NULL,
                 status TEXT NOT NULL,
                 actor TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 at_unix_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_plugin_lifecycle_audit
                 ON plugin_lifecycle_audit(plugin_id, at_unix_ms, id);
             CREATE TABLE IF NOT EXISTS transport_idempotency (
                 idempotency_key TEXT PRIMARY KEY,
                 principal TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 request_hash TEXT NOT NULL,
                 response_json TEXT NOT NULL,
                 status TEXT NOT NULL CHECK (status IN ('completed', 'failed')),
                 created_at_unix_ms INTEGER NOT NULL,
                 expires_at_unix_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_transport_idempotency_expiry
                 ON transport_idempotency(expires_at_unix_ms);
             CREATE TABLE IF NOT EXISTS transport_audit (
                 idempotency_key TEXT PRIMARY KEY,
                 principal TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 request_hash TEXT NOT NULL,
                 status TEXT NOT NULL,
                 at_unix_ms INTEGER NOT NULL
             );",
        )?;
        let columns = connection
            .prepare("PRAGMA table_info(plugin_registrations)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        let migrations = [
            ("plugin_version", "TEXT NOT NULL DEFAULT ''"),
            ("executable_path", "TEXT NOT NULL DEFAULT ''"),
            ("executable_hash", "TEXT NOT NULL DEFAULT ''"),
            ("working_dir", "TEXT NOT NULL DEFAULT ''"),
            ("environment_json", "TEXT NOT NULL DEFAULT '{}'"),
            ("sandbox_executable", "TEXT"),
            ("request_timeout_ms", "INTEGER NOT NULL DEFAULT 30000"),
            ("max_frame_bytes", "INTEGER NOT NULL DEFAULT 262144"),
            ("max_restart_attempts", "INTEGER NOT NULL DEFAULT 1"),
            ("launch_spec_hash", "TEXT NOT NULL DEFAULT ''"),
        ];
        for (column, definition) in migrations {
            if !columns.contains(column) {
                connection.execute(
                    &format!("ALTER TABLE plugin_registrations ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        Ok(())
    }

    pub fn load_idempotency(
        &self,
        key: &str,
        now_unix_ms: u64,
    ) -> Result<Option<IdempotencyRecord>, StorageError> {
        if key.is_empty() || key.len() > 256 {
            return Err(StorageError::Invalid(
                "klucz idempotency ma nieprawidłową długość".into(),
            ));
        }
        let connection = self.lock()?;
        connection.execute(
            "DELETE FROM transport_idempotency WHERE expires_at_unix_ms <= ?1",
            params![now_unix_ms],
        )?;
        connection
            .query_row(
                "SELECT idempotency_key, principal, scope, request_hash, response_json,
                        status, created_at_unix_ms, expires_at_unix_ms
                 FROM transport_idempotency WHERE idempotency_key = ?1",
                params![key],
                |row| {
                    Ok(IdempotencyRecord {
                        key: row.get(0)?,
                        principal: row.get(1)?,
                        scope: row.get(2)?,
                        request_hash: row.get(3)?,
                        response_json: row.get(4)?,
                        status: row.get(5)?,
                        created_at_unix_ms: row.get(6)?,
                        expires_at_unix_ms: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
            .and_then(|record| {
                let Some(record) = record else {
                    return Ok(None);
                };
                if !matches!(record.status.as_str(), "completed" | "failed")
                    || record.expires_at_unix_ms <= record.created_at_unix_ms
                    || record.response_json.len() > IDEMPOTENCY_MAX_RESPONSE_BYTES
                    || serde_json::from_str::<serde_json::Value>(&record.response_json).is_err()
                {
                    return Err(StorageError::Invalid(
                        "uszkodzony rekord idempotency".into(),
                    ));
                }
                Ok(Some(record))
            })
    }

    pub fn save_idempotency(&self, record: &IdempotencyRecord) -> Result<(), StorageError> {
        if record.key.is_empty()
            || record.key.len() > 256
            || record.principal.len() > 256
            || record.scope.len() > 256
            || record.request_hash.len() > 256
            || record.response_json.len() > IDEMPOTENCY_MAX_RESPONSE_BYTES
            || !matches!(record.status.as_str(), "completed" | "failed")
            || record.expires_at_unix_ms <= record.created_at_unix_ms
        {
            return Err(StorageError::Invalid(
                "nieprawidłowy rekord idempotency".into(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO transport_idempotency
             (idempotency_key, principal, scope, request_hash, response_json, status,
              created_at_unix_ms, expires_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(idempotency_key) DO NOTHING",
            params![
                record.key,
                record.principal,
                record.scope,
                record.request_hash,
                record.response_json,
                record.status,
                record.created_at_unix_ms,
                record.expires_at_unix_ms,
            ],
        )?;
        if inserted == 0 {
            return Err(StorageError::Invalid("konflikt klucza idempotency".into()));
        }
        transaction.execute(
            "INSERT INTO transport_audit
             (idempotency_key, principal, scope, request_hash, status, at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.key,
                record.principal,
                record.scope,
                record.request_hash,
                record.status,
                record.created_at_unix_ms,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn purge_idempotency(&self, now_unix_ms: u64) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        Ok(connection.execute(
            "DELETE FROM transport_idempotency WHERE expires_at_unix_ms <= ?1",
            params![now_unix_ms],
        )?)
    }

    pub fn transport_audit_count(&self) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        Ok(
            connection.query_row("SELECT COUNT(*) FROM transport_audit", [], |row| {
                row.get::<_, i64>(0)
            })? as usize,
        )
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Poisoned)
    }

    pub fn audit_count(&self) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        Ok(
            connection.query_row("SELECT COUNT(*) FROM tool_audit", [], |row| {
                row.get::<_, i64>(0)
            })? as usize,
        )
    }

    pub fn skill_decision_count(&self) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        Ok(
            connection.query_row("SELECT COUNT(*) FROM skill_decisions", [], |row| {
                row.get::<_, i64>(0)
            })? as usize,
        )
    }

    pub fn save_plugin_registration(
        &self,
        record: &PluginRegistrationRecord,
    ) -> Result<(), StorageError> {
        let command_json = serde_json::to_string(&record.command)
            .map_err(|error| StorageError::Invalid(error.to_string()))?;
        let environment_json = serde_json::to_string(&record.environment)
            .map_err(|error| StorageError::Invalid(error.to_string()))?;
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO plugin_registrations
             (plugin_id, plugin_version, manifest_json, manifest_hash, command_json,
              executable_path, executable_hash, working_dir, environment_json,
              sandbox_required, sandbox_executable, request_timeout_ms, max_frame_bytes,
              max_restart_attempts, launch_spec_hash, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(plugin_id) DO UPDATE SET
               plugin_version=excluded.plugin_version,
               manifest_json=excluded.manifest_json,
               manifest_hash=excluded.manifest_hash,
               command_json=excluded.command_json,
               executable_path=excluded.executable_path,
               executable_hash=excluded.executable_hash,
               working_dir=excluded.working_dir,
               environment_json=excluded.environment_json,
               sandbox_required=excluded.sandbox_required,
               sandbox_executable=excluded.sandbox_executable,
               request_timeout_ms=excluded.request_timeout_ms,
               max_frame_bytes=excluded.max_frame_bytes,
               max_restart_attempts=excluded.max_restart_attempts,
               launch_spec_hash=excluded.launch_spec_hash,
               status=excluded.status",
            params![
                record.plugin_id,
                record.plugin_version,
                record.manifest_json,
                record.manifest_hash,
                command_json,
                record.executable_path,
                record.executable_hash,
                record.working_dir,
                environment_json,
                i64::from(record.sandbox_required),
                record.sandbox_executable,
                record.request_timeout_ms,
                record.max_frame_bytes as u64,
                record.max_restart_attempts,
                record.launch_spec_hash,
                record.status,
            ],
        )?;
        Ok(())
    }

    pub fn set_plugin_status(&self, plugin_id: &str, status: &str) -> Result<(), StorageError> {
        if !matches!(status, "active" | "disabled") {
            return Err(StorageError::Invalid("nieznany status pluginu".into()));
        }
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE plugin_registrations SET status = ?1 WHERE plugin_id = ?2",
            params![status, plugin_id],
        )?;
        if changed == 0 {
            return Err(StorageError::Invalid(
                "plugin nie jest zarejestrowany".into(),
            ));
        }
        Ok(())
    }

    pub fn remove_plugin_registration(&self, plugin_id: &str) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "DELETE FROM plugin_registrations WHERE plugin_id = ?1",
            params![plugin_id],
        )?;
        if changed == 0 {
            return Err(StorageError::Invalid(
                "plugin nie jest zarejestrowany".into(),
            ));
        }
        Ok(())
    }

    pub fn record_plugin_lifecycle(
        &self,
        record: &PluginLifecycleAuditRecord,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO plugin_lifecycle_audit
             (plugin_id, action, status, actor, reason, at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.plugin_id,
                record.action,
                record.status,
                record.actor,
                record.reason,
                record.at_unix_ms,
            ],
        )?;
        Ok(())
    }

    pub fn plugin_lifecycle_history(
        &self,
        plugin_id: Option<&str>,
    ) -> Result<Vec<PluginLifecycleAuditRecord>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT plugin_id, action, status, actor, reason, at_unix_ms
             FROM plugin_lifecycle_audit
             WHERE (?1 IS NULL OR plugin_id = ?1)
             ORDER BY at_unix_ms ASC, id ASC",
        )?;
        let rows = statement.query_map(params![plugin_id], |row| {
            Ok(PluginLifecycleAuditRecord {
                plugin_id: row.get(0)?,
                action: row.get(1)?,
                status: row.get(2)?,
                actor: row.get(3)?,
                reason: row.get(4)?,
                at_unix_ms: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn plugin_registrations(&self) -> Result<Vec<PluginRegistrationRecord>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT plugin_id, plugin_version, manifest_json, manifest_hash, command_json,
                    executable_path, executable_hash, working_dir, environment_json,
                    sandbox_required, sandbox_executable, request_timeout_ms,
                    max_frame_bytes, max_restart_attempts, launch_spec_hash, status
             FROM plugin_registrations ORDER BY plugin_id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let command_json: String = row.get(4)?;
            let command = serde_json::from_str(&command_json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            let environment_json: String = row.get(8)?;
            let environment = serde_json::from_str(&environment_json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(PluginRegistrationRecord {
                plugin_id: row.get(0)?,
                plugin_version: row.get(1)?,
                manifest_json: row.get(2)?,
                manifest_hash: row.get(3)?,
                command,
                executable_path: row.get(5)?,
                executable_hash: row.get(6)?,
                working_dir: row.get(7)?,
                environment,
                sandbox_required: row.get::<_, i64>(9)? != 0,
                sandbox_executable: row.get(10)?,
                request_timeout_ms: row.get::<_, u64>(11)?,
                max_frame_bytes: row.get::<_, u64>(12)? as usize,
                max_restart_attempts: row.get::<_, u32>(13)?,
                launch_spec_hash: row.get(14)?,
                status: row.get(15)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    fn put_in_transaction(
        transaction: &Transaction<'_>,
        item: &ContextItem,
    ) -> Result<(), StorageError> {
        let (content_kind, content_text, content_uri, content_bytes, content_sha256) = match &item
            .content
        {
            ContextContent::Inline { text } => ("inline", Some(text.as_str()), None, None, None),
            ContextContent::Reference { uri, bytes, sha256 } => (
                "reference",
                None,
                Some(uri.as_str()),
                Some(i64::try_from(*bytes).map_err(|_| {
                    StorageError::Invalid("rozmiar referencji przekracza i64".into())
                })?),
                sha256.as_deref(),
            ),
        };
        let (retention_kind, retention_value) = retention_parts(item.retention);
        transaction.execute(
            "INSERT INTO context_items (
                scope_type, scope_id, item_id, kind, tier, sensitivity,
                content_kind, content_text, content_uri, content_bytes, content_sha256,
                source_origin, source_label, created_at, updated_at,
                retention_kind, retention_value
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(scope_type, scope_id, item_id) DO UPDATE SET
                kind=excluded.kind, tier=excluded.tier, sensitivity=excluded.sensitivity,
                content_kind=excluded.content_kind, content_text=excluded.content_text,
                content_uri=excluded.content_uri, content_bytes=excluded.content_bytes,
                content_sha256=excluded.content_sha256, source_origin=excluded.source_origin,
                source_label=excluded.source_label, created_at=excluded.created_at,
                updated_at=excluded.updated_at, retention_kind=excluded.retention_kind,
                retention_value=excluded.retention_value",
            params![
                item.scope.kind().as_str(),
                item.scope.id(),
                item.id.as_str(),
                item.kind.as_str(),
                item.tier.as_str(),
                item.sensitivity.as_str(),
                content_kind,
                content_text,
                content_uri,
                content_bytes,
                content_sha256,
                item.source.origin.as_str(),
                item.source.label,
                i64::try_from(item.created_at_unix_ms)
                    .map_err(|_| StorageError::Invalid("created_at przekracza i64".into()))?,
                i64::try_from(item.updated_at_unix_ms)
                    .map_err(|_| StorageError::Invalid("updated_at przekracza i64".into()))?,
                retention_kind,
                retention_value,
            ],
        )?;
        Ok(())
    }

    fn read_item(row: &rusqlite::Row<'_>) -> Result<ContextItem, StorageError> {
        let scope = parse_scope(&row.get::<_, String>(0)?, row.get(1)?)?;
        let item_id = ContextItemId::parse(row.get::<_, String>(2)?.as_str())
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        let kind = parse_item_kind(&row.get::<_, String>(3)?)?;
        let tier = parse_tier(&row.get::<_, String>(4)?)?;
        let sensitivity = parse_sensitivity(&row.get::<_, String>(5)?)?;
        let content = match row.get::<_, String>(6)?.as_str() {
            "inline" => ContextContent::inline(
                row.get::<_, Option<String>>(7)?
                    .ok_or_else(|| StorageError::Invalid("inline bez tekstu".into()))?,
            ),
            "reference" => ContextContent::Reference {
                uri: row
                    .get::<_, Option<String>>(8)?
                    .ok_or_else(|| StorageError::Invalid("reference bez URI".into()))?,
                bytes: u64::try_from(
                    row.get::<_, Option<i64>>(9)?
                        .ok_or_else(|| StorageError::Invalid("reference bez rozmiaru".into()))?,
                )
                .map_err(|_| StorageError::Invalid("ujemny rozmiar referencji".into()))?,
                sha256: row.get(10)?,
            },
            other => {
                return Err(StorageError::Invalid(format!(
                    "nieznany content_kind {other}"
                )))
            }
        };
        let source = ContextSource::new(
            parse_origin(&row.get::<_, String>(11)?)?,
            row.get::<_, String>(12)?,
        );
        let created = u64::try_from(row.get::<_, i64>(13)?)
            .map_err(|_| StorageError::Invalid("ujemny created_at".into()))?;
        let updated = u64::try_from(row.get::<_, i64>(14)?)
            .map_err(|_| StorageError::Invalid("ujemny updated_at".into()))?;
        let retention = parse_retention(&row.get::<_, String>(15)?, row.get(16)?)?;
        let item = ContextItem::new(item_id, scope, kind, tier, content, source, created)
            .map_err(|e| StorageError::Invalid(e.to_string()))?
            .with_sensitivity(sensitivity)
            .with_retention(retention)
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        Ok(ContextItem {
            updated_at_unix_ms: updated,
            ..item
        })
    }

    fn read_item_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextItem> {
        Self::read_item(row).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }
}

impl ContextStore for SqliteContextStore {
    fn put(&self, item: ContextItem) -> Result<(), ContextError> {
        let connection = self.lock().map_err(to_context_error)?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(to_context_error)?;
        Self::put_in_transaction(&transaction, &item).map_err(to_context_error)?;
        transaction.commit().map_err(to_context_error)
    }

    fn get(
        &self,
        id: &ContextItemId,
        scope: &ContextScope,
    ) -> Result<Option<ContextItem>, ContextError> {
        let connection = self.lock().map_err(to_context_error)?;
        let mut statement = connection
            .prepare(
                "SELECT scope_type, scope_id, item_id, kind, tier, sensitivity,
                 content_kind, content_text, content_uri, content_bytes, content_sha256,
                 source_origin, source_label, created_at, updated_at,
                 retention_kind, retention_value
                 FROM context_items WHERE scope_type = ?1 AND scope_id = ?2 AND item_id = ?3",
            )
            .map_err(to_context_error)?;
        statement
            .query_row(
                params![scope.kind().as_str(), scope.id(), id.as_str()],
                Self::read_item_sql,
            )
            .optional()
            .map_err(to_context_error)
    }

    fn delete(&self, id: &ContextItemId, scope: &ContextScope) -> Result<bool, ContextError> {
        let connection = self.lock().map_err(to_context_error)?;
        let changed = connection
            .execute(
                "DELETE FROM context_items WHERE scope_type = ?1 AND scope_id = ?2 AND item_id = ?3",
                params![scope.kind().as_str(), scope.id(), id.as_str()],
            )
            .map_err(to_context_error)?;
        Ok(changed == 1)
    }

    fn list(&self, scope: &ContextScope) -> Result<Vec<ContextItem>, ContextError> {
        let connection = self.lock().map_err(to_context_error)?;
        let mut statement = connection
            .prepare(
                "SELECT scope_type, scope_id, item_id, kind, tier, sensitivity,
                 content_kind, content_text, content_uri, content_bytes, content_sha256,
                 source_origin, source_label, created_at, updated_at,
                 retention_kind, retention_value
                 FROM context_items WHERE scope_type = ?1 AND scope_id = ?2
                 ORDER BY created_at DESC, item_id ASC",
            )
            .map_err(to_context_error)?;
        let result = statement
            .query_map(
                params![scope.kind().as_str(), scope.id()],
                Self::read_item_sql,
            )
            .map_err(to_context_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(to_context_error);
        result
    }

    fn purge_expired(&self, now: u64) -> Result<usize, ContextError> {
        let connection = self.lock().map_err(to_context_error)?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(to_context_error)?;
        let mut statement = transaction
            .prepare(
                "SELECT scope_type, scope_id, item_id, kind, tier, sensitivity,
                 content_kind, content_text, content_uri, content_bytes, content_sha256,
                 source_origin, source_label, created_at, updated_at,
                 retention_kind, retention_value FROM context_items",
            )
            .map_err(to_context_error)?;
        let items = statement
            .query_map([], Self::read_item_sql)
            .map_err(to_context_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(to_context_error)?;
        drop(statement);
        let mut removed = 0;
        for item in items {
            if item.is_expired(now) {
                transaction
                    .execute(
                        "DELETE FROM context_items WHERE scope_type = ?1 AND scope_id = ?2 AND item_id = ?3",
                        params![item.scope.kind().as_str(), item.scope.id(), item.id.as_str()],
                    )
                    .map_err(to_context_error)?;
                removed += 1;
            }
        }
        transaction.commit().map_err(to_context_error)?;
        Ok(removed)
    }
}

impl ToolResultStore for SqliteContextStore {
    fn commit_tool_result(
        &self,
        item: ContextItem,
        event: ToolAuditEvent,
    ) -> Result<(), ToolError> {
        let connection = self.lock().map_err(|e| ToolError::Storage(e.to_string()))?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|e| ToolError::Storage(e.to_string()))?;
        Self::put_in_transaction(&transaction, &item)
            .map_err(|e| ToolError::Storage(e.to_string()))?;
        transaction
            .execute(
                "INSERT INTO tool_audit
             (task_id, tool_id, tool_version, plan_hash, started_at, finished_at, outcome, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.task_id,
                    event.tool.as_str(),
                    event.tool_version,
                    event.plan_hash,
                    i64::try_from(event.started_at_unix_ms)
                        .map_err(|_| ToolError::Storage("started_at przekracza i64".into()))?,
                    i64::try_from(event.finished_at_unix_ms)
                        .map_err(|_| ToolError::Storage("finished_at przekracza i64".into()))?,
                    audit_outcome(event.outcome),
                    event.summary,
                ],
            )
            .map_err(|e| ToolError::Storage(e.to_string()))?;
        transaction
            .commit()
            .map_err(|e| ToolError::Storage(e.to_string()))
    }
}

impl victor_tools::AuditSink for SqliteContextStore {
    fn record(&self, event: ToolAuditEvent) -> Result<(), ToolError> {
        let connection = self.lock().map_err(|e| ToolError::Storage(e.to_string()))?;
        connection
            .execute(
                "INSERT INTO tool_audit
             (task_id, tool_id, tool_version, plan_hash, started_at, finished_at, outcome, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.task_id,
                    event.tool.as_str(),
                    event.tool_version,
                    event.plan_hash,
                    i64::try_from(event.started_at_unix_ms)
                        .map_err(|_| ToolError::Storage("started_at przekracza i64".into()))?,
                    i64::try_from(event.finished_at_unix_ms)
                        .map_err(|_| ToolError::Storage("finished_at przekracza i64".into()))?,
                    audit_outcome(event.outcome),
                    event.summary,
                ],
            )
            .map_err(|e| ToolError::Storage(e.to_string()))?;
        Ok(())
    }
}

impl SkillDecisionAuditSink for SqliteContextStore {
    fn record_skill_decision(&self, event: SkillDecisionAudit) -> Result<(), String> {
        let connection = self.lock().map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO skill_decisions
                 (skill_id, version, manifest_hash, from_state, to_state, actor, reason, at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.skill_id.as_str(),
                    event.version,
                    event.manifest_hash,
                    lifecycle_name(event.from),
                    lifecycle_name(event.to),
                    event.actor,
                    event.reason,
                    i64::try_from(event.at_unix_ms)
                        .map_err(|_| "at_unix_ms przekracza i64".to_string())?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

impl SkillDecisionAuditStore for SqliteContextStore {
    fn list_skill_decisions(
        &self,
        skill_id: Option<&SkillId>,
    ) -> Result<Vec<SkillDecisionAudit>, String> {
        let connection = self.lock().map_err(|error| error.to_string())?;
        let mut statement = connection
            .prepare(
                "SELECT skill_id, version, manifest_hash, from_state, to_state,
                        actor, reason, at_unix_ms
                 FROM skill_decisions
                 WHERE (?1 IS NULL OR skill_id = ?1)
                 ORDER BY at_unix_ms ASC, id ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![skill_id.map(SkillId::as_str)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| {
            let (id, version, hash, from, to, actor, reason, at) =
                row.map_err(|error| error.to_string())?;
            Ok(SkillDecisionAudit {
                skill_id: SkillId::parse(&id).map_err(|error| error.to_string())?,
                version,
                manifest_hash: hash,
                from: parse_lifecycle(&from)?,
                to: parse_lifecycle(&to)?,
                actor,
                reason,
                at_unix_ms: u64::try_from(at).map_err(|_| "ujemny at_unix_ms".to_string())?,
            })
        })
        .collect()
    }

    fn load_skill_registry(&self) -> Result<Option<String>, String> {
        let connection = self.lock().map_err(|error| error.to_string())?;
        connection
            .query_row(
                "SELECT snapshot FROM skill_registry WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    fn save_skill_registry(&self, snapshot: &str) -> Result<(), String> {
        let connection = self.lock().map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO skill_registry (id, snapshot) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET snapshot = excluded.snapshot",
                params![snapshot],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn lifecycle_name(state: SkillLifecycle) -> &'static str {
    match state {
        SkillLifecycle::Draft => "draft",
        SkillLifecycle::Validated => "validated",
        SkillLifecycle::Approved => "approved",
        SkillLifecycle::Active => "active",
        SkillLifecycle::Disabled => "disabled",
    }
}

fn parse_lifecycle(value: &str) -> Result<SkillLifecycle, String> {
    match value {
        "draft" => Ok(SkillLifecycle::Draft),
        "validated" => Ok(SkillLifecycle::Validated),
        "approved" => Ok(SkillLifecycle::Approved),
        "active" => Ok(SkillLifecycle::Active),
        "disabled" => Ok(SkillLifecycle::Disabled),
        other => Err(format!("nieznany lifecycle {other}")),
    }
}

fn retention_parts(retention: Retention) -> (&'static str, Option<String>) {
    match retention {
        Retention::KeepForever => ("forever", None),
        Retention::Until(value) => ("until", Some(value.to_string())),
        Retention::TtlFromCreation(value) => ("ttl", Some(value.to_string())),
    }
}

fn parse_retention(kind: &str, value: Option<String>) -> Result<Retention, StorageError> {
    let value = || {
        value
            .clone()
            .ok_or_else(|| StorageError::Invalid("retencja bez wartości".into()))
    };
    match kind {
        "forever" => Ok(Retention::KeepForever),
        "until" => {
            Ok(Retention::Until(value()?.parse().map_err(|_| {
                StorageError::Invalid("until nie jest u64".into())
            })?))
        }
        "ttl" => {
            Ok(Retention::TtlFromCreation(value()?.parse().map_err(
                |_| StorageError::Invalid("ttl nie jest u64".into()),
            )?))
        }
        other => Err(StorageError::Invalid(format!(
            "nieznany retention_kind {other}"
        ))),
    }
}

fn parse_scope(kind: &str, id: String) -> Result<ContextScope, StorageError> {
    let kind = match kind {
        "user" => ScopeKind::User,
        "project" => ScopeKind::Project,
        "conversation" => ScopeKind::Conversation,
        "task" => ScopeKind::Task,
        other => {
            return Err(StorageError::Invalid(format!(
                "nieznany scope_type {other}"
            )))
        }
    };
    ContextScope::new(kind, id).map_err(|e| StorageError::Invalid(e.to_string()))
}

fn parse_item_kind(value: &str) -> Result<ContextItemKind, StorageError> {
    match value {
        "message" => Ok(ContextItemKind::Message),
        "note" => Ok(ContextItemKind::Note),
        "decision" => Ok(ContextItemKind::Decision),
        "preference" => Ok(ContextItemKind::Preference),
        "observation" => Ok(ContextItemKind::Observation),
        "evidence" => Ok(ContextItemKind::Evidence),
        "file_reference" => Ok(ContextItemKind::FileReference),
        "tool_result" => Ok(ContextItemKind::ToolResult),
        other => Err(StorageError::Invalid(format!("nieznany kind {other}"))),
    }
}

fn parse_tier(value: &str) -> Result<MemoryTier, StorageError> {
    match value {
        "working" => Ok(MemoryTier::Working),
        "durable" => Ok(MemoryTier::Durable),
        "evidence" => Ok(MemoryTier::Evidence),
        other => Err(StorageError::Invalid(format!("nieznany tier {other}"))),
    }
}

fn parse_sensitivity(value: &str) -> Result<Sensitivity, StorageError> {
    match value {
        "public" => Ok(Sensitivity::Public),
        "internal" => Ok(Sensitivity::Internal),
        "sensitive" => Ok(Sensitivity::Sensitive),
        "secret" => Ok(Sensitivity::Secret),
        other => Err(StorageError::Invalid(format!(
            "nieznana sensitivity {other}"
        ))),
    }
}

fn parse_origin(value: &str) -> Result<Origin, StorageError> {
    match value {
        "user" => Ok(Origin::User),
        "assistant" => Ok(Origin::Assistant),
        "tool" => Ok(Origin::Tool),
        "model" => Ok(Origin::Model),
        "imported" => Ok(Origin::Imported),
        other => Err(StorageError::Invalid(format!("nieznany origin {other}"))),
    }
}

fn audit_outcome(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Succeeded => "succeeded",
        AuditOutcome::Denied => "denied",
        AuditOutcome::Failed => "failed",
    }
}

fn to_context_error(error: impl std::fmt::Display) -> ContextError {
    ContextError::Store(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use victor_context::{ContextContent, ContextItemKind};

    fn item(id: &str, created: u64) -> ContextItem {
        ContextItem::new(
            ContextItemId::parse(id).unwrap(),
            ContextScope::project("victor").unwrap(),
            ContextItemKind::Evidence,
            MemoryTier::Evidence,
            ContextContent::Reference {
                uri: format!("https://example.com/{id}"),
                bytes: 12,
                sha256: Some("abc".into()),
            },
            ContextSource::new(Origin::Tool, "web.fetch"),
            created,
        )
        .unwrap()
        .with_sensitivity(Sensitivity::Sensitive)
        .with_retention(Retention::TtlFromCreation(10))
        .unwrap()
    }

    #[test]
    fn restart_roundtrip_preserves_scope_fields_and_provenance() {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let store = SqliteContextStore::open(file.path()).unwrap();
            store.put(item("e1", 100)).unwrap();
        }
        let store = SqliteContextStore::open(file.path()).unwrap();
        let loaded = store
            .get(
                &ContextItemId::parse("e1").unwrap(),
                &ContextScope::project("victor").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(loaded.tier, MemoryTier::Evidence);
        assert_eq!(loaded.source.label, "web.fetch");
        assert_eq!(loaded.content.size_bytes(), 12);
        assert_eq!(loaded.expires_at(), Some(110));
    }

    #[test]
    fn purge_is_deterministic_and_keeps_unexpired_rows() {
        let store = SqliteContextStore::in_memory().unwrap();
        store.put(item("old", 100)).unwrap();
        store.put(item("new", 200)).unwrap();
        assert_eq!(store.purge_expired(110).unwrap(), 1);
        assert!(store
            .get(
                &ContextItemId::parse("old").unwrap(),
                &ContextScope::project("victor").unwrap()
            )
            .unwrap()
            .is_none());
        assert!(store
            .get(
                &ContextItemId::parse("new").unwrap(),
                &ContextScope::project("victor").unwrap()
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn idempotency_survives_restart_and_audits_atomically() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let record = IdempotencyRecord {
            key: "restart-key".into(),
            principal: "principal-hash".into(),
            scope: "local".into(),
            request_hash: "request-hash".into(),
            response_json: r#"{"ok":true,"result":{"value":1},"error":null}"#.into(),
            status: "completed".into(),
            created_at_unix_ms: 100,
            expires_at_unix_ms: 200,
        };
        {
            let store = SqliteContextStore::open(file.path()).unwrap();
            store.save_idempotency(&record).unwrap();
            assert_eq!(store.transport_audit_count().unwrap(), 1);
        }
        let restarted = SqliteContextStore::open(file.path()).unwrap();
        assert_eq!(
            restarted.load_idempotency("restart-key", 150).unwrap(),
            Some(record)
        );
        assert_eq!(restarted.transport_audit_count().unwrap(), 1);
    }

    #[test]
    fn idempotency_ttl_purge_and_corrupt_records_fail_closed() {
        let store = SqliteContextStore::in_memory().unwrap();
        let record = IdempotencyRecord {
            key: "ttl-key".into(),
            principal: "p".into(),
            scope: "s".into(),
            request_hash: "h".into(),
            response_json: r#"{"ok":false,"result":null,"error":"denied"}"#.into(),
            status: "failed".into(),
            created_at_unix_ms: 1,
            expires_at_unix_ms: 2,
        };
        store.save_idempotency(&record).unwrap();
        assert!(store.load_idempotency("ttl-key", 2).unwrap().is_none());
        assert_eq!(store.purge_idempotency(2).unwrap(), 0);

        store
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO transport_idempotency
                 (idempotency_key, principal, scope, request_hash, response_json, status,
                  created_at_unix_ms, expires_at_unix_ms)
                 VALUES ('bad', 'p', 's', 'h', '{not-json}', 'completed', 1, 100)",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.load_idempotency("bad", 2),
            Err(StorageError::Invalid(_))
        ));
    }

    #[test]
    fn skill_decision_audit_is_persistent() {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let store = SqliteContextStore::open(file.path()).unwrap();
            store
                .record_skill_decision(SkillDecisionAudit {
                    skill_id: victor_skills::SkillId::parse("demo.skill").unwrap(),
                    version: "1.0.0".into(),
                    manifest_hash: "sha256:test".into(),
                    from: SkillLifecycle::Validated,
                    to: SkillLifecycle::Approved,
                    actor: "user".into(),
                    reason: "review accepted".into(),
                    at_unix_ms: 100,
                })
                .unwrap();
            assert_eq!(store.skill_decision_count().unwrap(), 1);
        }
        let restarted = SqliteContextStore::open(file.path()).unwrap();
        assert_eq!(restarted.skill_decision_count().unwrap(), 1);
    }

    #[test]
    fn plugin_registration_and_disabled_status_survive_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let record = PluginRegistrationRecord {
            plugin_id: "plugin_sysinfo".into(),
            plugin_version: "1.0.0".into(),
            manifest_json: "{\"manifest\":true}".into(),
            manifest_hash: "sha256:test".into(),
            command: vec!["plugin_sysinfo".into(), "normal".into()],
            executable_path: "plugin_sysinfo".into(),
            executable_hash: "sha256:executable".into(),
            working_dir: "/tmp".into(),
            environment: BTreeMap::new(),
            sandbox_required: false,
            sandbox_executable: None,
            request_timeout_ms: 30_000,
            max_frame_bytes: 256 * 1024,
            max_restart_attempts: 1,
            launch_spec_hash: "sha256:launch".into(),
            status: "active".into(),
        };
        {
            let store = SqliteContextStore::open(file.path()).unwrap();
            store.save_plugin_registration(&record).unwrap();
            assert_eq!(store.plugin_registrations().unwrap(), vec![record.clone()]);
            store
                .set_plugin_status("plugin_sysinfo", "disabled")
                .unwrap();
        }
        let restarted = SqliteContextStore::open(file.path()).unwrap();
        let registrations = restarted.plugin_registrations().unwrap();
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].plugin_id, "plugin_sysinfo");
        assert_eq!(registrations[0].status, "disabled");
    }

    #[test]
    fn tool_result_and_audit_share_one_commit() {
        let store = SqliteContextStore::in_memory().unwrap();
        let event = ToolAuditEvent {
            task_id: "task-1".into(),
            tool: victor_domain::ToolId::parse("web.fetch").unwrap(),
            tool_version: "1.0.0".into(),
            plan_hash: "hash".into(),
            started_at_unix_ms: 100,
            finished_at_unix_ms: 101,
            outcome: AuditOutcome::Succeeded,
            summary: "ok".into(),
        };
        store
            .commit_tool_result(item("result", 100), event)
            .unwrap();
        assert_eq!(store.audit_count().unwrap(), 1);
        assert!(store
            .get(
                &ContextItemId::parse("result").unwrap(),
                &ContextScope::project("victor").unwrap()
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn idempotency_store_persists_conflicts_and_ttl() {
        let database = tempfile::NamedTempFile::new().unwrap();
        let record = IdempotencyRecord {
            key: "voice-1".into(),
            principal: "principal-a".into(),
            scope: "local".into(),
            request_hash: "request-a".into(),
            response_json: r#"{"ok":true,"result":{"status":"executed"},"error":null}"#.into(),
            status: "completed".into(),
            created_at_unix_ms: 10,
            expires_at_unix_ms: 20,
        };
        let store = SqliteContextStore::open(database.path()).unwrap();
        store.save_idempotency(&record).unwrap();
        assert_eq!(
            store.load_idempotency("voice-1", 19).unwrap(),
            Some(record.clone())
        );
        assert!(store.save_idempotency(&record).is_err());
        let restarted = SqliteContextStore::open(database.path()).unwrap();
        assert_eq!(restarted.load_idempotency("voice-1", 20).unwrap(), None);
        assert_eq!(restarted.purge_idempotency(20).unwrap(), 0);
    }
}
