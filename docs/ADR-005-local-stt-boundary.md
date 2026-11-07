# ADR-005 — Lokalny STT w daemonie

**Status:** przyjęta  
**Data:** 7 listopada 2026 r.

## Decyzja

Wejście `voice` przez prywatny Unix socket przyjmuje wyłącznie bajty audio i
identyfikator rozmowy. Daemon ładuje `local-stt.json` tylko z server-side
katalogu trusted definitions. Konfiguracja zawiera absolutną binarkę, model,
język, wątki, timeout, limit WAV, katalog pracy, allowlistę środowiska,
wymaganie sandboxa oraz hashe konfiguracji, binarki i modelu.

Przepływ jest stały: `AudioChunk → WhisperCppSpeechToText → SttRuntime →
Transcript/ContextItem → ConversationService → policy → runtime → plugin`.
STT i model nie otrzymują capabilities, risk, approval ani executora. Błąd
lokalnej binarki, modelu, formatu, timeoutu lub procesu kończy operację; nie ma
fallbacku do chmury.

## Prywatność i granice

Audio pozostaje w pamięci procesu i nie jest zapisywane w SQLite. Kontekst oraz
odpowiedź transportu zawierają transcript, `provider=whisper.cpp`,
`location=local` i hash audio. Zachowanie samego pliku audio wymaga osobnego,
jawnego mechanizmu retencji istniejącego w runtime.

## Konsekwencje

CLI jest cienkim klientem i nie może zmienić konfiguracji STT. WAV musi być
RIFF/WAVE, 16 kHz mono; V.I.C.T.O.R. nie uruchamia po cichu ffmpeg ani nie
konwertuje nagrania. TTS pozostaje osobnym, następnym adapterem.
