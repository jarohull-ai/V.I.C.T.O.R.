#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("identyfikator scope jest nieprawidłowy: `{0}`")]
    InvalidScope(String),

    #[error("identyfikator elementu kontekstu jest nieprawidłowy: `{0}`")]
    InvalidItemId(String),

    #[error("element kontekstu `{0}` jest nieprawidłowy: {1}")]
    InvalidItem(String, String),

    #[error("limit widoku musi być większy od zera: {0}")]
    InvalidBudget(String),

    #[error("błąd magazynu kontekstu: {0}")]
    Store(String),
}
