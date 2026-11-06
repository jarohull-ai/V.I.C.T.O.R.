use thiserror::Error;

use victor_context::ContextError;
use victor_integrations::IntegrationError;

/// Błędy pętli runtime. Świadomie oddzielamy odmowę polityki od błędu
/// technicznego dostawcy: UI ma pokazać „nie wolno", a nie „spróbuj ponownie".
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Integration(#[from] IntegrationError),

    #[error(transparent)]
    Context(#[from] ContextError),

    /// Chmura została odrzucona przez runtime/politykę. To nie jest błąd
    /// lokalnego backendu i **nigdy** nie uruchamia fallbacku.
    #[error("odmowa chmurowego STT: {0}")]
    CloudDenied(String),

    /// Adapter zwrócił wynik sprzeczny z kontraktem (np. lokalny provider
    /// oznajmił `location = Cloud`). Runtime woli odrzucić wynik niż zapisać
    /// fałszywą proweniencję w kontekście.
    #[error("naruszenie kontraktu dostawcy STT: {0}")]
    ProviderContract(String),
}
