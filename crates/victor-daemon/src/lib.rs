//! Bezpieczny lokalny proces spinający trwałość, API skills i plugin transport.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::oneshot;
use victor_domain::{RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{PluginLaunchPolicy, PluginLifecycleApi};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_skill_api::SkillApi;
use victor_skill_transport::{TransportError, UnixSkillServer};
use victor_storage::SqliteContextStore;
use victor_tools::{ToolManifest, ToolRegistry};

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub sqlite_path: PathBuf,
    pub socket_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub token_file: PathBuf,
    pub scope: String,
    pub plugin_sysinfo: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("nieprawidłowa konfiguracja daemona: {0}")]
    Config(String),
    #[error("błąd pliku daemona: {0}")]
    Io(#[from] std::io::Error),
    #[error("błąd storage daemona: {0}")]
    Storage(String),
    #[error("błąd API daemona: {0}")]
    Api(String),
    #[error("błąd transportu daemona: {0}")]
    Transport(#[from] TransportError),
}

impl DaemonConfig {
    pub fn validate(&self) -> Result<(), DaemonError> {
        for (name, path) in [
            ("sqlite", &self.sqlite_path),
            ("socket", &self.socket_path),
            ("runtime", &self.runtime_dir),
            ("token", &self.token_file),
        ] {
            if path.as_os_str().is_empty() {
                return Err(DaemonError::Config(format!("ścieżka {name} jest pusta")));
            }
        }
        if self.scope.trim().is_empty() {
            return Err(DaemonError::Config("scope nie może być pusty".into()));
        }
        Ok(())
    }
}

pub fn prepare_runtime_dir(path: &Path) -> Result<(), DaemonError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DaemonError::Config(
                "runtime directory nie jest bezpiecznym katalogiem".into(),
            ));
        }
        ensure_owned(&metadata, "runtime directory")?;
    } else {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DaemonError::Config("runtime directory zmienił typ".into()));
    }
    ensure_owned(&metadata, "runtime directory")
}

pub fn read_token_file(path: &Path) -> Result<String, DaemonError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DaemonError::Config(
            "token file nie jest zwykłym plikiem".into(),
        ));
    }
    ensure_owned(&metadata, "token file")?;
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(DaemonError::Config("token file musi mieć tryb 0600".into()));
    }
    let bytes = fs::read(path)?;
    if bytes.len() > 4096 {
        return Err(DaemonError::Config("token file jest za duży".into()));
    }
    let token = String::from_utf8(bytes)
        .map_err(|_| DaemonError::Config("token file nie jest UTF-8".into()))?
        .trim()
        .to_string();
    if token.len() < 16 || token.chars().any(char::is_whitespace) {
        return Err(DaemonError::Config("token ma nieprawidłowy format".into()));
    }
    Ok(token)
}

fn ensure_owned(metadata: &fs::Metadata, label: &str) -> Result<(), DaemonError> {
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(DaemonError::Config(format!(
            "{label} należy do innego użytkownika"
        )));
    }
    Ok(())
}

fn sysinfo_manifest() -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("plugin_sysinfo").expect("stały ToolId"),
            schema_version: 1,
            description: "CPU/RAM read-only".into(),
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
            capabilities: vec![],
            risk: RiskLevel::R0,
            default_timeout_ms: 1_000,
            undo_support: false,
            path_arguments: vec![],
        },
    )
}

pub async fn build_server(config: &DaemonConfig) -> Result<UnixSkillServer, DaemonError> {
    config.validate()?;
    prepare_runtime_dir(&config.runtime_dir)?;
    let token = read_token_file(&config.token_file)?;
    let store = Arc::new(
        SqliteContextStore::open(&config.sqlite_path)
            .map_err(|error| DaemonError::Storage(error.to_string()))?,
    );

    let mut tools = ToolRegistry::new();
    let mut enabled = Vec::new();
    if config.plugin_sysinfo.is_some() {
        tools
            .register(sysinfo_manifest())
            .map_err(|error| DaemonError::Api(error.to_string()))?;
        enabled.push(ToolId::parse("plugin_sysinfo").expect("stały ToolId"));
    }
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([config.runtime_dir.clone()]).with_enabled_tools(enabled),
        )
        .map_err(|error| DaemonError::Api(error.to_string()))?,
    );
    let skill_api = SkillApi::new(Arc::new(tools), policy.clone(), store.clone())
        .map_err(|error| DaemonError::Api(error.to_string()))?;
    let mut plugin_api = PluginLifecycleApi::open(store, policy, None)
        .await
        .map_err(|error| DaemonError::Api(error.to_string()))?;
    if let Some(executable) = &config.plugin_sysinfo {
        plugin_api = plugin_api
            .with_plugin_definition(
                sysinfo_manifest(),
                vec![executable.display().to_string()],
                PluginLaunchPolicy {
                    working_dir: Some(config.runtime_dir.clone()),
                    ..PluginLaunchPolicy::default()
                },
            )
            .map_err(|error| DaemonError::Api(error.to_string()))?;
    }
    Ok(
        UnixSkillServer::new(&config.socket_path, token, &config.scope, skill_api)?
            .with_plugin_api(plugin_api),
    )
}

pub async fn run(config: DaemonConfig) -> Result<(), DaemonError> {
    let server = build_server(&config).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let signal = async {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .map_err(DaemonError::Io)?;
            tokio::select! {
                result = tokio::signal::ctrl_c() => result.map_err(DaemonError::Io),
                _ = term.recv() => Ok(()),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.map_err(DaemonError::Io)
        }
    };
    tokio::select! {
        result = server.run(shutdown_rx) => result.map_err(DaemonError::Transport),
        signal_result = signal => {
            signal_result?;
            let _ = shutdown_tx.send(());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn token_file_requires_owner_and_private_permissions() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "0123456789abcdef\n").unwrap();
        fs::set_permissions(file.path(), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(file.path()).unwrap(), "0123456789abcdef");
        fs::set_permissions(file.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token_file(file.path()).is_err());
    }

    #[test]
    fn runtime_directory_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("runtime");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(prepare_runtime_dir(&link).is_err());
    }
}
