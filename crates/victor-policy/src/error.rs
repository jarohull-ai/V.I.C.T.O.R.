use std::path::PathBuf;

use victor_domain::DomainError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("ścieżka musi być absolutna: {0}")]
    RelativePath(PathBuf),

    #[error("ścieżka zawiera `..` i nie może być zaakceptowana: {0}")]
    ParentTraversal(PathBuf),

    #[error("zabroniony komponent `{component}` w ścieżce: {path}")]
    DeniedComponent { path: PathBuf, component: String },

    #[error("zabroniony prefiks systemowy: {0}")]
    DeniedPrefix(PathBuf),

    #[error("ścieżka nie istnieje, a jej katalog nadrzędny też nie: {0}")]
    MissingPath(PathBuf),

    #[error("ścieżka poza dozwolonymi katalogami: {0}")]
    OutsideAllowedRoot(PathBuf),

    #[error("symlink {path} wskazuje poza dozwolony zakres: {target}")]
    SymlinkEscape { path: PathBuf, target: PathBuf },

    #[error("ścieżka poza zakresem zadeklarowanym przez narzędzie: {path} (scope: {scope})")]
    OutsideToolScope { path: PathBuf, scope: PathBuf },

    #[error("lista dozwolonych katalogów nie może być pusta")]
    EmptyAllowedRoots,

    #[error("wymagana jest zgoda użytkownika, ale jej nie przekazano")]
    MissingApproval,

    #[error("działania krytyczne (R3) są wyłączone w bieżącym profilu")]
    CriticalActionsDisabled,

    #[error("działanie R3 wymaga zgody krytycznej, otrzymano zwykłą zgodę planu")]
    WeakApproval,

    #[error("zgoda odrzucona: {0}")]
    InvalidApproval(#[from] DomainError),
}
