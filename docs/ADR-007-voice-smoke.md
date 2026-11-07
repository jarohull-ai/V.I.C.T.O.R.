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
autoryzację, scope, replay idempotency w działającym procesie, restart daemona,
odtworzenie SQLite i brak surowego audio w context.

## Granice i znane ograniczenie

Wszystkie definicje są ładowane server-side i pozostają poza żądaniem klienta.
Błąd konfiguracji, hashy, procesu, policy lub socketu kończy się fail-closed;
testy jednostkowe poszczególnych adapterów pokrywają timeout, crash, rozłączenie,
zły framing i brak sandboxa.

Cache idempotency transportu jest obecnie procesowy. Po restarcie test używa
nowego klucza i potwierdza bezpieczne odtworzenie sesji/context/audit, ale trwała
ochrona replay tego samego klucza po restarcie pozostaje elementem release
readiness. Nie udajemy, że SQLite przechowuje tę ochronę, dopóki nie będzie to
właściwie zaimplementowane.
