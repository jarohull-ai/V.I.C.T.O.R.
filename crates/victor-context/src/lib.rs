//! Kontekst V.I.C.T.O.R.: co asystent wie o użytkowniku, projekcie, rozmowie
//! i zadaniu — oraz jak ograniczony wycinek trafia do promptu.
//!
//! Świadomie **nie ma tu** pamięci wektorowej, embeddingów ani zależności od
//! konkretnej bazy danych. To kontrakt: `ContextItem`, `ContextScope`,
//! `ContextView`, `ContextStore`; implementacja SQLite znajduje się osobno
//! w `victor-storage`.
//!
//! Zasady:
//! - scope jest twardą granicą — zapis i odczyt nie przeciekają między projektami;
//! - `ContextView` jest deterministyczny: ten sam stan + to samo żądanie = ten sam wynik;
//! - wrażliwość i retencja są częścią modelu, nie komentarzem w dokumentacji;
//! - polityka i zgody pozostają w `victor-policy`, ten crate ich nie zastępuje.

pub mod error;
pub mod item;
pub mod scope;
pub mod store;
pub mod view;

pub use error::ContextError;
pub use item::{
    ContextContent, ContextItem, ContextItemId, ContextItemKind, ContextSource, MemoryTier, Origin,
    Retention, Sensitivity,
};
pub use scope::{ContextScope, ScopeKind};
pub use store::{ContextStore, InMemoryContextStore};
pub use view::{build_view, ContextView, ContextViewRequest};
