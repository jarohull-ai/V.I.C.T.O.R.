#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("dostawca nie jest skonfigurowany: {0}")]
    NotConfigured(String),

    #[error("żądanie odrzucone przez politykę: {0}")]
    Denied(String),

    #[error("przekroczono limit: {0}")]
    LimitExceeded(String),

    #[error("przekroczono limit czasu: {0}")]
    Timeout(String),

    #[error("błąd transportu: {0}")]
    Transport(String),

    #[error("błąd dostawcy `{provider}`: {message}")]
    Provider { provider: String, message: String },

    #[error("błąd wejścia/wyjścia: {0}")]
    Io(#[from] std::io::Error),
}

impl IntegrationError {
    pub fn provider(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Provider {
            provider: provider.into(),
            message: message.into(),
        }
    }
}
