use std::path::PathBuf;

/// Błędy domenowe. Każdy wariant jest typowany, żeby warstwa UI mogła
/// pokazać konkretną przyczynę odmowy, a nie ogólne „coś poszło nie tak".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("nieprawidłowy identyfikator narzędzia: {0}")]
    InvalidToolId(String),

    #[error("niedozwolone przejście stanu zadania: {from} -> {to}")]
    InvalidTransition { from: String, to: String },

    #[error("plan nie może być pusty")]
    EmptyPlan,

    #[error("plan musi zawierać dokładnie jedno ryzyko najwyższego kroku, otrzymano niezgodność")]
    RiskMismatch,

    #[error("zgoda nie pasuje do planu (plan zmienił się po zatwierdzeniu)")]
    ApprovalMismatch,

    #[error("zgoda wygasła")]
    ApprovalExpired,

    #[error("nieprawidłowy zakres czasu zgody")]
    InvalidApprovalWindow,

    #[error("narzędzie nie deklaruje wymaganej możliwości: {0}")]
    CapabilityNotDeclared(String),

    #[error("ścieżka musi być absolutna: {0}")]
    RelativePath(PathBuf),
}
