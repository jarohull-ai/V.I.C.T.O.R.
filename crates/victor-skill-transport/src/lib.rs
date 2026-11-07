//! Lokalny transport API skills przez Unix domain socket.

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;
use victor_conversation::ConversationService;
use victor_domain::{Approval, ToolId};
use victor_plugin_host::{PluginLaunchRequest, PluginLifecycleApi};
use victor_skill_api::{SkillApi, SkillApiError};
use victor_skill_forge::ForgeRequest;
use victor_skills::{SkillId, SkillManifest};

pub const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum SkillOperation {
    Chat {
        conversation_id: String,
        input: String,
        at_unix_ms: u64,
        approval: Option<Approval>,
    },
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
    Plugin {
        plugin_operation: PluginOperation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PluginOperation {
    List,
    Review {
        plugin_id: ToolId,
    },
    Register {
        manifest: Box<victor_tools::ToolManifest>,
        launch: Box<PluginLaunchRequest>,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    Activate {
        plugin_id: ToolId,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    Disable {
        plugin_id: ToolId,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    Restart {
        plugin_id: ToolId,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    Unregister {
        plugin_id: ToolId,
        explicit_decision: bool,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    },
    AuditHistory {
        plugin_id: Option<ToolId>,
    },
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
    plugin_api: Option<Arc<Mutex<PluginLifecycleApi>>>,
    conversation: Option<Arc<dyn ConversationService>>,
    token: String,
    scope: String,
    max_request_bytes: usize,
    idempotent: HashMap<String, CachedResponse>,
}

#[derive(Clone)]
struct CachedResponse {
    principal: String,
    scope: String,
    request_hash: String,
    response: SkillResponse,
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
                plugin_api: None,
                conversation: None,
                token,
                scope,
                max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
                idempotent: HashMap::new(),
            })),
            request_timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_plugin_api(self, plugin_api: PluginLifecycleApi) -> Self {
        self.with_plugin_api_shared(Arc::new(Mutex::new(plugin_api)))
    }

    pub fn with_plugin_api_shared(self, plugin_api: Arc<Mutex<PluginLifecycleApi>>) -> Self {
        if let Ok(mut guard) = self.state.try_lock() {
            guard.plugin_api = Some(plugin_api);
        }
        self
    }

    pub fn with_conversation(self, service: Arc<dyn ConversationService>) -> Self {
        if let Ok(mut guard) = self.state.try_lock() {
            guard.conversation = Some(service);
        }
        self
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
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(TransportError::InvalidConfig(
                    "socket należy do innego użytkownika".into(),
                ));
            }
            remove_owned_socket(&self.socket_path)?;
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    let _ = remove_owned_socket(&self.socket_path);
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    if let Ok(cred) = stream.peer_cred() {
                        if cred.uid() != unsafe { libc::geteuid() } {
                            continue;
                        }
                    }
                    let state = self.state.clone();
                    let request_timeout = self.request_timeout;
                    tokio::spawn(async move { let _ = handle_connection(stream, state, request_timeout).await; });
                }
            }
        }
    }
}

fn remove_owned_socket(path: &Path) -> Result<(), TransportError> {
    let metadata = std::fs::symlink_metadata(path).map_err(TransportError::Io)?;
    if !metadata.file_type().is_socket() {
        return Err(TransportError::InvalidConfig(
            "odmowa usunięcia ścieżki nie będącej socketem".into(),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(TransportError::InvalidConfig(
            "odmowa usunięcia socketu obcego właściciela".into(),
        ));
    }
    std::fs::remove_file(path).map_err(TransportError::Io)
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
        let bytes = match timeout(
            request_timeout,
            read_limited_line(&mut reader, max_request_bytes),
        )
        .await
        .map_err(|_| TransportError::Timeout)?
        {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(()),
            Err(error) => {
                write_response(&mut write_half, failure(error.to_string())).await?;
                return Ok(());
            }
        };
        if bytes.is_empty() {
            return Ok(());
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

async fn read_limited_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, TransportError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(frame))
            };
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(position + 1) > max_bytes {
                return Err(TransportError::RequestTooLarge(max_bytes));
            }
            frame.extend_from_slice(&available[..=position]);
            reader.consume(position + 1);
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > max_bytes {
            return Err(TransportError::RequestTooLarge(max_bytes));
        }
        frame.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
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
    let principal = hash_text(&request.token);
    let request_hash = match serde_json::to_vec(&(&request.scope, &request.operation)) {
        Ok(bytes) => hash_bytes(&bytes),
        Err(error) => return failure(format!("nie można zhashować żądania: {error}")),
    };
    let mut guard = state.lock().await;
    if request.token != guard.token {
        return failure("nieautoryzowany klient");
    }
    if request.scope != guard.scope {
        return failure("odmowa scope klienta");
    }
    if let Some(key) = key.as_ref() {
        if let Some(cached) = guard.idempotent.get(key) {
            if cached.principal == principal
                && cached.scope == request.scope
                && cached.request_hash == request_hash
            {
                return cached.response.clone();
            }
            return failure("konflikt klucza idempotency");
        }
    }
    let result: Result<Value, String> = match request.operation {
        SkillOperation::Chat {
            conversation_id,
            input,
            at_unix_ms,
            approval,
        } => {
            let Some(service) = guard.conversation.as_ref().cloned() else {
                return failure("conversation API nie jest skonfigurowane");
            };
            service
                .chat(
                    conversation_id,
                    request.scope.clone(),
                    input,
                    at_unix_ms,
                    approval,
                )
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))
        }
        SkillOperation::List => {
            serde_json::to_value(guard.api.list()).map_err(|error| error.to_string())
        }
        SkillOperation::Review { skill_id } => guard
            .api
            .review(&skill_id)
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::CreateDraft { request } => guard
            .api
            .create_draft(request)
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::Update { manifest } => guard
            .api
            .update_manifest(manifest)
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::ApproveActivate {
            skill_id,
            actor,
            reason,
            at_unix_ms,
        } => guard
            .api
            .validate_and_approve_activate(&skill_id, &actor, at_unix_ms, &reason)
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::Disable {
            skill_id,
            actor,
            reason,
            at_unix_ms,
        } => guard
            .api
            .disable(&skill_id, &actor, at_unix_ms, &reason)
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::Rollback {
            skill_id,
            actor,
            reason,
            at_unix_ms,
        } => guard
            .api
            .rollback(&skill_id, &actor, at_unix_ms, &reason)
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::AuditHistory { skill_id } => guard
            .api
            .audit_history(skill_id.as_ref())
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::Snapshot => guard
            .api
            .snapshot_json()
            .and_then(to_value)
            .map_err(|error| error.to_string()),
        SkillOperation::Plugin { plugin_operation } => {
            let Some(plugin_api) = guard.plugin_api.as_ref().cloned() else {
                return failure("plugin lifecycle API nie jest skonfigurowane");
            };
            let mut plugin_api = plugin_api.lock().await;
            dispatch_plugin(&mut plugin_api, plugin_operation).await
        }
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
        guard.idempotent.insert(
            key,
            CachedResponse {
                principal,
                scope: request.scope,
                request_hash,
                response: response.clone(),
            },
        );
    }
    response
}

async fn dispatch_plugin(
    api: &mut PluginLifecycleApi,
    operation: PluginOperation,
) -> Result<Value, String> {
    match operation {
        PluginOperation::List => plugin_value(api.list()),
        PluginOperation::Review { plugin_id } => plugin_value(api.review(&plugin_id)),
        PluginOperation::Register {
            manifest,
            launch,
            approval,
            actor,
            reason,
            at_unix_ms,
        } => plugin_value(
            api.register(*manifest, *launch, approval, actor, reason, at_unix_ms)
                .await,
        ),
        PluginOperation::Activate {
            plugin_id,
            approval,
            actor,
            reason,
            at_unix_ms,
        } => plugin_value(
            api.activate(&plugin_id, approval, actor, reason, at_unix_ms)
                .await,
        ),
        PluginOperation::Disable {
            plugin_id,
            actor,
            reason,
            at_unix_ms,
        } => plugin_value(api.disable(&plugin_id, actor, reason, at_unix_ms)),
        PluginOperation::Restart {
            plugin_id,
            approval,
            actor,
            reason,
            at_unix_ms,
        } => plugin_value(
            api.restart(&plugin_id, approval, actor, reason, at_unix_ms)
                .await,
        ),
        PluginOperation::Unregister {
            plugin_id,
            explicit_decision,
            approval,
            actor,
            reason,
            at_unix_ms,
        } => plugin_value(api.unregister(
            &plugin_id,
            explicit_decision,
            approval,
            actor,
            reason,
            at_unix_ms,
        )),
        PluginOperation::AuditHistory { plugin_id } => {
            plugin_value(api.audit_history(plugin_id.as_ref()))
        }
    }
}

fn plugin_value<T: Serialize>(
    result: Result<T, victor_plugin_host::HostError>,
) -> Result<Value, String> {
    result
        .map_err(|error| error.to_string())
        .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))
}

fn hash_text(value: &str) -> String {
    hash_bytes(value.as_bytes())
}

fn hash_bytes(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
        assert!(roundtrip(&path, approve.clone()).await.ok);
        let mut conflicting = approve;
        conflicting.operation = SkillOperation::Disable {
            skill_id: SkillId::parse("transport.demo").unwrap(),
            actor: "user".into(),
            reason: "different request".into(),
            at_unix_ms: 2,
        };
        assert!(!roundtrip(&path, conflicting).await.ok);
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
    async fn plugin_operations_use_auth_scope_and_redacted_listing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.sock");
        let database = Arc::new(SqliteContextStore::in_memory().unwrap());
        let root = tempfile::tempdir().unwrap();
        let policy = Arc::new(
            victor_policy::PolicyEngine::new(PolicyConfig::new([root.path().to_path_buf()]))
                .unwrap(),
        );
        let plugin_api = victor_plugin_host::PluginLifecycleApi::open(database, policy, None)
            .await
            .unwrap();
        let server = UnixSkillServer::new(&path, "0123456789abcdef", "default", api())
            .unwrap()
            .with_plugin_api(plugin_api);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_task = tokio::spawn(server.run(shutdown_rx));
        for _ in 0..20 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let list = roundtrip(
            &path,
            operation_request(SkillOperation::Plugin {
                plugin_operation: PluginOperation::List,
            }),
        )
        .await;
        assert!(list.ok);
        assert!(list.result.unwrap().as_array().unwrap().is_empty());
        let mut bad_scope = operation_request(SkillOperation::Plugin {
            plugin_operation: PluginOperation::List,
        });
        bad_scope.scope = "other".into();
        assert!(!roundtrip(&path, bad_scope).await.ok);
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
