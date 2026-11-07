//! Bezpieczny, lokalny daemon składający trwały storage, API i Unix transport.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use victor_conversation::{
    ConversationEngine, ConversationLimits, ConversationRouter, ConversationService,
};
use victor_domain::ToolId;
use victor_model_local::{LocalModel, LocalModelConfig};
use victor_plugin_host::{PluginLaunchPolicy, PluginLaunchRequest, PluginLifecycleApi};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::ToolRuntime;
use victor_skill_api::SkillApi;
use victor_skill_transport::UnixSkillServer;
use victor_storage::SqliteContextStore;
use victor_tools::{ToolManifest, ToolRegistry};

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
        let server = UnixSkillServer::new(&config.socket_path, token, &config.scope, skill_api)
            .map_err(|e| DaemonError::Transport(e.to_string()))?
            .with_plugin_api_shared(plugin_api)
            .with_conversation(conversation);
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
        if path.file_name().and_then(|name| name.to_str()) == Some("local-model.json") {
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
        };
        assert!(load_local_model_definition(&config).is_err());
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
