# ADR-004 — Granica lokalnego modelu

**Status:** przyjęta
**Data:** 7 listopada 2026 r.

## Decyzja

Lokalny model jest osobnym procesem uruchamianym przez `victor-model-local`.
Jego jedynym kontraktem jest ograniczony JSON-over-stdio z handshake, limitem
ramki i timeoutem. Model może zwrócić wyłącznie `Reply` lub `PlanRequest`.

Model nie dostaje registry, executora, capabilities, risk, scope ani sekretów
środowiska. `PlanRequest` zawiera tylko identyfikator narzędzia, opis i
argumenty. Identyfikator jest walidowany składniowo, a istnienie narzędzia,
capabilities, ryzyko, zakres, approval i wykonanie są rozstrzygane dopiero przez
`victor-conversation → victor-policy → victor-runtime`.

## Lifecycle procesu

Host czyta ramki bez nieograniczonego bufora. Uszkodzony JSON, brak delimitera,
przekroczenie limitu, timeout, rozłączenie i crash zamykają stdin/stdout oraz
kończą child przez `kill` i `wait`. Liczba restartów jest jawnie ograniczona.
Proces jest uruchamiany z wyczyszczonym środowiskiem, bez sieciowego adaptera i
bez automatycznego fallbacku do chmury.

Fixture testowy nie jest prawdziwym modelem. Testuje handshake, odpowiedź,
plan, zły JSON, próbę prompt injection i timeout; produkcyjny model musi
implementować dokładnie ten sam kontrakt.

## Konsekwencje

Lokalny model może proponować plan, ale nigdy nie może sam nadać sobie dostępu.
Każde wykonanie nadal kończy się wynikiem w context/audit. Dodanie STT/TTS,
HTTP, WASM lub dynamicznych bibliotek wymaga osobnej decyzji architektonicznej.
