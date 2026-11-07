//! Host bezpośredniego dostępu do capabilities.
//!
//! Proces pluginu ma tylko kontrakt stdio. Policy, audit, context i approval są
//! nadal własnością `victor-runtime`; ten crate dostarcza wyłącznie executor.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use victor_domain::{Approval, Plan, PlanStep, ToolId, UnixMs};
use victor_plugin_contract::{
    decode_frame_with_limit, encode_with_limit, Hello, PluginManifest, Request, Response,
    WireMessage, DEFAULT_HEARTBEAT_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};
use victor_policy::{Outcome, PolicyEngine};
use victor_storage::{PluginRegistrationRecord, SqliteContextStore};
use victor_tools::{
    AuditOutcome, AuditSink, ToolAuditEvent, ToolError, ToolExecution, ToolExecutor, ToolManifest,
};

pub const DEFAULT_MAX_RESTART_ATTEMPTS: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("nie można uruchomić pluginu: {0}")]
    Spawn(String),
    #[error("plugin nie przeszedł handshake: {0}")]
    Handshake(String),
    #[error("plugin rozłączył się")]
    Disconnected,
    #[error("plugin przekroczył limit ramki")]
    FrameTooLarge,
    #[error("timeout pluginu po {0} ms")]
    Timeout(u64),
    #[error("błąd I/O pluginu: {0}")]
    Io(String),
    #[error("plugin zwrócił błąd {code}: {message}")]
    Plugin { code: String, message: String },
    #[error("manifest pluginu nie pasuje do lokalnego ToolManifest: {0}")]
    ManifestMismatch(String),
    #[error("nieprawidłowe framing pluginu: {0}")]
    InvalidFraming(String),
    #[error("plugin wymaga sandboxa, ale bubblewrap nie jest dostępny")]
    SandboxUnavailable,
    #[error("nieprawidłowa polityka uruchomienia pluginu: {0}")]
    InvalidLaunchPolicy(String),
    #[error("konflikt istniejącej rejestracji pluginu: {0}")]
    RegistrationConflict(String),
    #[error("plugin nie istnieje: {0}")]
    NotRegistered(String),
    #[error("niebezpieczny executable pluginu: {0}")]
    UnsafeExecutable(String),
    #[error("launch spec pluginu jest nieaktualny: {0}")]
    StaleLaunchSpec(String),
}

struct ProcessConnection {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    max_frame_bytes: usize,
}

impl ProcessConnection {
    async fn terminate(&mut self) {
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxRequirement {
    NotRequired,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginLaunchRequest {
    pub command: Vec<String>,
    pub working_dir: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub sandbox: SandboxRequirement,
    pub sandbox_executable: Option<String>,
    pub request_timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_restart_attempts: u32,
}

impl PluginLaunchRequest {
    fn policy(&self) -> Result<PluginLaunchPolicy, HostError> {
        Ok(PluginLaunchPolicy {
            sandbox: self.sandbox.clone(),
            sandbox_executable: self.sandbox_executable.as_ref().map(PathBuf::from),
            working_dir: Some(PathBuf::from(&self.working_dir)),
            environment: self.environment.clone(),
            request_timeout_ms: self.request_timeout_ms,
            max_frame_bytes: self.max_frame_bytes,
            max_restart_attempts: self.max_restart_attempts,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSummary {
    pub plugin_id: String,
    pub plugin_version: String,
    pub manifest_hash: String,
    pub sandbox_required: bool,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginReview {
    pub summary: PluginSummary,
    pub executable_name: String,
    pub working_dir: String,
    pub environment_keys: Vec<String>,
    pub argument_count: usize,
    pub request_timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_restart_attempts: u32,
    pub launch_spec_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginLifecycleDecision {
    pub plugin_id: String,
    pub action: String,
    pub status: String,
    pub actor: String,
    pub reason: String,
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLaunchPolicy {
    pub sandbox: SandboxRequirement,
    pub sandbox_executable: Option<PathBuf>,
    pub working_dir: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub request_timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_restart_attempts: u32,
}

impl Default for PluginLaunchPolicy {
    fn default() -> Self {
        Self {
            sandbox: SandboxRequirement::NotRequired,
            sandbox_executable: None,
            working_dir: None,
            environment: BTreeMap::new(),
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            max_frame_bytes: MAX_FRAME_BYTES,
            max_restart_attempts: DEFAULT_MAX_RESTART_ATTEMPTS,
        }
    }
}

pub trait SandboxRunner: Send + Sync {
    fn wrap_command(
        &self,
        command: &mut Command,
        executable: &str,
        args: &[String],
        policy: &PluginLaunchPolicy,
    ) -> Result<(), HostError>;
}

#[derive(Debug, Clone)]
pub struct BubblewrapSandboxRunner {
    executable: PathBuf,
}

impl BubblewrapSandboxRunner {
    pub fn detect() -> Option<Self> {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("bwrap"))
                .find(|candidate| candidate.is_file())
                .map(|executable| Self { executable })
        })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            executable: path.into(),
        }
    }
}

impl SandboxRunner for BubblewrapSandboxRunner {
    fn wrap_command(
        &self,
        command: &mut Command,
        executable: &str,
        args: &[String],
        policy: &PluginLaunchPolicy,
    ) -> Result<(), HostError> {
        if !self.executable.is_file() {
            return Err(HostError::SandboxUnavailable);
        }
        let mut wrapped = vec![
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
            "--clearenv".to_string(),
            "--unshare-net".to_string(),
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
        ];
        for (key, value) in &policy.environment {
            wrapped.extend(["--setenv".to_string(), key.clone(), value.clone()]);
        }
        wrapped.extend(["--".to_string(), executable.to_string()]);
        wrapped.extend(args.iter().cloned());
        *command = Command::new(&self.executable);
        command.args(wrapped);
        if let Some(dir) = &policy.working_dir {
            command.current_dir(dir);
        }
        Ok(())
    }
}

impl ProcessConnection {
    async fn send(&mut self, message: &WireMessage) -> Result<(), HostError> {
        let frame = encode_with_limit(message, self.max_frame_bytes)
            .map_err(|_| HostError::FrameTooLarge)?;
        self.stdin
            .write_all(&frame)
            .await
            .map_err(|e| HostError::Io(e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| HostError::Io(e.to_string()))?;
        Ok(())
    }

    async fn receive(&mut self, timeout_ms: u64) -> Result<WireMessage, HostError> {
        let line = timeout(
            Duration::from_millis(timeout_ms),
            read_limited_frame(&mut self.stdout, self.max_frame_bytes),
        )
        .await
        .map_err(|_| HostError::Timeout(timeout_ms))?
        .map_err(|error| match error {
            FrameReadError::TooLarge => HostError::FrameTooLarge,
            FrameReadError::Unterminated => {
                HostError::InvalidFraming("EOF przed końcem ramki".into())
            }
            FrameReadError::Io(error) => HostError::Io(error.to_string()),
        })?;
        let Some(line) = line else {
            return Err(HostError::Disconnected);
        };
        if line.len() > MAX_FRAME_BYTES {
            return Err(HostError::FrameTooLarge);
        }
        decode_frame_with_limit(&line, self.max_frame_bytes).map_err(|e| match e {
            victor_plugin_contract::ContractError::FrameTooLarge { .. } => HostError::FrameTooLarge,
            victor_plugin_contract::ContractError::MissingFrameDelimiter => {
                HostError::InvalidFraming("brak delimitera ramki".into())
            }
            other => HostError::Handshake(other.to_string()),
        })
    }
}

enum FrameReadError {
    TooLarge,
    Unterminated,
    Io(std::io::Error),
}

async fn read_limited_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, FrameReadError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(FrameReadError::Io)?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(FrameReadError::Unterminated)
            };
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(position + 1) > max_bytes {
                return Err(FrameReadError::TooLarge);
            }
            frame.extend_from_slice(&available[..=position]);
            reader.consume(position + 1);
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > max_bytes {
            return Err(FrameReadError::TooLarge);
        }
        frame.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

pub struct PluginToolExecutor {
    tool_id: ToolId,
    local_manifest: Arc<ToolManifest>,
    command: Arc<Vec<String>>,
    connection: Arc<Mutex<ProcessConnection>>,
    request_timeout_ms: u64,
    restart_attempts: Arc<Mutex<u32>>,
    max_restart_attempts: u32,
    audit: Option<Arc<dyn AuditSink>>,
    launch_policy: PluginLaunchPolicy,
}

/// Rejestr hosta. Rejestruje wyłącznie lokalnie zaufane manifesty narzędzi;
/// manifest otrzymany z procesu służy tylko do sprawdzenia zgodności schematu.
#[derive(Default)]
pub struct PluginHost {
    tools: BTreeMap<String, Arc<PluginToolExecutor>>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(
        &mut self,
        command: Vec<String>,
        manifest: ToolManifest,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        self.register_with_policy(command, manifest, PluginLaunchPolicy::default(), None)
            .await
    }

    pub async fn register_with_policy(
        &mut self,
        command: Vec<String>,
        manifest: ToolManifest,
        launch_policy: PluginLaunchPolicy,
        audit: Option<Arc<dyn AuditSink>>,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let id = manifest.descriptor.id.as_str().to_string();
        if self.tools.contains_key(&id) {
            return Err(HostError::ManifestMismatch(format!(
                "narzędzie {id} jest już zarejestrowane"
            )));
        }
        let executor = Arc::new(
            PluginToolExecutor::spawn_with_policy(command, manifest, launch_policy, audit).await?,
        );
        self.tools.insert(id, executor.clone());
        Ok(executor)
    }

    /// Rejestruje executor i lokalny manifest w tej samej ścieżce, z której
    /// runtime pobiera capabilities oraz risk. Plugin nie może rozszerzyć
    /// tego manifestu przez własny handshake.
    pub async fn register_into_registry(
        &mut self,
        command: Vec<String>,
        manifest: ToolManifest,
        launch_policy: PluginLaunchPolicy,
        audit: Option<Arc<dyn AuditSink>>,
        registry: &mut victor_tools::ToolRegistry,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let id = manifest.descriptor.id.clone();
        if registry.get(&id).is_some() {
            return Err(HostError::ManifestMismatch(format!(
                "narzędzie {} jest już w rejestrze runtime",
                id
            )));
        }
        let executor = self
            .register_with_policy(command, manifest.clone(), launch_policy, audit)
            .await?;
        if let Err(error) = registry.register(manifest) {
            self.tools.remove(id.as_str());
            return Err(HostError::ManifestMismatch(error.to_string()));
        }
        Ok(executor)
    }

    pub fn get(&self, id: &ToolId) -> Option<Arc<PluginToolExecutor>> {
        self.tools.get(id.as_str()).cloned()
    }

    pub fn tool_ids(&self) -> Vec<ToolId> {
        self.tools
            .keys()
            .filter_map(|id| ToolId::parse(id).ok())
            .collect()
    }
}

/// Rejestr pluginów związany z SQLite. Przy starcie uruchamia tylko rekordy
/// `active`; manifest jest deserializowany i hashowany przed wykonaniem komendy.
pub struct PersistentPluginHost {
    host: PluginHost,
    store: Arc<SqliteContextStore>,
    audit: Option<Arc<dyn AuditSink>>,
}

impl PersistentPluginHost {
    fn record_lifecycle(
        &self,
        id: &ToolId,
        outcome: AuditOutcome,
        summary: &str,
    ) -> Result<(), HostError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        let version = self
            .store
            .plugin_registrations()
            .map_err(storage_host_error)?
            .into_iter()
            .find(|record| record.plugin_id == id.as_str())
            .map(|record| record.plugin_version)
            .unwrap_or_else(|| "unknown".into());
        audit
            .record(ToolAuditEvent {
                task_id: format!("plugin-lifecycle:{id}"),
                tool: id.clone(),
                tool_version: version,
                plan_hash: "plugin-lifecycle".into(),
                started_at_unix_ms: 0,
                finished_at_unix_ms: 0,
                outcome,
                summary: summary.into(),
            })
            .map_err(|error| HostError::Spawn(format!("audit lifecycle pluginu: {error}")))
    }
}

impl PersistentPluginHost {
    pub async fn open(
        store: Arc<SqliteContextStore>,
        audit: Option<Arc<dyn AuditSink>>,
    ) -> Result<Self, HostError> {
        let mut persistent = Self {
            host: PluginHost::new(),
            store,
            audit,
        };
        for record in persistent
            .store
            .plugin_registrations()
            .map_err(storage_host_error)?
        {
            if record.status != "active" {
                continue;
            }
            let manifest: ToolManifest = serde_json::from_str(&record.manifest_json)
                .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
            if manifest.descriptor.id.as_str() != record.plugin_id
                || manifest_hash(&manifest)? != record.manifest_hash
            {
                return Err(HostError::ManifestMismatch(format!(
                    "hash lub identyfikator pluginu {} nie pasuje do SQLite",
                    record.plugin_id
                )));
            }
            validate_persisted_launch_spec(&record, &manifest)?;
            let policy = policy_from_record(&record);
            persistent
                .host
                .register_with_policy(record.command, manifest, policy, persistent.audit.clone())
                .await?;
        }
        Ok(persistent)
    }

    pub async fn register(
        &mut self,
        command: Vec<String>,
        manifest: ToolManifest,
        launch_policy: PluginLaunchPolicy,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let plugin_id = manifest.descriptor.id.as_str().to_string();
        let launch_policy = normalize_launch_policy(launch_policy)?;
        let (executable_path, executable_hash) = validate_executable(&command)?;
        let manifest_hash_value = manifest_hash(&manifest)?;
        let manifest_json = serde_json::to_string(&manifest)
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        let working_dir = launch_policy
            .working_dir
            .as_ref()
            .expect("normalized launch policy has working directory");
        let mut record = PluginRegistrationRecord {
            plugin_id: plugin_id.clone(),
            plugin_version: manifest.tool_version.clone(),
            manifest_json,
            manifest_hash: manifest_hash_value,
            command: command.clone(),
            executable_path: executable_path.display().to_string(),
            executable_hash,
            working_dir: working_dir.display().to_string(),
            environment: launch_policy.environment.clone(),
            sandbox_required: matches!(launch_policy.sandbox, SandboxRequirement::Required),
            sandbox_executable: launch_policy
                .sandbox_executable
                .as_ref()
                .map(|path| path.display().to_string()),
            request_timeout_ms: launch_policy.request_timeout_ms,
            max_frame_bytes: launch_policy.max_frame_bytes,
            max_restart_attempts: launch_policy.max_restart_attempts,
            launch_spec_hash: String::new(),
            status: "active".into(),
        };
        record.launch_spec_hash = launch_spec_hash(&record)?;
        if let Some(existing) = self
            .store
            .plugin_registrations()
            .map_err(storage_host_error)?
            .into_iter()
            .find(|item| item.plugin_id == plugin_id)
        {
            if existing != record {
                return Err(HostError::RegistrationConflict(plugin_id));
            }
            if let Some(existing_executor) = self.host.get(&manifest.descriptor.id) {
                return Ok(existing_executor);
            }
        }
        let executor = self
            .host
            .register_with_policy(
                command.clone(),
                manifest.clone(),
                launch_policy.clone(),
                self.audit.clone(),
            )
            .await?;
        self.store
            .save_plugin_registration(&record)
            .map_err(storage_host_error)?;
        self.record_lifecycle(
            &manifest.descriptor.id,
            AuditOutcome::Succeeded,
            "register active",
        )?;
        Ok(executor)
    }

    pub async fn register_into_registry(
        &mut self,
        command: Vec<String>,
        manifest: ToolManifest,
        launch_policy: PluginLaunchPolicy,
        registry: &mut victor_tools::ToolRegistry,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let id = manifest.descriptor.id.clone();
        if registry.get(&id).is_some() {
            return Err(HostError::ManifestMismatch(format!(
                "narzędzie {} jest już w rejestrze runtime",
                id
            )));
        }
        let executor = self
            .register(command, manifest.clone(), launch_policy)
            .await?;
        if let Err(error) = registry.register(manifest) {
            self.host.tools.remove(id.as_str());
            let _ = self.store.set_plugin_status(id.as_str(), "disabled");
            return Err(HostError::ManifestMismatch(error.to_string()));
        }
        Ok(executor)
    }

    pub fn get(&self, id: &ToolId) -> Option<Arc<PluginToolExecutor>> {
        self.host.get(id)
    }

    pub fn tool_ids(&self) -> Vec<ToolId> {
        self.host.tool_ids()
    }

    pub fn disable(&mut self, id: &ToolId) -> Result<(), HostError> {
        if self.host.tools.remove(id.as_str()).is_none()
            && !self
                .store
                .plugin_registrations()
                .map_err(storage_host_error)?
                .iter()
                .any(|record| record.plugin_id == id.as_str())
        {
            return Err(HostError::NotRegistered(id.to_string()));
        }
        self.store
            .set_plugin_status(id.as_str(), "disabled")
            .map_err(storage_host_error)?;
        self.record_lifecycle(id, AuditOutcome::Succeeded, "disable")
    }

    fn record_for(&self, id: &ToolId) -> Result<PluginRegistrationRecord, HostError> {
        self.store
            .plugin_registrations()
            .map_err(storage_host_error)?
            .into_iter()
            .find(|record| record.plugin_id == id.as_str())
            .ok_or_else(|| HostError::NotRegistered(id.to_string()))
    }

    pub async fn activate(
        &mut self,
        id: &ToolId,
        registry: &mut victor_tools::ToolRegistry,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let executor = self.activate_registered(id).await?;
        let record = self.record_for(id)?;
        let manifest: ToolManifest = serde_json::from_str(&record.manifest_json)
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        if registry.get(id).is_none() {
            registry
                .register(manifest)
                .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        }
        Ok(executor)
    }

    async fn activate_registered(
        &mut self,
        id: &ToolId,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let record = self.record_for(id)?;
        if record.status == "active" {
            if let Some(executor) = self.host.get(id) {
                return Ok(executor);
            }
        }
        let manifest: ToolManifest = serde_json::from_str(&record.manifest_json)
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        validate_persisted_launch_spec(&record, &manifest)?;
        let policy = policy_from_record(&record);
        let executor = self
            .host
            .register_with_policy(
                record.command.clone(),
                manifest.clone(),
                policy,
                self.audit.clone(),
            )
            .await?;
        self.store
            .set_plugin_status(id.as_str(), "active")
            .map_err(storage_host_error)?;
        self.record_lifecycle(id, AuditOutcome::Succeeded, "activate")?;
        Ok(executor)
    }

    pub async fn restart(&mut self, id: &ToolId) -> Result<Arc<PluginToolExecutor>, HostError> {
        let record = self
            .store
            .plugin_registrations()
            .map_err(storage_host_error)?
            .into_iter()
            .find(|record| record.plugin_id == id.as_str())
            .ok_or_else(|| HostError::NotRegistered(id.to_string()))?;
        let manifest: ToolManifest = serde_json::from_str(&record.manifest_json)
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        validate_persisted_launch_spec(&record, &manifest)?;
        let executor = self
            .host
            .get(id)
            .ok_or_else(|| HostError::NotRegistered(id.to_string()))?;
        executor.restart().await?;
        self.record_lifecycle(id, AuditOutcome::Succeeded, "restart")?;
        Ok(executor)
    }

    pub fn unregister(&mut self, id: &ToolId, explicit_decision: bool) -> Result<(), HostError> {
        if !explicit_decision {
            return Err(HostError::InvalidLaunchPolicy(
                "unregister wymaga jawnej decyzji użytkownika".into(),
            ));
        }
        self.host.tools.remove(id.as_str());
        self.store
            .remove_plugin_registration(id.as_str())
            .map_err(storage_host_error)?;
        self.record_lifecycle(id, AuditOutcome::Succeeded, "unregister")
    }
}

/// UI-agnostyczna brama lifecycle. Transport i CLI otrzymują wyłącznie ten
/// interfejs; registry, policy, SQLite i executory pozostają po stronie hosta.
pub struct PluginLifecycleApi {
    host: PersistentPluginHost,
    policy: Arc<PolicyEngine>,
    store: Arc<SqliteContextStore>,
    catalog: BTreeMap<String, (ToolManifest, Vec<String>, PluginLaunchPolicy)>,
}

impl PluginLifecycleApi {
    /// Zwraca executor już uruchomionego, zarejestrowanego pluginu. Dostęp
    /// pozostaje za bramą lifecycle; klient transportu nie otrzymuje hosta.
    pub fn get(&self, id: &ToolId) -> Option<Arc<PluginToolExecutor>> {
        self.host.get(id)
    }

    pub async fn open(
        store: Arc<SqliteContextStore>,
        policy: Arc<PolicyEngine>,
        audit: Option<Arc<dyn AuditSink>>,
    ) -> Result<Self, HostError> {
        for record in store
            .plugin_registrations()
            .map_err(storage_host_error)?
            .into_iter()
            .filter(|record| record.status == "active")
        {
            let manifest: ToolManifest = serde_json::from_str(&record.manifest_json)
                .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
            check_manifest_policy(&policy, &manifest, None, 0)?;
        }
        let host = PersistentPluginHost::open(store.clone(), audit).await?;
        Ok(Self {
            host,
            policy,
            store,
            catalog: BTreeMap::new(),
        })
    }

    /// Dodaje definicję zaufaną przez proces serwera. UI/CLI może wybrać tylko
    /// jej identyfikator; nie może nadać sobie manifestu, capabilities, risku
    /// ani launch policy.
    pub fn with_plugin_definition(
        mut self,
        manifest: ToolManifest,
        command: Vec<String>,
        launch_policy: PluginLaunchPolicy,
    ) -> Result<Self, HostError> {
        manifest
            .validate()
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        let id = manifest.descriptor.id.as_str().to_string();
        if self.catalog.contains_key(&id) {
            return Err(HostError::RegistrationConflict(id));
        }
        let launch_policy = normalize_launch_policy(launch_policy)?;
        validate_executable(&command)?;
        self.catalog.insert(id, (manifest, command, launch_policy));
        Ok(self)
    }

    pub fn list(&self) -> Result<Vec<PluginSummary>, HostError> {
        self.store
            .plugin_registrations()
            .map_err(storage_host_error)
            .map(|records| records.iter().map(summary_from_record).collect())
    }

    pub fn review(&self, id: &ToolId) -> Result<PluginReview, HostError> {
        let record = self.record(id)?;
        let launch_spec_verified = self.verify_record(&record).is_ok();
        let executable_name = Path::new(&record.executable_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        Ok(PluginReview {
            summary: summary_from_record(&record),
            executable_name,
            working_dir: record.working_dir.clone(),
            environment_keys: record.environment.keys().cloned().collect(),
            argument_count: record.command.len().saturating_sub(1),
            request_timeout_ms: record.request_timeout_ms,
            max_frame_bytes: record.max_frame_bytes,
            max_restart_attempts: record.max_restart_attempts,
            launch_spec_verified,
        })
    }

    pub fn audit_history(
        &self,
        id: Option<&ToolId>,
    ) -> Result<Vec<PluginLifecycleDecision>, HostError> {
        self.store
            .plugin_lifecycle_history(id.map(ToolId::as_str))
            .map_err(storage_host_error)
            .map(|records| {
                records
                    .into_iter()
                    .map(|record| PluginLifecycleDecision {
                        plugin_id: record.plugin_id,
                        action: record.action,
                        status: record.status,
                        actor: record.actor,
                        reason: record.reason,
                        at_unix_ms: record.at_unix_ms,
                    })
                    .collect()
            })
    }

    pub async fn register(
        &mut self,
        manifest: ToolManifest,
        launch: PluginLaunchRequest,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    ) -> Result<PluginSummary, HostError> {
        let trusted = self
            .catalog
            .get(manifest.descriptor.id.as_str())
            .ok_or_else(|| {
                HostError::InvalidLaunchPolicy("plugin nie ma zaufanej definicji serwera".into())
            })?;
        if trusted.0 != manifest || trusted.1 != launch.command || trusted.2 != launch.policy()? {
            return Err(HostError::InvalidLaunchPolicy(
                "manifest lub launch spec różni się od definicji serwera".into(),
            ));
        }
        self.check_policy(&manifest, approval.as_ref(), at_unix_ms)?;
        let id = manifest.descriptor.id.clone();
        let policy = launch.policy()?;
        self.host.register(launch.command, manifest, policy).await?;
        self.record_decision(&id, "register", "active", actor, reason, at_unix_ms)?;
        self.summary(&id)
    }

    pub async fn activate(
        &mut self,
        id: &ToolId,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    ) -> Result<PluginSummary, HostError> {
        let manifest = self.manifest(id)?;
        self.check_policy(&manifest, approval.as_ref(), at_unix_ms)?;
        self.host.activate_registered(id).await?;
        self.record_decision(id, "activate", "active", actor, reason, at_unix_ms)?;
        self.summary(id)
    }

    pub fn disable(
        &mut self,
        id: &ToolId,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    ) -> Result<PluginSummary, HostError> {
        let manifest = self.manifest(id)?;
        self.check_policy(&manifest, None, at_unix_ms)?;
        self.host.disable(id)?;
        self.record_decision(id, "disable", "disabled", actor, reason, at_unix_ms)?;
        self.summary(id)
    }

    pub async fn restart(
        &mut self,
        id: &ToolId,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    ) -> Result<PluginSummary, HostError> {
        let manifest = self.manifest(id)?;
        self.check_policy(&manifest, approval.as_ref(), at_unix_ms)?;
        self.host.restart(id).await?;
        self.record_decision(id, "restart", "active", actor, reason, at_unix_ms)?;
        self.summary(id)
    }

    pub fn unregister(
        &mut self,
        id: &ToolId,
        explicit_decision: bool,
        approval: Option<Approval>,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    ) -> Result<Value, HostError> {
        if !explicit_decision {
            return Err(HostError::InvalidLaunchPolicy(
                "unregister wymaga jawnej decyzji użytkownika".into(),
            ));
        }
        let manifest = self.manifest(id)?;
        self.check_policy(&manifest, approval.as_ref(), at_unix_ms)?;
        self.host.unregister(id, true)?;
        self.record_decision(id, "unregister", "removed", actor, reason, at_unix_ms)?;
        Ok(serde_json::json!({"plugin_id": id, "status": "removed"}))
    }

    fn record(&self, id: &ToolId) -> Result<PluginRegistrationRecord, HostError> {
        self.store
            .plugin_registrations()
            .map_err(storage_host_error)?
            .into_iter()
            .find(|record| record.plugin_id == id.as_str())
            .ok_or_else(|| HostError::NotRegistered(id.to_string()))
    }

    fn manifest(&self, id: &ToolId) -> Result<ToolManifest, HostError> {
        let record = self.record(id)?;
        serde_json::from_str(&record.manifest_json)
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))
    }

    fn summary(&self, id: &ToolId) -> Result<PluginSummary, HostError> {
        Ok(summary_from_record(&self.record(id)?))
    }

    fn verify_record(&self, record: &PluginRegistrationRecord) -> Result<(), HostError> {
        let manifest: ToolManifest = serde_json::from_str(&record.manifest_json)
            .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
        validate_persisted_launch_spec(record, &manifest)
    }

    fn check_policy(
        &self,
        manifest: &ToolManifest,
        approval: Option<&Approval>,
        now: u64,
    ) -> Result<(), HostError> {
        check_manifest_policy(&self.policy, manifest, approval, now)
    }
}

fn check_manifest_policy(
    policy: &PolicyEngine,
    manifest: &ToolManifest,
    approval: Option<&Approval>,
    now: u64,
) -> Result<(), HostError> {
    let plan = Plan::new(
        "plugin-lifecycle",
        now.min(u32::MAX as u64) as u32,
        format!("plugin lifecycle: {}", manifest.descriptor.id),
        vec![PlanStep::new(
            manifest.descriptor.id.clone(),
            "plugin lifecycle",
            serde_json::json!({}),
            manifest.descriptor.risk,
        )],
    )
    .map_err(|error| HostError::InvalidLaunchPolicy(error.to_string()))?;
    let decision = policy.evaluate_plan(std::slice::from_ref(&manifest.descriptor), &plan);
    if decision.outcome == Outcome::Denied {
        return Err(HostError::InvalidLaunchPolicy(decision.reasons.join("; ")));
    }
    policy
        .require_approval(&plan, approval, now)
        .map_err(|error| HostError::InvalidLaunchPolicy(error.to_string()))
}

impl PluginLifecycleApi {
    fn record_decision(
        &self,
        id: &ToolId,
        action: &str,
        status: &str,
        actor: String,
        reason: String,
        at_unix_ms: u64,
    ) -> Result<(), HostError> {
        self.store
            .record_plugin_lifecycle(&victor_storage::PluginLifecycleAuditRecord {
                plugin_id: id.to_string(),
                action: action.into(),
                status: status.into(),
                actor,
                reason,
                at_unix_ms,
            })
            .map_err(storage_host_error)
    }
}

fn summary_from_record(record: &PluginRegistrationRecord) -> PluginSummary {
    PluginSummary {
        plugin_id: record.plugin_id.clone(),
        plugin_version: record.plugin_version.clone(),
        manifest_hash: record.manifest_hash.clone(),
        sandbox_required: record.sandbox_required,
        status: record.status.clone(),
    }
}

fn storage_host_error(error: impl std::fmt::Display) -> HostError {
    HostError::Spawn(format!("storage pluginu: {error}"))
}

fn manifest_hash(manifest: &ToolManifest) -> Result<String, HostError> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| HostError::ManifestMismatch(error.to_string()))?;
    let digest = Sha256::digest(bytes);
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn policy_from_record(record: &PluginRegistrationRecord) -> PluginLaunchPolicy {
    PluginLaunchPolicy {
        sandbox: if record.sandbox_required {
            SandboxRequirement::Required
        } else {
            SandboxRequirement::NotRequired
        },
        sandbox_executable: record.sandbox_executable.as_ref().map(PathBuf::from),
        working_dir: Some(PathBuf::from(&record.working_dir)),
        environment: record.environment.clone(),
        request_timeout_ms: record.request_timeout_ms,
        max_frame_bytes: record.max_frame_bytes,
        max_restart_attempts: record.max_restart_attempts,
    }
}

fn validate_persisted_launch_spec(
    record: &PluginRegistrationRecord,
    manifest: &ToolManifest,
) -> Result<(), HostError> {
    if record.plugin_id != manifest.descriptor.id.as_str()
        || record.plugin_version != manifest.tool_version
        || manifest_hash(manifest)? != record.manifest_hash
    {
        return Err(HostError::StaleLaunchSpec(
            "manifest, wersja lub hash ToolManifest nie pasują".into(),
        ));
    }
    if launch_spec_hash(record)? != record.launch_spec_hash {
        return Err(HostError::StaleLaunchSpec(
            "hash pełnego launch spec nie pasuje".into(),
        ));
    }
    let policy = policy_from_record(record);
    validate_launch_policy(&policy)?;
    let (path, hash) = validate_executable(&record.command)?;
    if path.display().to_string() != record.executable_path || hash != record.executable_hash {
        return Err(HostError::StaleLaunchSpec(
            "executable zmienił się albo nie pasuje do zapisanego identity".into(),
        ));
    }
    if record.command.first().map(Path::new) != Some(path.as_path()) {
        return Err(HostError::StaleLaunchSpec(
            "komenda nie wskazuje na zapisany executable".into(),
        ));
    }
    Ok(())
}

fn normalize_launch_policy(
    mut policy: PluginLaunchPolicy,
) -> Result<PluginLaunchPolicy, HostError> {
    if policy.working_dir.is_none() {
        policy.working_dir = Some(
            std::env::current_dir()
                .map_err(|error| HostError::InvalidLaunchPolicy(error.to_string()))?,
        );
    }
    if matches!(policy.sandbox, SandboxRequirement::Required) && policy.sandbox_executable.is_none()
    {
        policy.sandbox_executable =
            BubblewrapSandboxRunner::detect().map(|runner| runner.executable);
    }
    validate_launch_policy(&policy)?;
    Ok(policy)
}

fn validate_executable(command: &[String]) -> Result<(PathBuf, String), HostError> {
    let raw = command
        .first()
        .ok_or_else(|| HostError::Spawn("pusta komenda pluginu".into()))?;
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(HostError::UnsafeExecutable(
            "ścieżka executable musi być absolutna".into(),
        ));
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| HostError::UnsafeExecutable(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(HostError::UnsafeExecutable(
            "executable nie może być symlinkiem i musi być zwykłym plikiem".into(),
        ));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(HostError::UnsafeExecutable(
            "executable jest zapisywalny przez grupę lub innych użytkowników".into(),
        ));
    }
    let hash = file_sha256(&path)?;
    Ok((path, hash))
}

fn file_sha256(path: &Path) -> Result<String, HostError> {
    let mut file =
        File::open(path).map_err(|error| HostError::UnsafeExecutable(error.to_string()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| HostError::UnsafeExecutable(error.to_string()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let digest = digest.finalize();
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn launch_spec_hash(record: &PluginRegistrationRecord) -> Result<String, HostError> {
    let canonical = (
        &record.plugin_id,
        &record.plugin_version,
        &record.manifest_hash,
        &record.command,
        &record.executable_path,
        &record.executable_hash,
        &record.working_dir,
        &record.environment,
        record.sandbox_required,
        &record.sandbox_executable,
        record.request_timeout_ms,
        record.max_frame_bytes,
        record.max_restart_attempts,
    );
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| HostError::StaleLaunchSpec(error.to_string()))?;
    let digest = Sha256::digest(bytes);
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

impl PluginToolExecutor {
    pub async fn spawn(
        command: Vec<String>,
        local_manifest: ToolManifest,
    ) -> Result<Self, HostError> {
        Self::spawn_with_policy(command, local_manifest, PluginLaunchPolicy::default(), None).await
    }

    pub async fn spawn_with_policy(
        command: Vec<String>,
        local_manifest: ToolManifest,
        launch_policy: PluginLaunchPolicy,
        audit: Option<Arc<dyn AuditSink>>,
    ) -> Result<Self, HostError> {
        let launch_policy = normalize_launch_policy(launch_policy)?;
        if command.is_empty() {
            return Err(HostError::Spawn("pusta komenda pluginu".into()));
        }
        local_manifest
            .validate()
            .map_err(|e| HostError::ManifestMismatch(e.to_string()))?;
        validate_executable(&command)?;
        validate_launch_policy(&launch_policy)?;
        let connection = spawn_and_handshake(&command, &local_manifest, &launch_policy).await?;
        let tool_id = local_manifest.descriptor.id.clone();
        let request_timeout_ms = local_manifest
            .descriptor
            .default_timeout_ms
            .min(DEFAULT_REQUEST_TIMEOUT_MS);
        Ok(Self {
            tool_id,
            local_manifest: Arc::new(local_manifest),
            command: Arc::new(command),
            connection: Arc::new(Mutex::new(connection)),
            request_timeout_ms: request_timeout_ms.min(launch_policy.request_timeout_ms),
            restart_attempts: Arc::new(Mutex::new(0)),
            max_restart_attempts: launch_policy.max_restart_attempts,
            audit,
            launch_policy,
        })
    }

    async fn restart(&self) -> Result<(), HostError> {
        let mut old = self.connection.lock().await;
        old.terminate().await;
        let connection =
            spawn_and_handshake(&self.command, &self.local_manifest, &self.launch_policy).await?;
        *old = connection;
        Ok(())
    }

    /// Wysyła heartbeat bez uruchamiania narzędzia. Host może użyć go do
    /// sprawdzenia procesu przed pokazaniem go jako online.
    pub async fn heartbeat(&self, nonce: impl Into<String>) -> Result<(), HostError> {
        let nonce = nonce.into();
        let mut connection = self.connection.lock().await;
        let result = async {
            connection
                .send(&WireMessage::Heartbeat {
                    nonce: nonce.clone(),
                })
                .await?;
            match connection.receive(DEFAULT_HEARTBEAT_TIMEOUT_MS).await? {
                WireMessage::HeartbeatAck { nonce: received } if received == nonce => Ok(()),
                other => Err(HostError::Handshake(format!(
                    "heartbeat zwrócił {:?}",
                    other.kind()
                ))),
            }
        }
        .await;
        if result.is_err() {
            connection.terminate().await;
        }
        result
    }

    fn record_restart(&self, attempt: u32, reason: &str) -> Result<(), ToolError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        audit.record(ToolAuditEvent {
            task_id: format!("plugin-host:{}", self.tool_id),
            tool: self.tool_id.clone(),
            tool_version: self.local_manifest.tool_version.clone(),
            plan_hash: "plugin-host-restart".into(),
            started_at_unix_ms: 0,
            finished_at_unix_ms: 0,
            outcome: AuditOutcome::Failed,
            summary: format!("restart attempt {attempt}: {reason}"),
        })
    }
}

async fn spawn_and_handshake(
    command: &[String],
    local: &ToolManifest,
    launch_policy: &PluginLaunchPolicy,
) -> Result<ProcessConnection, HostError> {
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process.env_clear();
    process.envs(&launch_policy.environment);
    if let Some(dir) = &launch_policy.working_dir {
        process.current_dir(dir);
    }
    if matches!(launch_policy.sandbox, SandboxRequirement::Required) {
        let runner = launch_policy
            .sandbox_executable
            .as_ref()
            .map(|path| BubblewrapSandboxRunner::at(path.clone()))
            .or_else(BubblewrapSandboxRunner::detect)
            .ok_or(HostError::SandboxUnavailable)?;
        runner.wrap_command(&mut process, &command[0], &command[1..], launch_policy)?;
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        process.env_clear();
        process.envs(&launch_policy.environment);
    }
    let mut child = process
        .spawn()
        .map_err(|e| HostError::Spawn(e.to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HostError::Spawn("brak stdin pluginu".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HostError::Spawn("brak stdout pluginu".into()))?;
    let mut connection = ProcessConnection {
        child,
        stdin: BufWriter::new(stdin),
        stdout: BufReader::new(stdout),
        max_frame_bytes: launch_policy.max_frame_bytes,
    };
    let hello = match connection.receive(DEFAULT_HEARTBEAT_TIMEOUT_MS).await {
        Ok(message) => message,
        Err(error) => {
            connection.terminate().await;
            return Err(error);
        }
    };
    let WireMessage::Hello(Hello { manifest }) = hello else {
        connection.terminate().await;
        return Err(HostError::Handshake(
            "pierwsza wiadomość nie jest Hello".into(),
        ));
    };
    if let Err(error) = validate_manifest(&manifest, local) {
        connection.terminate().await;
        return Err(error);
    }
    if let Err(error) = connection
        .send(&WireMessage::HelloAck {
            protocol_version: PROTOCOL_VERSION,
        })
        .await
    {
        connection.terminate().await;
        return Err(error);
    }
    Ok(connection)
}

fn validate_launch_policy(policy: &PluginLaunchPolicy) -> Result<(), HostError> {
    if policy.environment.keys().any(|key| {
        let upper = key.to_ascii_uppercase();
        key.is_empty()
            || key.contains('=')
            || upper.starts_with("LD_")
            || upper.contains("PROXY")
            || [
                "SECRET",
                "TOKEN",
                "PASSWORD",
                "CREDENTIAL",
                "PRIVATE_KEY",
                "AUTH",
            ]
            .iter()
            .any(|word| upper.contains(word))
    }) {
        return Err(HostError::InvalidLaunchPolicy(
            "środowisko pluginu zawiera niedozwoloną nazwę".into(),
        ));
    }
    let dir = policy
        .working_dir
        .as_ref()
        .ok_or_else(|| HostError::InvalidLaunchPolicy("katalog roboczy musi być jawny".into()))?;
    if !dir.is_absolute() {
        return Err(HostError::InvalidLaunchPolicy(
            "katalog roboczy musi być absolutny".into(),
        ));
    }
    let metadata = fs::symlink_metadata(dir).map_err(|_| {
        HostError::InvalidLaunchPolicy("katalog roboczy pluginu nie istnieje".into())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HostError::InvalidLaunchPolicy(
            "katalog roboczy nie może być symlinkiem i musi być katalogiem".into(),
        ));
    }
    if policy.request_timeout_ms == 0 || policy.request_timeout_ms > DEFAULT_REQUEST_TIMEOUT_MS {
        return Err(HostError::InvalidLaunchPolicy(
            "timeout pluginu jest poza dozwolonym zakresem".into(),
        ));
    }
    if policy.max_frame_bytes == 0 || policy.max_frame_bytes > MAX_FRAME_BYTES {
        return Err(HostError::InvalidLaunchPolicy(
            "limit ramki pluginu jest poza dozwolonym zakresem".into(),
        ));
    }
    if policy.max_restart_attempts > 8 {
        return Err(HostError::InvalidLaunchPolicy(
            "limit restartów pluginu jest zbyt wysoki".into(),
        ));
    }
    if let Some(sandbox) = &policy.sandbox_executable {
        let metadata = fs::symlink_metadata(sandbox).map_err(|_| HostError::SandboxUnavailable)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(HostError::SandboxUnavailable);
        }
    }
    Ok(())
}

fn validate_manifest(remote: &PluginManifest, local: &ToolManifest) -> Result<(), HostError> {
    if remote.protocol_version != PROTOCOL_VERSION {
        return Err(HostError::Handshake(format!(
            "wersja {} zamiast {}",
            remote.protocol_version, PROTOCOL_VERSION
        )));
    }
    let id = local.descriptor.id.as_str();
    let Some(binding) = remote.tools.iter().find(|tool| tool.id == id) else {
        return Err(HostError::ManifestMismatch(format!(
            "plugin nie deklaruje narzędzia {id}"
        )));
    };
    if binding.schema_version != local.descriptor.schema_version
        || binding.input_schema != local.descriptor.input_schema
        || binding.output_schema != local.descriptor.output_schema
    {
        return Err(HostError::ManifestMismatch(
            "schemat pluginu różni się od lokalnego".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl ToolExecutor for PluginToolExecutor {
    fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }

    async fn execute(
        &self,
        args: Value,
        started_at_unix_ms: UnixMs,
    ) -> Result<ToolExecution, ToolError> {
        let mut restart_attempts = self.restart_attempts.lock().await;
        for attempt in 0..=self.max_restart_attempts {
            let request_id = format!("plugin-{started_at_unix_ms}-{attempt}");
            let result = async {
                let mut connection = self.connection.lock().await;
                connection
                    .send(&WireMessage::Request(Request {
                        request_id: request_id.clone(),
                        tool_id: self.tool_id.to_string(),
                        args: args.clone(),
                    }))
                    .await
                    .map_err(|e| e.to_string())?;
                match connection
                    .receive(self.request_timeout_ms)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    WireMessage::Response(Response {
                        request_id: response_id,
                        output,
                        summary,
                        finished_at_unix_ms,
                    }) if response_id == request_id => Ok(ToolExecution {
                        output,
                        summary,
                        finished_at_unix_ms,
                    }),
                    WireMessage::Error(error) => Err(format!("{}: {}", error.code, error.message)),
                    other => Err(format!("nieoczekiwana odpowiedź {:?}", other.kind())),
                }
            }
            .await;
            match result {
                Ok(value) => {
                    *restart_attempts = 0;
                    return Ok(value);
                }
                Err(error) if attempt < self.max_restart_attempts => {
                    *restart_attempts = restart_attempts.saturating_add(1);
                    let restart_number = *restart_attempts;
                    self.record_restart(restart_number, &error)?;
                    drop(restart_attempts);
                    self.restart().await.map_err(|e| ToolError::Execution {
                        tool: self.tool_id.to_string(),
                        reason: e.to_string(),
                    })?;
                    restart_attempts = self.restart_attempts.lock().await;
                }
                Err(error) => {
                    self.connection.lock().await.terminate().await;
                    return Err(ToolError::Execution {
                        tool: self.tool_id.to_string(),
                        reason: error,
                    });
                }
            }
        }
        unreachable!()
    }
}

impl Drop for PluginToolExecutor {
    fn drop(&mut self) {
        if let Ok(mut connection) = self.connection.try_lock() {
            let _ = connection.child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use victor_domain::{Capability, PathScope, RiskLevel, ToolDescriptor, ToolId};

    fn manifest() -> ToolManifest {
        ToolManifest::new(
            "1.0.0",
            ToolDescriptor {
                id: ToolId::parse("plugin_sysinfo").unwrap(),
                schema_version: 1,
                description: "CPU RAM".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                capabilities: vec![],
                risk: RiskLevel::R0,
                default_timeout_ms: 2_000,
                undo_support: false,
                path_arguments: vec![],
            },
        )
    }

    #[tokio::test]
    async fn rejects_remote_capability_claim_by_schema_mismatch() {
        let mut local = manifest();
        local.descriptor.capabilities = vec![Capability::FilesystemRead {
            scope: PathScope::new("/tmp"),
        }];
        let remote = PluginManifest {
            protocol_version: PROTOCOL_VERSION,
            plugin_id: "plugin_sysinfo".into(),
            plugin_version: "1".into(),
            tools: vec![victor_plugin_contract::PluginToolBinding {
                id: "plugin_sysinfo".into(),
                schema_version: 1,
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
            }],
        };
        assert!(validate_manifest(&remote, &local).is_ok());
        assert_eq!(local.descriptor.risk, RiskLevel::R0);
    }
}
