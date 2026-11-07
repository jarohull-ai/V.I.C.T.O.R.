//! Bezpieczny, lokalny daemon składający trwały storage, API i Unix transport.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use victor_context::ContextItemId;
use victor_conversation::{
    ConversationEngine, ConversationLimits, ConversationRouter, ConversationService,
};
use victor_domain::ToolId;
use victor_integrations::{
    AudioFormat, SpeechOutput, SpeechRequest, WhisperCppConfig, WhisperCppSpeechToText,
};
use victor_model_local::{LocalModel, LocalModelConfig};
use victor_plugin_host::{PluginLaunchPolicy, PluginLaunchRequest, PluginLifecycleApi};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::{SttJob, SttRuntime, ToolRuntime};
use victor_skill_api::SkillApi;
use victor_skill_transport::{UnixSkillServer, VoiceService};
use victor_storage::SqliteContextStore;
use victor_tools::{AuditOutcome, AuditSink, ToolAuditEvent, ToolManifest, ToolRegistry};
use victor_tts_local::{LocalTts, LocalTtsConfig};

const DEFAULT_SCOPE: &str = "local";

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("nieprawidłowa konfiguracja daemonu: {0}")]
    Config(String),
    #[error("błąd pliku: {0}")]
    Io(#[from] std::io::Error),
    #[error("błąd JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("błąd storage: {0}")]
    Storage(String),
    #[error("błąd polityki: {0}")]
    Policy(String),
    #[error("błąd hosta pluginów: {0}")]
    Host(String),
    #[error("błąd API skills: {0}")]
    Skill(String),
    #[error("błąd transportu: {0}")]
    Transport(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub sqlite_path: PathBuf,
    pub socket_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub trusted_definitions_dir: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
    pub token_file: PathBuf,
    pub scope: String,
    pub local_model_definition: PathBuf,
    pub local_stt_definition: PathBuf,
    pub local_tts_definition: Option<PathBuf>,
}

impl DaemonConfig {
    pub fn validate(&self) -> Result<(), DaemonError> {
        if self.scope.trim().is_empty() || self.scope.len() > 128 {
            return Err(DaemonError::Config("scope jest pusty lub za długi".into()));
        }
        for (label, path) in [
            ("sqlite", &self.sqlite_path),
            ("socket", &self.socket_path),
            ("runtime", &self.runtime_dir),
            ("trusted definitions", &self.trusted_definitions_dir),
        ] {
            if !path.is_absolute() {
                return Err(DaemonError::Config(format!("{label} musi być absolutny")));
            }
        }
        if !self.token_file.is_absolute() {
            return Err(DaemonError::Config("token file musi być absolutny".into()));
        }
        if !self.local_model_definition.is_absolute() {
            return Err(DaemonError::Config(
                "local model definition musi być absolutna".into(),
            ));
        }
        if !self.local_stt_definition.is_absolute() {
            return Err(DaemonError::Config(
                "local STT definition musi być absolutna".into(),
            ));
        }
        if self.allowed_roots.is_empty()
            || self.allowed_roots.iter().any(|path| !path.is_absolute())
        {
            return Err(DaemonError::Config(
                "wymagany jest co najmniej jeden absolutny allowed root".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPluginDefinition {
    pub id: ToolId,
    pub manifest: ToolManifest,
    pub launch: PluginLaunchRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedLocalModelSpec {
    pub command: Vec<String>,
    pub working_dir: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_restart_attempts: u32,
    pub sandbox_required: bool,
    pub sandbox_executable: Option<String>,
    pub executable_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedLocalModelDefinition {
    pub config_sha256: String,
    pub spec: TrustedLocalModelSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedLocalSttSpec {
    pub executable: String,
    pub model: String,
    pub language: Option<String>,
    pub threads: Option<u16>,
    pub timeout_ms: u64,
    pub max_wav_bytes: usize,
    pub working_dir: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub sandbox_required: bool,
    pub sandbox_executable: Option<String>,
    pub executable_sha256: String,
    pub model_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedLocalSttDefinition {
    pub config_sha256: String,
    pub spec: TrustedLocalSttSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedLocalTtsSpec {
    pub command: Vec<String>,
    pub voice: String,
    pub model: String,
    pub language: String,
    pub output_format: AudioFormat,
    pub min_speed: f32,
    pub max_speed: f32,
    pub working_dir: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_audio_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_restart_attempts: u32,
    pub sandbox_required: bool,
    pub sandbox_executable: Option<String>,
    pub executable_sha256: String,
    pub model_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedLocalTtsDefinition {
    pub config_sha256: String,
    pub spec: TrustedLocalTtsSpec,
}

struct DaemonVoiceService {
    stt: SttRuntime<WhisperCppSpeechToText>,
    conversation: Arc<dyn ConversationService>,
    tts: Option<Arc<LocalTts>>,
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl VoiceService for DaemonVoiceService {
    async fn transcribe_and_chat(
        &self,
        conversation_id: String,
        scope: String,
        audio: Vec<u8>,
        at_unix_ms: u64,
    ) -> Result<serde_json::Value, String> {
        let audio = victor_integrations::AudioChunk {
            bytes: audio,
            format: victor_integrations::AudioFormat::Wav,
            sample_rate_hz: 16_000,
            channels: 1,
        };
        let audio_hash = audio.sha256();
        let item_id = ContextItemId::parse(&format!("stt-{}", &audio_hash[..16]))
            .map_err(|e| e.to_string())?;
        let context_scope =
            victor_context::ContextScope::conversation(format!("{scope}.{conversation_id}"))
                .map_err(|e| e.to_string())?;
        let outcome = self
            .stt
            .transcribe(SttJob {
                item_id,
                scope: context_scope,
                audio,
                now_unix_ms: at_unix_ms,
            })
            .await
            .map_err(|e| e.to_string())?;
        let reply = self
            .conversation
            .chat(
                conversation_id.clone(),
                scope,
                outcome.transcript.text.clone(),
                at_unix_ms,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let response_text = match &reply {
            victor_conversation::ConversationOutcome::Reply { text }
            | victor_conversation::ConversationOutcome::Executed { text, .. }
            | victor_conversation::ConversationOutcome::ApprovalRequired { text, .. }
            | victor_conversation::ConversationOutcome::Denied { text, .. } => text,
        };
        let tts = if let Some(tts) = &self.tts {
            match tts
                .synthesize(SpeechRequest::new(response_text, "pl"))
                .await
            {
                Ok(audio) => serde_json::json!({
                    "status": "synthesized", "provider": tts.provider(),
                    "format": audio.format, "sample_rate_hz": audio.sample_rate_hz,
                    "cache_key": audio.cache_key, "bytes": audio.bytes
                }),
                Err(error) => serde_json::json!({
                    "status": "error", "provider": tts.provider(), "error": error.to_string()
                }),
            }
        } else {
            serde_json::json!({"status": "disabled"})
        };
        let tts_status = tts
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error");
        let audit_outcome = match tts_status {
            "synthesized" => AuditOutcome::Succeeded,
            "disabled" => AuditOutcome::Denied,
            _ => AuditOutcome::Failed,
        };
        let audit_tool = ToolId::parse("tts.local").map_err(|e| e.to_string())?;
        self.audit
            .record(ToolAuditEvent {
                task_id: format!("voice:{conversation_id}"),
                tool: audit_tool,
                tool_version: "1".into(),
                plan_hash: format!("audio:{audio_hash}"),
                started_at_unix_ms: at_unix_ms,
                finished_at_unix_ms: at_unix_ms,
                outcome: audit_outcome,
                summary: format!("local TTS: {tts_status}"),
            })
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "transcript": outcome.transcript.text,
            "provider": outcome.transcript.provider,
            "location": outcome.transcript.location,
            "audio_sha256": audio_hash,
            "conversation": reply,
            "text": response_text,
            "tts": tts
        }))
    }
}

pub struct Daemon {
    config: DaemonConfig,
    server: Option<UnixSkillServer>,
}

impl Daemon {
    pub async fn prepare(config: DaemonConfig) -> Result<Self, DaemonError> {
        config.validate()?;
        prepare_runtime(&config.runtime_dir)?;
        ensure_parent_safe(&config.socket_path, &config.runtime_dir, "socket")?;
        ensure_database_path(&config.sqlite_path)?;
        ensure_directory(&config.trusted_definitions_dir)?;
        for root in &config.allowed_roots {
            ensure_allowed_root(root)?;
        }
        let token = load_token(&config)?;
        let definitions = load_definitions(&config.trusted_definitions_dir)?;
        let model_definition = load_local_model_definition(&config)?;
        let stt_definition = load_local_stt_definition(&config)?;
        let tts_definition = config
            .local_tts_definition
            .as_ref()
            .map(|_| load_local_tts_definition(&config))
            .transpose()?;

        let store = Arc::new(
            SqliteContextStore::open(&config.sqlite_path)
                .map_err(|e| DaemonError::Storage(e.to_string()))?,
        );
        let mut registry = ToolRegistry::new();
        let mut runtime_registry = ToolRegistry::new();
        let mut enabled = Vec::new();
        for definition in &definitions {
            registry
                .register(definition.manifest.clone())
                .map_err(|e| DaemonError::Config(e.to_string()))?;
            runtime_registry
                .register(definition.manifest.clone())
                .map_err(|e| DaemonError::Config(e.to_string()))?;
            enabled.push(definition.id.clone());
        }
        let policy = Arc::new(
            PolicyEngine::new(
                PolicyConfig::new(config.allowed_roots.clone()).with_enabled_tools(enabled),
            )
            .map_err(|e| DaemonError::Policy(e.to_string()))?,
        );
        let skill_api = SkillApi::new(Arc::new(registry), policy.clone(), store.clone())
            .map_err(|e| DaemonError::Skill(e.to_string()))?;
        let mut plugin_api =
            PluginLifecycleApi::open(store.clone(), policy.clone(), Some(store.clone()))
                .await
                .map_err(|e| DaemonError::Host(e.to_string()))?;
        for definition in &definitions {
            plugin_api = plugin_api
                .with_plugin_definition(
                    definition.manifest.clone(),
                    definition.launch.command.clone(),
                    launch_policy(&definition.launch)?,
                )
                .map_err(|e| DaemonError::Host(e.to_string()))?;
        }
        let existing_plugins = plugin_api
            .list()
            .map_err(|e| DaemonError::Host(e.to_string()))?;
        for definition in &definitions {
            let already_registered = existing_plugins
                .iter()
                .any(|summary| summary.plugin_id == definition.id.as_str());
            if !already_registered {
                plugin_api
                    .register(
                        definition.manifest.clone(),
                        definition.launch.clone(),
                        None,
                        "daemon".into(),
                        "trusted startup".into(),
                        0,
                    )
                    .await
                    .map_err(|e| DaemonError::Host(e.to_string()))?;
            }
        }
        let plugin_api = Arc::new(Mutex::new(plugin_api));
        let runtime_registry = Arc::new(runtime_registry);
        let stt = SttRuntime::new(
            WhisperCppSpeechToText::new(stt_definition_to_config(&stt_definition)?)
                .map_err(|e| DaemonError::Config(e.to_string()))?,
            policy.clone(),
            store.clone(),
            Default::default(),
        );
        let runtime = Arc::new(
            ToolRuntime::new(
                runtime_registry.clone(),
                policy,
                store.clone(),
                store.clone(),
            )
            .with_transactional_store(store.clone()),
        );
        let default_path = definitions.iter().find_map(|definition| {
            definition
                .launch
                .command
                .windows(2)
                .find(|pair| pair[0] == "--root")
                .map(|pair| pair[1].clone())
        });
        let local_model = LocalModel::new(model_definition_to_config(&model_definition)?)
            .map_err(|e| DaemonError::Config(e.to_string()))?;
        let router = ConversationRouter::new(
            Arc::new(local_model),
            runtime,
            runtime_registry,
            plugin_api.clone(),
        );
        let router = match default_path {
            Some(path) => router.with_default_path(path),
            None => router,
        };
        let conversation: Arc<dyn ConversationService> = Arc::new(ConversationEngine::new(
            router,
            store.clone(),
            ConversationLimits::default(),
        ));
        let voice = Arc::new(DaemonVoiceService {
            stt,
            conversation: conversation.clone(),
            tts: tts_definition
                .as_ref()
                .map(tts_definition_to_config)
                .transpose()?
                .map(LocalTts::new)
                .transpose()
                .map_err(|e| DaemonError::Config(e.to_string()))?
                .map(Arc::new),
            audit: store.clone(),
        });
        let server = UnixSkillServer::new(&config.socket_path, token, &config.scope, skill_api)
            .map_err(|e| DaemonError::Transport(e.to_string()))?
            .with_audio_limit(stt_definition.spec.max_wav_bytes)
            .map_err(|e| DaemonError::Transport(e.to_string()))?
            .with_plugin_api_shared(plugin_api)
            .with_conversation(conversation)
            .with_voice(voice)
            .with_idempotency_store(store);
        Ok(Self {
            config,
            server: Some(server),
        })
    }

    pub async fn run(mut self) -> Result<(), DaemonError> {
        let server = self
            .server
            .take()
            .ok_or_else(|| DaemonError::Config("daemon już uruchomiony".into()))?;
        let (tx, rx) = oneshot::channel();
        let mut task = tokio::spawn(server.run(rx));
        tokio::select! {
            result = &mut task => {
                let result = result.map_err(|e| DaemonError::Transport(e.to_string()))?;
                result.map_err(|e| DaemonError::Transport(e.to_string()))
            },
            signal = shutdown_signal() => {
                signal?;
                let _ = tx.send(());
                let result = task.await.map_err(|e| DaemonError::Transport(e.to_string()))?;
                result.map_err(|e| DaemonError::Transport(e.to_string()))
            }
        }
    }

    /// Uruchamia daemon z kontrolowanym sygnałem shutdown; używane także przez
    /// testy procesu transportowego bez wysyłania sygnału do całego test runnera.
    pub async fn run_with_shutdown(
        mut self,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DaemonError> {
        let server = self
            .server
            .take()
            .ok_or_else(|| DaemonError::Config("daemon już uruchomiony".into()))?;
        server
            .run(shutdown)
            .await
            .map_err(|error| DaemonError::Transport(error.to_string()))
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }
}

async fn shutdown_signal() -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(DaemonError::Io)?;
        tokio::select! { result = tokio::signal::ctrl_c() => result.map_err(DaemonError::Io), _ = term.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(DaemonError::Io)
    }
}

fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn ensure_directory(path: &Path) -> Result<(), DaemonError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| DaemonError::Config(format!("{}: {e}", path.display())))?;
    if !meta.is_dir() || meta.uid() != uid() || meta.permissions().mode() & 0o077 != 0 {
        return Err(DaemonError::Config(format!(
            "niebezpieczny katalog: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_allowed_root(path: &Path) -> Result<(), DaemonError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| DaemonError::Config(format!("{}: {e}", path.display())))?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != uid()
        || meta.permissions().mode() & 0o022 != 0
    {
        return Err(DaemonError::Config(format!(
            "niebezpieczny allowed root: {}",
            path.display()
        )));
    }
    Ok(())
}

fn prepare_runtime(path: &Path) -> Result<(), DaemonError> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    ensure_directory(path)
}

fn ensure_parent_safe(path: &Path, runtime: &Path, label: &str) -> Result<(), DaemonError> {
    let parent = path
        .parent()
        .ok_or_else(|| DaemonError::Config(format!("brak katalogu {label}")))?;
    if parent != runtime {
        return Err(DaemonError::Config(format!(
            "{label} musi być w runtime dir"
        )));
    }
    ensure_directory(parent)
}

fn ensure_database_path(path: &Path) -> Result<(), DaemonError> {
    let parent = path
        .parent()
        .ok_or_else(|| DaemonError::Config("brak katalogu bazy".into()))?;
    let parent_meta = fs::symlink_metadata(parent)
        .map_err(|e| DaemonError::Config(format!("{}: {e}", parent.display())))?;
    if !parent_meta.is_dir() || parent_meta.file_type().is_symlink() || parent_meta.uid() != uid() {
        return Err(DaemonError::Config(format!(
            "niebezpieczny katalog bazy: {}",
            parent.display()
        )));
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.is_file() || meta.uid() != uid() || meta.permissions().mode() & 0o022 != 0 {
            return Err(DaemonError::Config("niebezpieczny plik SQLite".into()));
        }
    }
    Ok(())
}

fn load_token(config: &DaemonConfig) -> Result<String, DaemonError> {
    let meta = fs::symlink_metadata(&config.token_file)?;
    if !meta.is_file() || meta.uid() != uid() || meta.permissions().mode() & 0o077 != 0 {
        return Err(DaemonError::Config(
            "token file musi być prywatny 0600".into(),
        ));
    }
    let raw = fs::read_to_string(&config.token_file)?;
    let token = raw.trim();
    if token.len() < 16 || token.len() > 4096 || token.chars().any(char::is_whitespace) {
        return Err(DaemonError::Config("token ma nieprawidłowy format".into()));
    }
    Ok(token.to_owned())
}

fn load_definitions(dir: &Path) -> Result<Vec<TrustedPluginDefinition>, DaemonError> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    let mut result = Vec::new();
    for entry in entries {
        let path = entry.path();
        if matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("local-model.json" | "local-stt.json" | "local-tts.json")
        ) {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(DaemonError::Config(format!(
                "symlink trusted definition: {}",
                path.display()
            )));
        }
        if !meta.is_file() || meta.uid() != uid() || meta.permissions().mode() & 0o022 != 0 {
            return Err(DaemonError::Config(format!(
                "niebezpieczna trusted definition: {}",
                path.display()
            )));
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let definition: TrustedPluginDefinition = serde_json::from_reader(File::open(&path)?)?;
        if definition.id != definition.manifest.descriptor.id {
            return Err(DaemonError::Config("id nie pasuje do manifestu".into()));
        }
        definition
            .manifest
            .validate()
            .map_err(|e| DaemonError::Config(e.to_string()))?;
        if definition
            .launch
            .command
            .first()
            .map(Path::new)
            .is_none_or(|p| !p.is_absolute())
        {
            return Err(DaemonError::Config(
                "executable trusted definition musi być absolutny".into(),
            ));
        }
        result.push(definition);
    }
    Ok(result)
}

fn launch_policy(request: &PluginLaunchRequest) -> Result<PluginLaunchPolicy, DaemonError> {
    Ok(PluginLaunchPolicy {
        sandbox: request.sandbox.clone(),
        sandbox_executable: request.sandbox_executable.as_ref().map(PathBuf::from),
        working_dir: Some(PathBuf::from(&request.working_dir)),
        environment: request.environment.clone(),
        request_timeout_ms: request.request_timeout_ms,
        max_frame_bytes: request.max_frame_bytes,
        max_restart_attempts: request.max_restart_attempts,
    })
}

fn load_local_model_definition(
    config: &DaemonConfig,
) -> Result<TrustedLocalModelDefinition, DaemonError> {
    let meta = fs::symlink_metadata(&config.local_model_definition)?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != uid()
        || meta.permissions().mode() & 0o022 != 0
    {
        return Err(DaemonError::Config(
            "niebezpieczna definicja lokalnego modelu".into(),
        ));
    }
    if config.local_model_definition.parent() != Some(config.trusted_definitions_dir.as_path()) {
        return Err(DaemonError::Config(
            "definicja lokalnego modelu musi być w trusted-definitions".into(),
        ));
    }
    let definition: TrustedLocalModelDefinition =
        serde_json::from_reader(File::open(&config.local_model_definition)?)?;
    let spec_bytes = serde_json::to_vec(&definition.spec)?;
    if hash_bytes(&spec_bytes) != definition.config_sha256 {
        return Err(DaemonError::Config(
            "hash trusted config lokalnego modelu nie pasuje".into(),
        ));
    }
    let executable = definition
        .spec
        .command
        .first()
        .ok_or_else(|| DaemonError::Config("brak executable lokalnego modelu".into()))?;
    let executable = Path::new(executable);
    if !executable.is_absolute() || !safe_owned_file(executable) {
        return Err(DaemonError::Config(
            "niebezpieczny executable lokalnego modelu".into(),
        ));
    }
    if hash_file(executable)? != definition.spec.executable_sha256 {
        return Err(DaemonError::Config(
            "hash executable lokalnego modelu nie pasuje".into(),
        ));
    }
    let working_dir = Path::new(&definition.spec.working_dir);
    if !working_dir.is_absolute() || !safe_owned_directory(working_dir) {
        return Err(DaemonError::Config(
            "niebezpieczny working directory lokalnego modelu".into(),
        ));
    }
    if definition.spec.environment.keys().any(|key| {
        let key = key.to_ascii_lowercase();
        ["secret", "token", "password", "api_key", "private_key"]
            .iter()
            .any(|marker| key.contains(marker))
    }) {
        return Err(DaemonError::Config(
            "środowisko lokalnego modelu zawiera nazwę sekretu".into(),
        ));
    }
    Ok(definition)
}

fn load_local_stt_definition(
    config: &DaemonConfig,
) -> Result<TrustedLocalSttDefinition, DaemonError> {
    let path = &config.local_stt_definition;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != uid()
        || meta.permissions().mode() & 0o022 != 0
        || path.parent() != Some(config.trusted_definitions_dir.as_path())
    {
        return Err(DaemonError::Config(
            "niebezpieczna definicja lokalnego STT".into(),
        ));
    }
    let definition: TrustedLocalSttDefinition = serde_json::from_reader(File::open(path)?)?;
    if hash_bytes(&serde_json::to_vec(&definition.spec)?) != definition.config_sha256 {
        return Err(DaemonError::Config(
            "hash trusted config lokalnego STT nie pasuje".into(),
        ));
    }
    let executable = Path::new(&definition.spec.executable);
    let model = Path::new(&definition.spec.model);
    let working_dir = Path::new(&definition.spec.working_dir);
    if !executable.is_absolute()
        || !model.is_absolute()
        || !working_dir.is_absolute()
        || !safe_owned_file(executable)
        || !safe_owned_file(model)
        || !safe_owned_directory(working_dir)
        || hash_file(executable)? != definition.spec.executable_sha256
        || hash_file(model)? != definition.spec.model_sha256
        || definition.spec.timeout_ms == 0
        || definition.spec.max_wav_bytes == 0
    {
        return Err(DaemonError::Config(
            "niebezpieczna lub niekompletna konfiguracja lokalnego STT".into(),
        ));
    }
    if definition.spec.environment.keys().any(|key| {
        let key = key.to_ascii_lowercase();
        ["secret", "token", "password", "api_key", "private_key"]
            .iter()
            .any(|marker| key.contains(marker))
    }) {
        return Err(DaemonError::Config(
            "środowisko lokalnego STT zawiera nazwę sekretu".into(),
        ));
    }
    if definition.spec.sandbox_required {
        let sandbox = definition
            .spec
            .sandbox_executable
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| DaemonError::Config("STT wymaga jawnego sandbox executable".into()))?;
        if !sandbox.is_absolute() || !safe_owned_file(sandbox) {
            return Err(DaemonError::Config("niebezpieczny sandbox STT".into()));
        }
    }
    Ok(definition)
}

fn load_local_tts_definition(
    config: &DaemonConfig,
) -> Result<TrustedLocalTtsDefinition, DaemonError> {
    let path = config
        .local_tts_definition
        .as_ref()
        .ok_or_else(|| DaemonError::Config("lokalny TTS nie jest skonfigurowany".into()))?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != uid()
        || meta.permissions().mode() & 0o022 != 0
        || path.parent() != Some(config.trusted_definitions_dir.as_path())
    {
        return Err(DaemonError::Config(
            "niebezpieczna definicja lokalnego TTS".into(),
        ));
    }
    let definition: TrustedLocalTtsDefinition = serde_json::from_reader(File::open(path)?)?;
    if hash_bytes(&serde_json::to_vec(&definition.spec)?) != definition.config_sha256 {
        return Err(DaemonError::Config(
            "hash trusted config lokalnego TTS nie pasuje".into(),
        ));
    }
    let executable = definition
        .spec
        .command
        .first()
        .map(Path::new)
        .ok_or_else(|| DaemonError::Config("brak executable lokalnego TTS".into()))?;
    let model = Path::new(&definition.spec.model);
    let working_dir = Path::new(&definition.spec.working_dir);
    if !executable.is_absolute()
        || !model.is_absolute()
        || !working_dir.is_absolute()
        || !safe_owned_file(executable)
        || !safe_owned_file(model)
        || !safe_owned_directory(working_dir)
        || hash_file(executable)? != definition.spec.executable_sha256
        || hash_file(model)? != definition.spec.model_sha256
        || definition.spec.timeout_ms == 0
        || definition.spec.max_frame_bytes < 128
        || definition.spec.max_audio_bytes == 0
        || definition.spec.max_stderr_bytes == 0
    {
        return Err(DaemonError::Config(
            "niebezpieczna konfiguracja lokalnego TTS".into(),
        ));
    }
    if definition.spec.environment.keys().any(|key| {
        let key = key.to_ascii_lowercase();
        ["secret", "token", "password", "api_key", "private_key"]
            .iter()
            .any(|marker| key.contains(marker))
    }) {
        return Err(DaemonError::Config(
            "środowisko lokalnego TTS zawiera nazwę sekretu".into(),
        ));
    }
    if definition.spec.sandbox_required {
        let sandbox = definition
            .spec
            .sandbox_executable
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| DaemonError::Config("TTS wymaga sandbox executable".into()))?;
        if !sandbox.is_absolute() || !safe_owned_file(sandbox) {
            return Err(DaemonError::Config("niebezpieczny sandbox TTS".into()));
        }
    }
    Ok(definition)
}

fn stt_definition_to_config(
    definition: &TrustedLocalSttDefinition,
) -> Result<WhisperCppConfig, DaemonError> {
    let spec = &definition.spec;
    let mut config = WhisperCppConfig::new(&spec.executable, &spec.model)
        .with_timeout(std::time::Duration::from_millis(spec.timeout_ms))
        .with_working_dir(&spec.working_dir)
        .with_environment(spec.environment.clone())
        .with_max_audio_bytes(spec.max_wav_bytes);
    if let Some(language) = &spec.language {
        config = config.with_language(language.clone());
    }
    if let Some(threads) = spec.threads {
        config = config.with_threads(threads);
    }
    if spec.sandbox_required {
        config = config.with_sandbox(spec.sandbox_executable.clone().map(PathBuf::from));
    }
    Ok(config)
}

fn tts_definition_to_config(
    definition: &TrustedLocalTtsDefinition,
) -> Result<LocalTtsConfig, DaemonError> {
    let spec = &definition.spec;
    Ok(LocalTtsConfig {
        command: spec.command.clone(),
        voice: spec.voice.clone(),
        model: PathBuf::from(&spec.model),
        language: spec.language.clone(),
        output_format: spec.output_format,
        min_speed: spec.min_speed,
        max_speed: spec.max_speed,
        working_dir: PathBuf::from(&spec.working_dir),
        environment: spec.environment.clone(),
        timeout: std::time::Duration::from_millis(spec.timeout_ms),
        max_frame_bytes: spec.max_frame_bytes,
        max_audio_bytes: spec.max_audio_bytes,
        max_stderr_bytes: spec.max_stderr_bytes,
        max_restart_attempts: spec.max_restart_attempts,
        sandbox_required: spec.sandbox_required,
        sandbox_executable: spec.sandbox_executable.clone().map(PathBuf::from),
    })
}

fn model_definition_to_config(
    definition: &TrustedLocalModelDefinition,
) -> Result<LocalModelConfig, DaemonError> {
    let spec = &definition.spec;
    Ok(LocalModelConfig {
        command: spec.command.clone(),
        working_dir: PathBuf::from(&spec.working_dir),
        environment: spec.environment.clone(),
        timeout: std::time::Duration::from_millis(spec.timeout_ms),
        max_frame_bytes: spec.max_frame_bytes,
        max_stderr_bytes: spec.max_stderr_bytes,
        max_restart_attempts: spec.max_restart_attempts,
        sandbox_required: spec.sandbox_required,
        sandbox_executable: spec.sandbox_executable.clone().map(PathBuf::from),
    })
}

fn safe_owned_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| {
            meta.is_file()
                && !meta.file_type().is_symlink()
                && meta.uid() == uid()
                && meta.permissions().mode() & 0o022 == 0
        })
        .unwrap_or(false)
}

fn safe_owned_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| {
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == uid()
                && meta.permissions().mode() & 0o022 == 0
        })
        .unwrap_or(false)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn hash_file(path: &Path) -> Result<String, DaemonError> {
    Ok(hash_bytes(&fs::read(path)?))
}

pub fn parse_args(args: impl Iterator<Item = String>) -> Result<DaemonConfig, DaemonError> {
    let mut values = BTreeMap::new();
    let mut iter = args.skip(1);
    while let Some(arg) = iter.next() {
        let key = arg
            .strip_prefix("--")
            .ok_or_else(|| DaemonError::Config("argument musi zaczynać się od --".into()))?
            .to_owned();
        if !matches!(
            key.as_str(),
            "sqlite"
                | "socket"
                | "runtime-dir"
                | "trusted-definitions"
                | "token-file"
                | "allowed-root"
                | "local-model-definition"
                | "local-stt-definition"
                | "local-tts-definition"
                | "scope"
        ) {
            return Err(DaemonError::Config(format!("nieznany argument --{key}")));
        }
        let value = iter
            .next()
            .ok_or_else(|| DaemonError::Config(format!("brak wartości dla --{key}")))?;
        if value.starts_with("--") {
            return Err(DaemonError::Config(format!("brak wartości dla --{key}")));
        }
        values.insert(key, value);
    }
    let required = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| DaemonError::Config(format!("brak --{key}")))
    };
    Ok(DaemonConfig {
        sqlite_path: required("sqlite")?.into(),
        socket_path: required("socket")?.into(),
        runtime_dir: required("runtime-dir")?.into(),
        trusted_definitions_dir: required("trusted-definitions")?.into(),
        allowed_roots: vec![required("allowed-root")?.into()],
        token_file: required("token-file")?.into(),
        scope: values
            .get("scope")
            .cloned()
            .unwrap_or_else(|| DEFAULT_SCOPE.into()),
        local_model_definition: required("local-model-definition")?.into(),
        local_stt_definition: required("local-stt-definition")?.into(),
        local_tts_definition: values.get("local-tts-definition").map(PathBuf::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use tempfile::tempdir;

    #[test]
    fn token_source_is_explicit() {
        let root = tempdir().unwrap();
        let config = DaemonConfig {
            sqlite_path: root.path().join("db"),
            socket_path: root.path().join("run/sock"),
            runtime_dir: root.path().join("run"),
            trusted_definitions_dir: root.path().join("defs"),
            allowed_roots: vec![root.path().to_path_buf()],
            token_file: "token".into(),
            scope: "local".into(),
            local_model_definition: root.path().join("model.json"),
            local_stt_definition: root.path().join("stt.json"),
            local_tts_definition: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_insecure_allowed_root() {
        let root = tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();

        assert!(ensure_allowed_root(root.path()).is_err());
    }

    #[test]
    fn missing_local_model_definition_is_fail_closed() {
        let root = tempdir().unwrap();
        let config = DaemonConfig {
            sqlite_path: root.path().join("state.sqlite"),
            socket_path: root.path().join("run.sock"),
            runtime_dir: root.path().to_path_buf(),
            trusted_definitions_dir: root.path().to_path_buf(),
            allowed_roots: vec![root.path().to_path_buf()],
            token_file: root.path().join("token"),
            scope: "local".into(),
            local_model_definition: root.path().join("local-model.json"),
            local_stt_definition: root.path().join("local-stt.json"),
            local_tts_definition: None,
        };
        assert!(load_local_model_definition(&config).is_err());
    }

    #[test]
    fn missing_local_stt_definition_is_fail_closed() {
        let root = tempdir().unwrap();
        let config = DaemonConfig {
            sqlite_path: root.path().join("state.sqlite"),
            socket_path: root.path().join("run.sock"),
            runtime_dir: root.path().to_path_buf(),
            trusted_definitions_dir: root.path().to_path_buf(),
            allowed_roots: vec![root.path().to_path_buf()],
            token_file: root.path().join("token"),
            scope: "local".into(),
            local_model_definition: root.path().join("local-model.json"),
            local_stt_definition: root.path().join("local-stt.json"),
            local_tts_definition: None,
        };
        assert!(load_local_stt_definition(&config).is_err());
    }

    #[tokio::test]
    async fn prepares_with_private_token_and_empty_catalog() {
        let root = tempdir().unwrap();
        let runtime = root.path().join("run");
        let defs = root.path().join("definitions");
        fs::create_dir(&runtime).unwrap();
        fs::create_dir(&defs).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&defs, fs::Permissions::from_mode(0o700)).unwrap();
        let test_executable = root.path().join("model-exec");
        fs::copy(std::env::current_exe().unwrap(), &test_executable).unwrap();
        fs::set_permissions(&test_executable, fs::Permissions::from_mode(0o700)).unwrap();
        let spec = TrustedLocalModelSpec {
            command: vec![test_executable.display().to_string()],
            working_dir: root.path().display().to_string(),
            environment: BTreeMap::new(),
            timeout_ms: 1000,
            max_frame_bytes: 4096,
            max_stderr_bytes: 4096,
            max_restart_attempts: 0,
            sandbox_required: false,
            sandbox_executable: None,
            executable_sha256: hash_file(&test_executable).unwrap(),
        };
        let model_definition = TrustedLocalModelDefinition {
            config_sha256: hash_bytes(&serde_json::to_vec(&spec).unwrap()),
            spec,
        };
        let model_path = defs.join("local-model.json");
        fs::write(&model_path, serde_json::to_vec(&model_definition).unwrap()).unwrap();
        fs::set_permissions(&model_path, fs::Permissions::from_mode(0o600)).unwrap();
        let stt_spec = TrustedLocalSttSpec {
            executable: test_executable.display().to_string(),
            model: test_executable.display().to_string(),
            language: Some("pl".into()),
            threads: Some(1),
            timeout_ms: 1000,
            max_wav_bytes: 1024,
            working_dir: root.path().display().to_string(),
            environment: BTreeMap::new(),
            sandbox_required: false,
            sandbox_executable: None,
            executable_sha256: hash_file(&test_executable).unwrap(),
            model_sha256: hash_file(&test_executable).unwrap(),
        };
        let stt_definition = TrustedLocalSttDefinition {
            config_sha256: hash_bytes(&serde_json::to_vec(&stt_spec).unwrap()),
            spec: stt_spec,
        };
        let stt_path = defs.join("local-stt.json");
        fs::write(&stt_path, serde_json::to_vec(&stt_definition).unwrap()).unwrap();
        fs::set_permissions(&stt_path, fs::Permissions::from_mode(0o600)).unwrap();
        let token_path = root.path().join("token");
        let mut token = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&token_path)
            .unwrap();
        writeln!(token, "0123456789abcdef").unwrap();
        let daemon = Daemon::prepare(DaemonConfig {
            sqlite_path: root.path().join("state.sqlite"),
            socket_path: runtime.join("victor.sock"),
            runtime_dir: runtime,
            trusted_definitions_dir: defs.clone(),
            allowed_roots: vec![root.path().to_path_buf()],
            token_file: token_path,
            scope: "local".into(),
            local_model_definition: model_path,
            local_stt_definition: stt_path,
            local_tts_definition: None,
        })
        .await
        .unwrap();
        assert_eq!(daemon.config().scope, "local");
    }

    #[test]
    fn rejects_symlink_definition() {
        let root = tempdir().unwrap();
        let defs = root.path().join("definitions");
        fs::create_dir(&defs).unwrap();
        let outside = root.path().join("outside.json");
        fs::write(&outside, b"{}").unwrap();
        std::os::unix::fs::symlink(&outside, defs.join("plugin.json")).unwrap();
        assert!(load_definitions(&defs).is_err());
    }
}
