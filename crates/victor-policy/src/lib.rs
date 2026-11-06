//! Bramka polityki V.I.C.T.O.R.
//!
//! Odpowiada za §9 projektu: canonicalizację ścieżek, wykrywanie ucieczki przez
//! symlink, odrzucanie `..`, listę katalogów zabronionych, capability check
//! oraz weryfikację zgody powiązanej z hashem planu.
//!
//! Ten crate nie wykonuje operacji — tylko decyduje. Wykonanie należy do
//! `victor-tools`, które może działać wyłącznie na wyniku `Allowed` /
//! `ApprovalRequired` + ważnej zgodzie.

pub mod engine;
pub mod error;
pub mod path_guard;

pub use engine::{CriticalActionPolicy, Outcome, PolicyConfig, PolicyDecision, PolicyEngine};
pub use error::PolicyError;
pub use path_guard::{PathGuard, DEFAULT_DENIED_COMPONENTS};
