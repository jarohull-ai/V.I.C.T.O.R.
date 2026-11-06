//! Pętla wykonawcza V.I.C.T.O.R.: STT, proweniencja i zapis do kontekstu.
//!
//! Ten crate jest cienki i celowo nie zna UI. Łączy kontrakty z
//! `victor-integrations` z magazynem `victor-context` oraz bramką
//! `victor-policy`. Chmura jest tylko opcjonalnym adapterem rejestrowanym przez
//! hosta; nie ma tu wyboru providera z poziomu żądania.

pub mod context_bridge;
pub mod error;
pub mod stt;
pub mod testing;
pub mod tools;

pub use context_bridge::{
    retained_audio_context_item, transcript_to_context_item, AudioRetention,
    TranscriptContextPolicy,
};
pub use error::RuntimeError;
pub use stt::{
    CloudEgressPolicy, SttJob, SttRoute, SttRuntime, SttRuntimeConfig, TranscriptionOutcome,
    CLOUD_STT_TOOL_ID,
};
pub use tools::{ToolExecutionJob, ToolExecutionOutcome, ToolRuntime, ToolRuntimeError};
