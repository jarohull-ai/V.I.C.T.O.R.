//! Most `Transcript -> ContextItem`.
//!
//! Dwie zasady są tu wymuszone typem, nie konwencją:
//!
//! - transkrypcja trafia do kontekstu jako **tekst**, nigdy jako surowe audio;
//! - surowe audio można zachować wyłącznie przez jawne [`AudioRetention`],
//!   i nawet wtedy jest to referencja (URI + rozmiar + SHA-256), nie bajty.

use victor_context::Retention;
use victor_context::{
    ContextContent, ContextError, ContextItem, ContextItemId, ContextItemKind, ContextScope,
    ContextSource, MemoryTier, Sensitivity,
};
use victor_domain::UnixMs;
use victor_integrations::{AudioChunk, Transcript, TranscriptionLocation};

/// Domyślna polityka dla tekstu transkrypcji. Głos może zawierać dane prywatne,
/// więc wpis jest `Sensitive` i domyślnie wygasa po 24 godzinach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptContextPolicy {
    pub tier: MemoryTier,
    pub sensitivity: Sensitivity,
    pub retention: Retention,
}

impl Default for TranscriptContextPolicy {
    fn default() -> Self {
        Self {
            tier: MemoryTier::Working,
            sensitivity: Sensitivity::Sensitive,
            retention: Retention::TtlFromCreation(24 * 60 * 60 * 1_000),
        }
    }
}

/// Zamienia finalną transkrypcję na wpis kontekstu. Audio nie jest tu w ogóle
/// przekazywane — most nie ma dostępu do bajtów nagrania.
pub fn transcript_to_context_item(
    id: ContextItemId,
    scope: ContextScope,
    transcript: &Transcript,
    policy: TranscriptContextPolicy,
    now_unix_ms: UnixMs,
) -> Result<ContextItem, ContextError> {
    if transcript.text.trim().is_empty() {
        return Err(ContextError::InvalidItem(
            id.to_string(),
            "pusta transkrypcja nie może trafić do kontekstu".into(),
        ));
    }
    if transcript.provider.trim().is_empty() {
        return Err(ContextError::InvalidItem(
            id.to_string(),
            "transkrypcja bez nazwy dostawcy nie może trafić do kontekstu".into(),
        ));
    }
    if !transcript.is_final {
        return Err(ContextError::InvalidItem(
            id.to_string(),
            "częściowa transkrypcja nie może trafić do trwałego kontekstu".into(),
        ));
    }

    let source = match transcript.location {
        TranscriptionLocation::Local => ContextSource::local_stt(&transcript.provider),
        TranscriptionLocation::Cloud => ContextSource::cloud_stt(&transcript.provider),
    };

    ContextItem::new(
        id,
        scope,
        ContextItemKind::Message,
        policy.tier,
        ContextContent::inline(transcript.text.clone()),
        source,
        now_unix_ms,
    )?
    .with_sensitivity(policy.sensitivity)
    .with_retention(policy.retention)
}

/// Jawna decyzja o zachowaniu nagrania. Brak `Default` jest celowy: nie da się
/// przypadkiem zapisać audio „na wiecznie" albo z domyślną retencją.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRetention {
    uri: String,
    retention: Retention,
}

impl AudioRetention {
    pub fn explicit(uri: impl Into<String>, retention: Retention) -> Self {
        Self {
            uri: uri.into(),
            retention,
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }
}

/// Zapisuje **referencję** do nagrania. Nigdy nie wkłada bajtów audio do
/// `ContextItem`; zapisuje URI, rozmiar i SHA-256, a retencję bierze z jawnego
/// [`AudioRetention`]. Dzięki temu przyszły magazyn audio może wyczyścić plik
/// na podstawie tego samego wpisu.
pub fn retained_audio_context_item(
    id: ContextItemId,
    scope: ContextScope,
    audio: &AudioChunk,
    source: ContextSource,
    retention: AudioRetention,
    now_unix_ms: UnixMs,
) -> Result<ContextItem, ContextError> {
    audio
        .validate()
        .map_err(|error| ContextError::InvalidItem(id.to_string(), error.to_string()))?;

    let content = ContextContent::Reference {
        uri: retention.uri,
        bytes: audio.bytes.len() as u64,
        sha256: Some(audio.sha256()),
    };

    ContextItem::new(
        id,
        scope,
        ContextItemKind::FileReference,
        MemoryTier::Evidence,
        content,
        source,
        now_unix_ms,
    )?
    .with_sensitivity(Sensitivity::Sensitive)
    .with_retention(retention.retention)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ContextScope {
        ContextScope::project("victor").unwrap()
    }

    fn transcript(location: TranscriptionLocation) -> Transcript {
        Transcript {
            text: "otwórz projekt".into(),
            language: Some("pl".into()),
            confidence: Some(0.9),
            is_final: true,
            provider: "test-stt".into(),
            location,
        }
    }

    fn audio() -> AudioChunk {
        AudioChunk {
            bytes: vec![1, 2, 3, 4],
            format: victor_integrations::AudioFormat::Wav,
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }

    #[test]
    fn transcript_becomes_text_with_local_provenance() {
        let item = transcript_to_context_item(
            ContextItemId::parse("utt-1").unwrap(),
            scope(),
            &transcript(TranscriptionLocation::Local),
            TranscriptContextPolicy::default(),
            1_000,
        )
        .unwrap();

        assert_eq!(item.content, ContextContent::inline("otwórz projekt"));
        assert_eq!(item.source, ContextSource::local_stt("test-stt"));
        assert_eq!(item.sensitivity, Sensitivity::Sensitive);
        assert!(item.expires_at().is_some());
    }

    #[test]
    fn transcript_becomes_text_with_cloud_provenance() {
        let item = transcript_to_context_item(
            ContextItemId::parse("utt-2").unwrap(),
            scope(),
            &transcript(TranscriptionLocation::Cloud),
            TranscriptContextPolicy::default(),
            1_000,
        )
        .unwrap();

        assert_eq!(item.source, ContextSource::cloud_stt("test-stt"));
    }

    #[test]
    fn raw_audio_requires_explicit_retention_and_stays_a_reference() {
        let item = retained_audio_context_item(
            ContextItemId::parse("audio-1").unwrap(),
            scope(),
            &audio(),
            ContextSource::new(victor_context::Origin::User, "audio:capture"),
            AudioRetention::explicit("file:///tmp/utt-1.wav", Retention::TtlFromCreation(5_000)),
            1_000,
        )
        .unwrap();

        assert_eq!(item.tier, MemoryTier::Evidence);
        match item.content {
            ContextContent::Reference {
                ref uri,
                bytes,
                ref sha256,
            } => {
                assert_eq!(uri, "file:///tmp/utt-1.wav");
                assert_eq!(bytes, 4);
                assert!(sha256.is_some());
            }
            ContextContent::Inline { .. } => panic!("audio nie może być inline"),
        }
        assert_eq!(item.expires_at(), Some(6_000));
    }

    #[test]
    fn partial_or_empty_transcripts_are_rejected() {
        let mut partial = transcript(TranscriptionLocation::Local);
        partial.is_final = false;
        assert!(transcript_to_context_item(
            ContextItemId::parse("utt-1").unwrap(),
            scope(),
            &partial,
            TranscriptContextPolicy::default(),
            1_000,
        )
        .is_err());

        let mut empty = transcript(TranscriptionLocation::Local);
        empty.text = "   ".into();
        assert!(transcript_to_context_item(
            ContextItemId::parse("utt-2").unwrap(),
            scope(),
            &empty,
            TranscriptContextPolicy::default(),
            1_000,
        )
        .is_err());
    }
}
