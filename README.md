# victor — rdzeń

Implementacja etapu 1 z `../VICTOR_PROJECT_DESIGN.md` §16: typy domenowe i bramka
polityki. Bez UI, bez providerów, bez bazy — ten kod ma działać identycznie pod
Tauri, egui i w testach.

## Co jest

| Crate | Zawartość |
|---|---|
| `crates/victor-domain` | `TaskState` (graf przejść), `Plan` + `PlanHash`, `Approval` (TTL + hash), `ToolDescriptor`, `Preview`, `UndoRecord`, `Capability`, `RiskLevel` |
| `crates/victor-policy` | `PathGuard` (canonicalizacja, `..`, symlink escape, deny-lista) i `PolicyEngine` (capabilities, ryzyko, zgoda) |
| `crates/victor-integrations` | Kontrakty `SpeechToText`, `SpeechOutput`, `WebFetcher`, lokalny `WhisperCppSpeechToText` + atrapy testowe. Zero zależności od AionCore/Audioticon/BALD i **zero klienta HTTP** — pilnuje tego test manifestu |
| `crates/victor-stt-cloud` | Osobny crate na chmurę: OpenAI i Deepgram za feature flagami `openai`/`deepgram`, wyłącznie przez `AuthorizedCloudTranscriptionRequest`; allowlista endpointów, twarde limity, retry bez fallbacku, brak własnego logowania |
| `crates/victor-context` | `ContextScope`, `ContextItem`, `MemoryTier` (working/durable/evidence), `Retention`, `ContextView` i `ContextStore`. Magazyn w pamięci; bez embeddingów i bez SQLite |
| `crates/victor-tools` | Registry manifestów, wersjonowanie, deklaratywne JSON Schema wejścia/wyjścia, kontrakt `ToolExecutor` i strukturalny `AuditSink` |
| `crates/victor-runtime` | Runtime STT i narzędzi: policy → approval → executor → wynik → context/audit |
| `crates/victor-fetch-bald` | Osobny adapter BALD dla read-only `web.fetch`; allowlista hostów na każdym redirect hopie, SSRF, limity i provenance evidence |
| `vendor/bse-fetch` | Samowystarczalna, MIT-licensed kopia potrzebnego crate'a fetch; brak zależności Cargo od checkoutu lub Git BALD |
| `crates/victor-storage` | SQLite `ContextStore` z kluczem `(scope_type, scope_id, item_id)`, indeksami scope/tier/czasu, purge i atomowym commit context+audit |
| `crates/victor-skills` | Deklaratywne manifesty skills: hash, JSON Schema, workflow z registry narzędzi, lifecycle, zależności i rollback |
| `crates/victor-skill-forge` | Bezpieczny forge opisu/recipe do Draft, dry-run, policy validation i Active; bez kodu i bez nowych capabilities |
| `crates/victor-skill-api` | Lokalny, UI-agnostyczny API list/review/lifecycle/audit dla skills; operacje są ponownie walidowane po stronie backendu |
| `crates/victor-skill-transport` | Unix socket transport z tokenem sesji, scope, limitami, timeoutem i idempotency cache; bez nasłuchu sieciowego |
| `crates/victor-skill-cli` | Referencyjny klient CLI nad Unix socketem: list/review/draft/update/approve/disable/rollback/audit |
| `crates/victor-plugin-contract` | Wersjonowany JSON-over-stdio/JFP contract z limitem ramek i handshake |
| `crates/victor-plugin-host` | Host procesów pluginów, sandbox boundary, restart lifecycle i trwały rejestr SQLite |
| `crates/victor-model-local` | Lokalny model przez limitowany JSON-over-stdio; tylko Reply/PlanRequest, bez sieci i fallbacku |

Decyzje o reuse: [`docs/ADR-001-reuse.md`](docs/ADR-001-reuse.md).

## Licencja

V.I.C.T.O.R. jest publicznie dostępnym kodem source-available na licencji
Business Source License 1.1 (BUSL-1.1), a nie projektem open source przed datą
zmiany licencji. Szczegóły parametrów, zakresu użycia niekomercyjnego i zasad
użycia produkcyjnego znajdują się w pliku [`LICENSE`](LICENSE) oraz w
[`docs/ADR-003-licensing.md`](docs/ADR-003-licensing.md).

Katalog `vendor/bse-fetch` jest odrębnym wyjątkiem licencyjnym: pozostaje na
licencji MIT i zachowuje własne pliki `LICENSE` oraz `ORIGIN.md`.

Publiczny gate CI dla `main` uruchamia kontrolę granic repozytorium, licencji,
formatowania, testów, Clippy i białych znaków dla każdego push oraz pull request.

## Granica lokalnego modelu

`victor-model-local` uruchamia jawnie wskazany proces z wyczyszczonym
środowiskiem i komunikuje się z nim wyłącznie przez limitowane ramki
JSON-over-stdio. Handshake wymaga wersji protokołu i identyfikatora modelu.
Adapter przyjmuje tylko `Reply` albo `PlanRequest`; nie istnieje w nim pole na
capabilities, risk, scope ani executor. Każdy `PlanRequest` wraca do
`victor-conversation`, gdzie manifest, `victor-policy`, `victor-runtime`,
context i audit pozostają jedyną ścieżką wykonania. Uszkodzony JSON, timeout,
EOF i crash kończą stdin/stdout przez `kill` + `wait`, a restart jest limitowany
konfiguracją. Adapter nie ma klienta sieciowego i nigdy nie przełącza się
automatycznie na chmurę.

Daemon ładuje model wyłącznie z server-side pliku `local-model.json` w katalogu
trusted definitions. Plik zawiera absolutną komendę, jawny working directory,
allowlistę środowiska, limity, timeout, restart policy, wymaganie sandboxa oraz
hash konfiguracji i executable. Klient CLI/UI nie może zmienić tych wartości;
brak pliku, zmiana hashy, symlink executable lub niebezpieczne uprawnienia
kończą start daemona.

## Granica pluginów

Zewnętrzne narzędzia działają jako ograniczone procesy potomne przez
`victor-plugin-host` i wersjonowany `victor-plugin-contract`. Manifest wire
opisuje tylko tożsamość pluginu oraz schematy narzędzi. Capabilities, zakresy
plików i risk nigdy nie są przyznawane przez plugin: pochodzą z lokalnego
`victor-tools::ToolManifest` i przechodzą przez `victor-policy` w
`victor-runtime`.

Pierwszym testem produkcyjnego kształtu jest `plugin_sysinfo` — narzędzie R0,
które raportuje dane CPU/RAM bez sieci i zapisu. Host używa JSON-over-stdio,
limitów ramek, timeoutów, heartbeatów i najwyżej jednego kontrolowanego
restartu po awarii procesu. Każdy child dostaje wyczyszczone środowisko oraz
wyłącznie jawnie przekazane zmienne. `SandboxRequirement::Required` odmawia
startu bez dostępnego bubblewrap; brak sandboxa nie jest maskowany jako
izolacja. WASM, biblioteki dynamiczne, proxy HTTP i kod generowany pozostają
poza tą granicą.

Awaria, timeout, rozłączenie lub błędny framing zamykają stdin/stdout i kończą
proces przez `kill` + `wait`. Limit ramki działa także przed otrzymaniem newline,
więc bufor nie rośnie bez ograniczeń. Restart jest limitowany do jednej próby i
może zapisać zdarzenie `Failed` do `AuditSink`.

`PersistentPluginHost` odtwarza po restarcie wyłącznie aktywne rejestracje z
SQLite. Przed uruchomieniem sprawdza identyfikator oraz SHA-256 lokalnego
manifestu; rekord zmieniony lub uszkodzony jest odrzucany. Rejestracja przez
`register_into_registry` wpisuje manifest do `victor-tools::ToolRegistry`,
więc jedyną ścieżką wykonania pozostaje `policy → runtime → executor`.

## Uruchomienie

```sh
cargo test --workspace
cargo test -p victor-stt-cloud --no-default-features
cargo test -p victor-stt-cloud --features openai
cargo test -p victor-stt-cloud --features deepgram
cargo test -p victor-stt-cloud --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
```

## Decyzje podjęte w kodzie

1. **`ToolDescriptor::path_arguments`** — rozszerzenie względem §13. Policy engine
   nie zgaduje, które pola JSON są ścieżkami; narzędzie musi je jawnie wymienić.
   Bez tej listy nie da się rzetelnie odróżnić `{"path": "/etc"}` od
   `{"pattern": "/etc"}`.
2. **`Planned -> Running` dozwolone** tylko po to, by plany R0 mogły wykonać się
   bez bramki zgody. Dla R1+ runtime musi przejść przez `AwaitingApproval`.
3. **R1 wymaga `undo_support`** — jeśli narzędzie deklaruje zmianę odwracalną, ale
   nie umie cofnąć, policy odmawia. To wymusza kontrakt z §9 zamiast obietnicy w UI.
4. **Zapis plików jest osobną flagą** (`PolicyConfig::allow_filesystem_write`),
   niezależną od włączenia narzędzia.
5. **Rozróżnienie `SymlinkEscape` od `OutsideAllowedRoot`** — komunikat w UI ma
   mówić, że coś próbowało wyjść z zakresu przez link, a nie tylko „poza zakresem".
6. **Hash planu** to SHA-256 po serializacji `serde_json`; mapa w `Value` jest
   sortowana po kluczu, więc kolejność pól nie wpływa na zgodę.
7. **R3 nie jest zakazane na stałe** (zgodnie z misją: „robić wszystko, co
   rozsądnie bezpieczne"). Domyślny profil to `CriticalActionPolicy::Blocked`,
   ale `RequireStrongConfirmation` pozwala na R3 wyłącznie ze zgodą
   `ApprovalKind::Critical`. Zwykłe „Zatwierdź" z widoku planu nie wystarcza —
   to osobny, mocniejszy ekran potwierdzenia.
8. **`ContextScope` nie ma ukrytej hierarchii.** To wywołujący (runtime + polityka)
   jawnie podaje listę scope'ów do `ContextViewRequest`. Dzięki temu „zadanie
   widzi projekt" jest decyzją polityki, a nie efektem ubocznym modelu danych.
9. **`ContextView` jest deterministyczny.** Sortowanie: `created_at` malejąco,
   przy remisie `id` rosnąco; budżet (`max_items`, `max_bytes`) odrzuca elementy,
   które się nie mieszczą, i liczy je w `dropped_items`. Ten sam stan magazynu
   daje ten sam prompt.
10. **Wrażliwość i retencja są w typach, nie w konwencji.** `Sensitivity::Secret`
    nie wchodzi do widoku bez `including_secret(true)`; `Retention::Until` i
    `TtlFromCreation` są sprawdzane przy budowie widoku i w `purge_expired`.
11. **Klucz elementu to `(scope, id)`.** Dwa projekty mogą mieć element o tym
    samym `id`; `get`/`delete` poza swoim scope zachowują się tak, jakby elementu
    nie było. To nie ujawnia istnienia danych w innym projekcie, a zarazem nie
    zmusza do globalnej unikalności identyfikatorów.
12. **STT jest lokalny domyślnie.** `WhisperCppSpeechToText` uruchamia wyłącznie
    lokalną binarkę na lokalnym WAV. `victor-integrations` nie linkuje żadnego
    klienta HTTP; test `integrations_crate_links_no_http_client` czyta manifest
    i wywala build, jeśli pojawi się `reqwest`, `hyper`, `ureq` lub `isahc`.
    Adapter chmurowy powstał jako osobny crate `victor-stt-cloud` (pkt 17–23).
13. **Brak fallbacku.** Brak binarki/modelu, błąd procesu, timeout albo pusty
    wynik to błąd zwrócony użytkownikowi. Nigdy ciche przełączenie na chmurę.
14. **Brak cichej konwersji audio.** whisper.cpp dostaje tylko WAV 16 kHz mono.
    Inny format albo próbkowanie = `Denied` z wyjaśnieniem; ffmpeg nie jest
    uruchamiany za plecami użytkownika.
15. **Zgoda chmurowa jest dwuczęściowa.** `CloudTranscriptionPermit` powstaje
    tylko z `ApprovalKind::Critical` i wiąże **hash planu** oraz **hash nagrania**
    (format + próbkowanie + kanały + bajty). Adapter chmurowy ma obowiązek
    sprawdzić permit jeszcze raz przed wysłaniem.
16. **`Transcript` niesie provenance.** `provider` i `location` (`Local`/`Cloud`)
    idą do `ContextItem` przez `ContextSource::local_stt` / `cloud_stt`, więc
    pamięć odróżnia transkrypcję lokalną od chmurowej bez zaglądania w logi.
17. **Daemon udostępnia lokalny głos przez `voice`.** Trusted `local-stt.json`
    określa binarkę whisper.cpp, model, limity, środowisko i sandbox; klient
    wysyła tylko audio oraz conversation ID. Audio nie trafia do SQLite, a
    transcript przechodzi przez runtime, rozmowę, policy i narzędzia.
18. **Chmura to osobny crate i opt-in.** `victor-stt-cloud` ma `default = []`;
    dopiero `--features openai` albo `--features deepgram` wciąga klienta HTTP.
    `victor-integrations` i lokalna ścieżka whisper.cpp nie wiedzą o istnieniu
    chmury, a `victor-stt-cloud` nie zna whispera.
18. **Zgoda jest wymuszona typem, nie konwencją.** Warstwa HTTP przyjmuje tylko
    `AuthorizedCloudTranscriptionRequest`, którego jedyny konstruktor sprawdza
    `CloudTranscriptionPermit` dla konkretnego planu i nagrania. Nie ma API,
    które przyjmie zwykłe `TranscriptionRequest` z opcjonalną zgodą.
19. **Permit jest sprawdzany przed każdą próbą i każdym retry.**
    `execute_with_retry` woła `verify` (hash planu + hash nagrania) w każdej
    iteracji; podmiana planu, bajtów albo metadanych audio kończy się `Denied`
    jeszcze przed wysłaniem czegokolwiek.
20. **Allowlista endpointów jest twarda.** Dozwolony jest tylko HTTPS, konkretny
    host i prefiks ścieżki; odrzucamy dane logowania, fragment, niestandardowy
    port i segmenty `..`. `HttpRequest` da się zbudować wyłącznie z
    `ValidatedEndpoint`, więc nie istnieje ścieżka „wyślij pod dowolny URL".
    Redirecty i proxy z env są wyłączone, żeby nie omijały allowlisty.
21. **Brak fallbacku między providerami.** OpenAI i Deepgram to dwa niezależne
    typy. Błąd jednego nie uruchamia drugiego; runtime musi jawnie zdecydować,
    którego providera woła.
22. **Retry jest ograniczony i nie ponawia 4xx.** Ponawiamy 408/425/429/5xx oraz
    timeout/błąd transportu, maksymalnie 5 prób, z backoffem ograniczonym do 30 s.
    Odpowiedź jest czytana strumieniowo i ucinana na limicie rozmiaru.
23. **Sekrety i treść nie trafiają do logów ani błędów.** `ApiKey` ma
    `Debug = ApiKey(***)`; `Debug` requestu pokazuje tylko nazwy nagłówków i
    długość body; komunikaty błędów nie zawierają body odpowiedzi. Crate nie ma
    bezpośredniej zależności `log`/`tracing` i pilnuje tego test manifestu.
    **Operacyjnie:** aplikacja nie może włączać trace-logów `reqwest`, bo
    wewnętrzny trace klienta może pokazać bajty połączenia.
24. **Runtime wybiera providera, nie UI.** Zwykłe `SttRuntime::transcribe` zawsze
    używa skonfigurowanego lokalnego Whispera. Chmura ma osobną metodę ścieżki
    planu i wymaga jednocześnie: włączonej polityki egressu, pojedynczego kroku
    `stt.cloud` R3, aktualnej zgody krytycznej oraz permitu z hashem planu i audio.
    Błąd lokalnego backendu kończy zadanie; nie istnieje automatyczny fallback.
25. **Transkrypcja trafia do kontekstu jako tekst z provenance.** Runtime zapisuje
    `ContextSource::local_stt` albo `ContextSource::cloud_stt`. Surowe audio nie
    jest zapisywane w `ContextItem`; osobna funkcja może zachować wyłącznie
    referencję po jawnym podaniu URI i retencji.

26. **Wykonanie narzędzia ma jedno źródło prawdy.** victor-tools przechowuje
    manifest i registry, ale nie podejmuje decyzji bezpieczeństwa. victor-runtime
    przekazuje plan do victor-policy, wymaga approval dla R1+, waliduje wejście
    i wyjście względem manifestu, dopiero potem wywołuje ToolExecutor. Udany
    wynik trafia do victor-context oraz do AuditSink.
27. **Skills są deklaratywne i nie nadają sobie capabilities.** `victor-skills`
    przechowuje manifest, wersję, hash, provenance, schematy i graf workflow,
    ale każde narzędzie musi pochodzić z `ToolRegistry`, a capabilities i risk
    wynikają wyłącznie z jego manifestu. Pierwszy profil skills dopuszcza tylko
    R0; walidacja przechodzi przez `victor-policy`, a lifecycle wymusza
    `Draft → Validated → Approved → Active → Disabled`. Aktualizacje zapisują
    historię i mogą zostać cofnięte do poprzedniego manifestu.
28. **Skill ma test produkcyjnego kształtu.** `victor-runtime/tests/skill_flow.rs`
    prowadzi aktywny `research.summary` przez trzy narzędzia, policy, SQLite,
    audit i restart. Ten sam test sprawdza odmowę stanu Draft/Disabled, zatrzymanie
    po błędzie `web.fetch`, allowlistę hosta, izolację scope oraz rollback hash.
29. **Forge generuje tylko deklaracje.** `victor-skill-forge` przyjmuje opis
    użytkownika i ograniczony recipe kroków, sprawdza istnienie narzędzi oraz
    odrzuca ryzyko wyższe niż R0. Manifest trafia do `Draft`, dry-run działa na
    kopii registry bez executorów, a walidacja, approval i activation pozostają
    w `victor-skills`. Forge nie ma API rejestracji narzędzi ani capabilities.
30. **Review i decyzje lifecycle są produkcyjne.** Forge zwraca serializowalny
    `SkillReview` z manifestem, dry-run reportem i `ManifestDiff`. Approve,
    activate, disable i rollback mogą zapisać `SkillDecisionAudit`; SQLite
    przechowuje te decyzje niezależnie od audytu wykonania narzędzi.
31. **UI nie jest źródłem zaufania.** `victor-skill-api` udostępnia listę,
    review, aktywację, disable, rollback i historię decyzji jako lokalny API.
    Każda operacja ponownie przechodzi przez Forge, `victor-skills` i policy;
    UI może tylko zainicjować żądanie, nie może zmienić lifecycle bez backendu.
32. **Transport jest lokalny z założenia.** `victor-skill-transport` używa Unix
    domain socket, wymaga tokena sesji i scope klienta, ogranicza rozmiar oraz
    czas żądania, a operacje z kluczem idempotencji są zwracane bez ponownego
    wykonania. Nie ma bindu HTTP ani nasłuchiwania na interfejsie sieciowym.
33. **CLI jest cienkim klientem.** `victor-skill` nie ma registry, policy ani
    executorów. Serializuje żądania do Unix socketu i wyświetla odpowiedź
    backendu; token może pochodzić z `VICTOR_SKILL_TOKEN` (preferowane) albo
    jawnego `--token`.
34. **Smoke test obejmuje cały transportowy cykl.** Test produkcyjnie kształtowany
    przechodzi przez draft → review → approve → list → audit → disable/update →
    rollback → ponowną aktywację, zapisuje snapshot registry, uruchamia drugi
    serwer i potwierdza odtworzenie aktywnej wersji oraz historii audytu z SQLite.
35. **Registry odtwarza się automatycznie.** `SkillApi::new` ładuje zapisany
    snapshot registry z `SkillDecisionAuditStore`, a mutacje zapisują nowy
    snapshot. Test restartu otwiera ponownie plik SQLite i nie wywołuje ręcznego
    restore.

## Czego jeszcze nie ma

- Synchronizacji zmian `bse-fetch` z BALD upstream. V.I.C.T.O.R. używa
  świadomie vendored snapshotu w `vendor/bse-fetch`, z zachowaną licencją MIT i
  własnym `ORIGIN.md`; aktualizacja wymaga osobnego audytu i osobnego commita
  wyłącznie w V.I.C.T.O.R.
- Indeksu semantycznego i `apps/`.
- Migracji schematu SQLite poza pierwszą migracją bazową — obecny storage ma
  trwały schemat kontekstu i audytu oraz odtwarzanie po restarcie.
- UI rozmowy i wybór konfiguracji hosta — runtime ma już pętlę
  `AudioChunk → adapter → Transcript → victor-context`, ale nie ma jeszcze
  aplikacji `apps/` ani interfejsu użytkownika.
- Integracji `victor-fetch-bald` z aplikacją hostującą; `plugin_sysinfo` jest
  pierwszym podłączonym pluginem demonstracyjnym.
- Konwersji audio do WAV 16 kHz mono — celowo jej nie ma; użytkownik musi
  dostarczyć poprawny plik albo jawnie włączyć przyszły konwerter.
- Podłączenia adaptera BALD do aplikacji hostującej i produkcyjnego audytu
  wyników.
- `InMemoryContextStore` pozostaje implementacją testową. Runtime może użyć
  atomowego `ToolResultStore`, aby wynik narzędzia i audyt zapisać jedną transakcją.
- Pełnej implementacji JSON Schema — obecnie `victor-tools` egzekwuje
  deterministyczny podzbiór typów, pól wymaganych i `properties`; nieobsługiwane
  słowa kluczowe są jawnie odrzucane.
- Dynamicznie generowanego kodu skills, WASM ani subprocess extensions — na tym
  etapie skills są wyłącznie deklaratywnymi grafami zarejestrowanych narzędzi.
- Deny-listy katalogów przeglądarek per profil — jest lista komponentów, nie
  wykrywanie realnych ścieżek profili.
