//! Kontrakty integracji V.I.C.T.O.R.
//!
//! Ten crate celowo **nie zależy** od AionCore, Audioticon, BALD ani od żadnego
//! innego projektu. Definiuje tylko granice, do których te projekty będą
//! podłączane adapterami. Dzięki temu decyzja „włączamy crate X" nie zmienia
//! ani domeny, ani polityki.
//!
//! Kolejność z audytu: najpierw adaptery i kontrakty, potem włączenie
//! większej części cudzego workspace'u.

pub mod error;
pub mod fetch;
pub mod stt_whisper;
pub mod testing;
pub mod voice;

pub use error::IntegrationError;
pub use fetch::{
    FetchPolicy, FetchProvenance, FetchRequest, FetchResponse, WebFetcher,
    DEFAULT_MAX_RESPONSE_BYTES,
};
pub use stt_whisper::{
    ProcessWhisperRunner, WhisperCppConfig, WhisperCppSpeechToText, WhisperInvocation,
    WhisperRunner, DEFAULT_WHISPER_TIMEOUT, WHISPER_PROVIDER, WHISPER_SAMPLE_RATE_HZ,
};
pub use voice::{
    speech_cache_key, AudioChunk, AudioFormat, CloudTranscriptionPermit, SpeechAudio, SpeechOutput,
    SpeechRequest, SpeechToText, Transcript, TranscriptionLocation, TranscriptionRequest,
};

/// SHA-256 w postaci hex. Wspólne dla cache audio i proweniencji pobrań.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("zapis do String nie może się udać");
    }
    hex
}
