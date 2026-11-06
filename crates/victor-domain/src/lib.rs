//! Typy domenowe V.I.C.T.O.R.
//!
//! Ten crate nie zna frameworka UI, żadnego providera modelu ani bazy danych.
//! Zawiera wyłącznie kontrakty opisane w `VICTOR_PROJECT_DESIGN.md` §6, §9 i §13:
//! stan zadania, plan, poziomy ryzyka, capabilities i zgodę powiązaną z hashem planu.

pub mod capability;
pub mod error;
pub mod plan;
pub mod task;
pub mod tool;

pub use capability::{Capability, PathScope, RiskLevel};
pub use error::DomainError;
pub use plan::{Approval, ApprovalKind, Plan, PlanHash, PlanStep};
pub use task::{Task, TaskId, TaskState};
pub use tool::{
    Preview, PreviewAction, PreviewItem, ToolDescriptor, ToolId, ToolOutput, UndoRecord, UndoState,
};

/// Znacznik czasu w milisekundach od epoki UNIX.
pub type UnixMs = u64;
