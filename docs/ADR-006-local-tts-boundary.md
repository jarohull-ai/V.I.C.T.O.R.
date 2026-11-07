# ADR-006 — Lokalny TTS

**Status:** przyjęta  
**Data:** 7 listopada 2026 r.

## Decyzja

V.I.C.T.O.R. używa provider-neutral adaptera `victor-tts-local`, który
implementuje `victor_integrations::SpeechOutput` przez ograniczony proces
JSON-over-stdio. Proces otrzymuje wyłącznie tekst odpowiedzi, język, voice,
prędkość i zatwierdzony model. Nie otrzymuje planu, capabilities, risk,
approval ani executora.

Daemon ładuje `local-tts.json` wyłącznie z trusted definitions. Hash
konfiguracji, executable i modelu, prywatny working directory, allowlista env,
limity ramek/audio/stderr, timeout, restart policy i wymaganie sandboxa są
weryfikowane przed uruchomieniem. Brak sandboxa przy wymaganiu kończy się
odmową.

## Błąd i prywatność

Tekst odpowiedzi pozostaje dostępny, gdy TTS jest wyłączony albo zakończy się
błędem. Status TTS i błąd są jawnie zwracane; nie ma cichego fallbacku do
chmury. Audio wejściowe STT i audio wyjściowe TTS nie są zapisywane w SQLite
bez osobnej, jawnej retencji. Wynik niesie lokalne provenance i deterministyczny
klucz cache.

TTS jest osobnym adapterem. Nie zmienia polityki chmurowego STT/TTS i nie
wprowadza HTTP ani zależności od konkretnego providera.
