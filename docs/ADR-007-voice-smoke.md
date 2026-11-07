# ADR-007 — Produkcyjny smoke test rozmowy głosowej

**Status:** przyjęta  
**Data:** 7 listopada 2026 r.

## Decyzja

V.I.C.T.O.R. utrzymuje deterministyczny smoke test całego lokalnego przepływu
głosowego. Test uruchamia daemon z tymczasowymi, zaufanymi definicjami fixture:
STT, modelu, `plugin_sysinfo` i TTS. Nie używa prawdziwych modeli, sieci ani
zewnętrznych providerów.

Uruchomienie:

```sh
cargo test -p victor-daemon --test chat_e2e --all-features --locked \
  -- voice_smoke_full_chain_restart_and_security
```

Test wysyła deterministyczny WAV przez prywatny Unix socket i sprawdza kolejno:
lokalną transkrypcję z provenance, plan modelu, policy/runtime, plugin,
context/audit, tekst odpowiedzi oraz deterministyczne audio TTS. Sprawdza też
autoryzację, scope, replay idempotency w działającym procesie i po restarcie,
odtworzenie SQLite i brak surowego audio w context.

## Granice i znane ograniczenie

Wszystkie definicje są ładowane server-side i pozostają poza żądaniem klienta.
Błąd konfiguracji, hashy, procesu, policy lub socketu kończy się fail-closed;
testy jednostkowe poszczególnych adapterów pokrywają timeout, crash, rozłączenie,
zły framing i brak sandboxa.

Transport używa tabeli `transport_idempotency` w tym samym SQLite co context i
audit. Rekord wiąże klucz z hashem principal, scope i pełnym hashem żądania,
przechowuje status oraz odpowiedź ograniczoną do 4 MiB i wygasa po 24 godzinach.
Kolejność jest stała: auth → scope → walidacja klucza/hash → lookup → wykonanie.
Zły principal/scope/hash, uszkodzony JSON, niespójny status albo błąd SQLite
nie zwracają starej odpowiedzi i zatrzymują żądanie.
