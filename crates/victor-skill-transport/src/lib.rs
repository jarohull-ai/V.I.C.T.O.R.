//! Lokalny transport API skills przez Unix domain socket.

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;
use victor_skill_api::{SkillApi, SkillApiError};
use victor_skill_forge::ForgeRequest;
use victor_skills::{SkillId, SkillManifest};

pub const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum SkillOperation {
    List,
    Review {
        skill_id: SkillId,
    },
    CreateDraft {
        request: ForgeRequest,
    },
    Update {
        manifest: SkillManifest,
    },
    ApproveActivate {
        skill_id: SkillId,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    Disable {
        skill_id: SkillId,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    Rollback {
        skill_id: SkillId,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    AuditHistory {
        skill_id: Option<SkillId>,
    },
    Snapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRequest {
    pub token: String,
    pub scope: String,
    pub idempotency_key: Option<String>,
    pub operation: SkillOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResponse {
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport nie jest skonfigurowany: {0}")]
    InvalidConfig(String),
    #[error("błąd socketu: {0}")]
    Io(#[from] std::io::Error),
    #[error("błąd JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("żądanie przekracza limit {0} bajtów")]
    RequestTooLarge(usize),
    #[error("przekroczono timeout żądania")]
    Timeout,
}

struct ServerState {
    api: SkillApi,
    token: String,
    scope: String,
    max_request_bytes: usize,
    idempotent: HashMap<String, SkillResponse>,
}

pub struct UnixSkillServer {
    socket_path: PathBuf,
    state: Arc<Mutex<ServerState>>,
    request_timeout: Duration,
}

impl UnixSkillServer {
    pub fn new(
        socket_path: impl Into<PathBuf>,
        token: impl Into<String>,
        scope: impl Into<String>,
        api: SkillApi,
    ) -> Result<Self, TransportError> {
        let token = token.into();
        let scope = scope.into();
        if token.len() < 16 || scope.trim().is_empty() {
            return Err(TransportError::InvalidConfig(
                "token musi mieć co najmniej 16 znaków, scope nie może być pusty".into(),
            ));
        }
        Ok(Self {
            socket_path: socket_path.into(),
            state: Arc::new(Mutex::new(ServerState {
                api,
                token,
                scope,
                max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
                idempotent: HashMap::new(),
            })),
            request_timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_limits(
        mut self,
        max_request_bytes: usize,
        request_timeout: Duration,
    ) -> Result<Self, TransportError> {
        if max_request_bytes == 0 || request_timeout.is_zero() {
            return Err(TransportError::InvalidConfig(
                "limity muszą być większe od zera".into(),
            ));
        }
        self.state
            .try_lock()
            .map_err(|_| TransportError::InvalidConfig("blokada stanu".into()))?
            .max_request_bytes = max_request_bytes;
        self.request_timeout = request_timeout;
        Ok(self)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), TransportError> {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.socket_path) {
            if !metadata.file_type().is_socket() {
                return Err(TransportError::InvalidConfig(
                    "ścieżka socketu wskazuje istniejący plik nie będący Unix socketem".into(),
                ));
            }
            std::fs::remove_file(&self.socket_path)?;
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    let _ = std::fs::remove_file(&self.socket_path);
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let state = self.state.clone();
                    let request_timeout = self.request_timeout;
                    tokio::spawn(async move { let _ = handle_connection(stream, state, request_timeout).await; });
                }
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    state: Arc<Mutex<ServerState>>,
    request_timeout: Duration,
) -> Result<(), TransportError> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let max_request_bytes = state.lock().await.max_request_bytes;
        let mut bytes = Vec::new();
        let read = timeout(request_timeout, reader.read_until(b'\n', &mut bytes))
            .await
            .map_err(|_| TransportError::Timeout)??;
        if read == 0 {
            return Ok(());
        }
        if bytes.len() > max_request_bytes {
            write_response(
                &mut write_half,
                SkillResponse {
                    ok: false,
                    result: None,
                    error: Some(TransportError::RequestTooLarge(max_request_bytes).to_string()),
                },
            )
            .await?;
            continue;
        }
        let request: SkillRequest = match serde_json::from_slice(&bytes) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut write_half,
                    SkillResponse {
                        ok: false,
                        result: None,
                        error: Some(error.to_string()),
                    },
                )
                .await?;
                continue;
            }
        };
        write_response(&mut write_half, dispatch(request, &state).await).await?;
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: SkillResponse,
) -> Result<(), TransportError> {
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn dispatch(request: SkillRequest, state: &Arc<Mutex<ServerState>>) -> SkillResponse {
    let key = request.idempotency_key.clone();
    let mut guard = state.lock().await;
    if request.token != guard.token {
        return failure("nieautoryzowany klient");
    }
    if request.scope != guard.scope {
        return failure("odmowa scope klienta");
    }
    if let Some(key) = key.as_ref() {
        if let Some(response) = guard.idempotent.get(key) {
            return response.clone();
        }
    }
    let result = match request.operation {
        SkillOperation::List => serde_json::to_value(guard.api.list())
            .map_err(|error| SkillApiError::Audit(error.to_string())),
        SkillOperation::Review { skill_id } => guard.api.review(&skill_id).and_then(to_value),
        SkillOperation::CreateDraft { request } => {
            guard.api.create_draft(request).and_then(to_value)
        }
        SkillOperation::Update { manifest } => {
            guard.api.update_manifest(manifest).and_then(to_value)
        }
        SkillOperation::ApproveActivate {
            skill_id,
            actor,
            reason,
            at_unix_ms,
        } => guard
            .api
            .validate_and_approve_activate(&skill_id, &actor, at_unix_ms, &reason)
            .and_then(to_value),
        SkillOperation::Disable {
            skill_id,
            actor,
            reason,
            at_unix_ms,
        } => guard
            .api
            .disable(&skill_id, &actor, at_unix_ms, &reason)
            .and_then(to_value),
        SkillOperation::Rollback {
            skill_id,
            actor,
            reason,
            at_unix_ms,
        } => guard
            .api
            .rollback(&skill_id, &actor, at_unix_ms, &reason)
            .and_then(to_value),
        SkillOperation::AuditHistory { skill_id } => guard
            .api
            .audit_history(skill_id.as_ref())
            .and_then(to_value),
        SkillOperation::Snapshot => guard.api.snapshot_json().and_then(to_value),
    };
    let response = match result {
        Ok(result) => SkillResponse {
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => failure(error.to_string()),
    };
    if let Some(key) = key {
        guard.idempotent.insert(key, response.clone());
    }
    response
}

fn to_value<T: Serialize>(value: T) -> Result<Value, SkillApiError> {
    serde_json::to_value(value).map_err(|error| SkillApiError::Audit(error.to_string()))
}

fn failure(error: impl Into<String>) -> SkillResponse {
    SkillResponse {
        ok: false,
        result: None,
        error: Some(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use victor_domain::{Capability, RiskLevel, ToolDescriptor, ToolId};
    use victor_policy::PolicyConfig;
    use victor_skill_forge::ForgeRequest;
    use victor_skills::{SkillLifecycle, SkillManifest, SkillStep};
    use victor_storage::SqliteContextStore;
    use victor_tools::{ToolManifest, ToolRegistry};

    fn api_with_audit(audit: Arc<SqliteContextStore>) -> SkillApi {
        let mut tools = ToolRegistry::new();
        tools
            .register(ToolManifest::new(
                "1.0.0",
                ToolDescriptor {
                    id: ToolId::parse("demo.read").unwrap(),
                    schema_version: 1,
                    description: "read".into(),
                    input_schema: json!({"type":"object"}),
                    output_schema: json!({"type":"object"}),
                    capabilities: vec![Capability::Network {
                        provider: "test".into(),
                    }],
                    risk: RiskLevel::R0,
                    default_timeout_ms: 1000,
                    undo_support: false,
                    path_arguments: vec![],
                },
            ))
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let policy = victor_policy::PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("demo.read").unwrap()]),
        )
        .unwrap();
        SkillApi::new(Arc::new(tools), Arc::new(policy), audit).unwrap()
    }

    fn api() -> SkillApi {
        api_with_audit(Arc::new(SqliteContextStore::in_memory().unwrap()))
    }

    fn request() -> SkillRequest {
        SkillRequest {
            token: "0123456789abcdef".into(),
            scope: "default".into(),
            idempotency_key: None,
            operation: SkillOperation::CreateDraft {
                request: ForgeRequest {
                    description: "read demo".into(),
                    skill_id: SkillId::parse("transport.demo").unwrap(),
                    version: "1.0.0".into(),
                    input_schema: json!({"type":"object"}),
                    output_schema: json!({"type":"object"}),
                    workflow: vec![SkillStep {
                        id: "read".into(),
                        tool: ToolId::parse("demo.read").unwrap(),
                        args: json!({}),
                        depends_on: vec![],
                    }],
                    dependencies: vec![],
                    author: "user".into(),
                },
            },
        }
    }

    async fn roundtrip(path: &Path, request: SkillRequest) -> SkillResponse {
        let mut stream = UnixStream::connect(path).await.unwrap();
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn unix_transport_auth_scope_and_idempotency_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.sock");
        let server = UnixSkillServer::new(&path, "0123456789abcdef", "default", api()).unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_task = tokio::spawn(server.run(shutdown_rx));
        for _ in 0..20 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut bad = request();
        bad.token = "wrong-token-123456".into();
        assert!(!roundtrip(&path, bad).await.ok);
        let mut bad_scope = request();
        bad_scope.scope = "other".into();
        assert!(!roundtrip(&path, bad_scope).await.ok);
        assert!(roundtrip(&path, request()).await.ok);
        let mut approve = request();
        approve.idempotency_key = Some("approve-1".into());
        approve.operation = SkillOperation::ApproveActivate {
            skill_id: SkillId::parse("transport.demo").unwrap(),
            actor: "user".into(),
            reason: "ok".into(),
            at_unix_ms: 1,
        };
        let mut unauthorized_approve = approve.clone();
        unauthorized_approve.token = "wrong-token-123456".into();
        assert!(!roundtrip(&path, unauthorized_approve).await.ok);
        assert!(roundtrip(&path, approve.clone()).await.ok);
        assert!(roundtrip(&path, approve).await.ok);
        let mut audit = request();
        audit.operation = SkillOperation::AuditHistory { skill_id: None };
        assert_eq!(
            roundtrip(&path, audit)
                .await
                .result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let _ = shutdown_tx.send(());
        assert!(server_task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn production_shaped_smoke_survives_restart_and_persists_audit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.sock");
        let database_path = dir.path().join("skills.sqlite3");
        let audit = Arc::new(SqliteContextStore::open(&database_path).unwrap());
        let server = UnixSkillServer::new(
            &path,
            "0123456789abcdef",
            "default",
            api_with_audit(audit.clone()),
        )
        .unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_task = tokio::spawn(server.run(shutdown_rx));
        for _ in 0..20 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let draft = roundtrip(&path, request()).await;
        assert!(draft.ok);
        let skill_id = SkillId::parse("transport.demo").unwrap();

        let review = roundtrip(
            &path,
            SkillRequest {
                operation: SkillOperation::Review {
                    skill_id: skill_id.clone(),
                },
                ..request()
            },
        )
        .await;
        assert!(review.ok);
        let review_value = review.result.unwrap();
        let mut v1: SkillManifest =
            serde_json::from_value(review_value["manifest"].clone()).unwrap();

        let approved = roundtrip(
            &path,
            lifecycle_request(
                SkillOperation::ApproveActivate {
                    skill_id: skill_id.clone(),
                    actor: "smoke".into(),
                    reason: "approve v1".into(),
                    at_unix_ms: 1,
                },
                "approve-v1",
            ),
        )
        .await;
        assert!(approved.ok);

        let listed = roundtrip(&path, operation_request(SkillOperation::List)).await;
        assert_eq!(
            listed.result.unwrap()[0]["lifecycle"],
            json!(SkillLifecycle::Active)
        );

        let audit_before = roundtrip(
            &path,
            operation_request(SkillOperation::AuditHistory {
                skill_id: Some(skill_id.clone()),
            }),
        )
        .await;
        assert_eq!(audit_before.result.unwrap().as_array().unwrap().len(), 2);

        let disabled = roundtrip(
            &path,
            lifecycle_request(
                SkillOperation::Disable {
                    skill_id: skill_id.clone(),
                    actor: "smoke".into(),
                    reason: "prepare rollback".into(),
                    at_unix_ms: 2,
                },
                "disable-v1",
            ),
        )
        .await;
        assert!(disabled.ok);

        v1.version = "1.0.1".into();
        v1.hash = v1.computed_hash().unwrap();
        let updated = roundtrip(
            &path,
            operation_request(SkillOperation::Update { manifest: v1 }),
        )
        .await;
        assert!(updated.ok);

        let rolled_back = roundtrip(
            &path,
            lifecycle_request(
                SkillOperation::Rollback {
                    skill_id: skill_id.clone(),
                    actor: "smoke".into(),
                    reason: "restore previous version".into(),
                    at_unix_ms: 3,
                },
                "rollback-v2",
            ),
        )
        .await;
        assert!(rolled_back.ok);
        assert_eq!(rolled_back.result.unwrap()["version"], json!("1.0.0"));

        let reactivated = roundtrip(
            &path,
            lifecycle_request(
                SkillOperation::ApproveActivate {
                    skill_id: skill_id.clone(),
                    actor: "smoke".into(),
                    reason: "reactivate restored version".into(),
                    at_unix_ms: 4,
                },
                "approve-restored",
            ),
        )
        .await;
        assert!(reactivated.ok);

        let _ = shutdown_tx.send(());
        assert!(server_task.await.unwrap().is_ok());

        let restarted_store = Arc::new(SqliteContextStore::open(&database_path).unwrap());
        let restored_api = api_with_audit(restarted_store);
        let restart_path = dir.path().join("skills-restart.sock");
        let restarted =
            UnixSkillServer::new(&restart_path, "0123456789abcdef", "default", restored_api)
                .unwrap();
        let (restart_shutdown_tx, restart_shutdown_rx) = oneshot::channel();
        let restart_task = tokio::spawn(restarted.run(restart_shutdown_rx));
        for _ in 0..20 {
            if restart_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let after_restart = roundtrip(&restart_path, operation_request(SkillOperation::List)).await;
        assert!(after_restart.ok);
        let after_restart_result = after_restart.result.unwrap();
        assert_eq!(after_restart_result[0]["version"], json!("1.0.0"));
        assert_eq!(
            after_restart_result[0]["lifecycle"],
            json!(SkillLifecycle::Active)
        );
        let audit_after = roundtrip(
            &restart_path,
            operation_request(SkillOperation::AuditHistory {
                skill_id: Some(skill_id),
            }),
        )
        .await;
        assert_eq!(audit_after.result.unwrap().as_array().unwrap().len(), 6);
        let _ = restart_shutdown_tx.send(());
        assert!(restart_task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn transport_does_not_remove_non_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"keep me").unwrap();
        let server = UnixSkillServer::new(&path, "0123456789abcdef", "default", api()).unwrap();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let error = server.run(shutdown_rx).await.unwrap_err();
        assert!(error.to_string().contains("nie będący Unix socketem"));
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
    }

    fn operation_request(operation: SkillOperation) -> SkillRequest {
        SkillRequest {
            token: "0123456789abcdef".into(),
            scope: "default".into(),
            idempotency_key: None,
            operation,
        }
    }

    fn lifecycle_request(operation: SkillOperation, key: &str) -> SkillRequest {
        SkillRequest {
            idempotency_key: Some(key.into()),
            ..operation_request(operation)
        }
    }
}
