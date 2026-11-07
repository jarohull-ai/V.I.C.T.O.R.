# ADR-001 — Reuse komponentów zewnętrznych

**Status:** przyjęta (adapter-first), otwarte punkty P0  
**Data:** 6 listopada 2026  
**Kontekst:** raport audytowy reuse dla V.I.C.T.O.R.

## Decyzja

1. **Adapter-first.** V.I.C.T.O.R. definiuje własne kontrakty (`victor-integrations`),
   a projekty zewnętrzne są podłączane dopiero za nimi. Żaden crate AionCore,
   Audioticon, BALD, rust-lab ani SEED nie wchodzi do grafu zależności w pierwszej fali.
2. **Właścicielem polityki, zgód i ryzyka jest V.I.C.T.O.R.** (`victor-domain`,
   `victor-policy`). AionCore nie przejmuje tych decyzji — jego FSM służy do stanu
   rozmowy/transportu, a `waiting_on_approval` mapuje się na nasze `AwaitingApproval`,
   ale nie jest źródłem prawdy o zgodzie.
3. **Fail-closed bez wyjątków.** Nie uruchamiamy executora bez jawnej polityki.
   AionCore `sandbox.rs` jest opt-in: brak pliku polityki oznacza komendę bez
   wrappera. Tego zachowania nie dziedziczymy — brak polityki = odmowa.
4. **Jedno wyjście sieciowe.** Wszystkie żądania pobierania treści z sieci przechodzą
   przez `WebFetcher` (docelowo BALD `bse-fetch`). Pluginy XcratesX nie mogą robić
   własnego HTTP. Jedyny drugi, świadomy kanał egress to wysyłka audio do STT:
   osobny crate `victor-stt-cloud`, własna allowlista endpointów i zgoda krytyczna
   związana z hashem planu i nagrania.
5. **`seedy_ontic_v008` wykluczony.** Potwierdzone: `system2.rs:384` zwraca
   `Ok(true)` (czyli `PLAN_ACCEPTED`) gdy binarka walidatora jest niedostępna.

## Weryfikacja licencji (sprawdzone w plikach, nie z pamięci)

| Projekt | Plik |Licencja | Wniosek |
|---|---|---|---|
| AionCore | `LICENSE` | Apache-2.0 | OK |
| Audioticon | `LICENSE-MIT`, `LICENSE-APACHE` | MIT / Apache-2.0 | OK |
| BALD Search Engine | `LICENSE` | MIT | OK |
| rust-lab | `LICENSE-MIT`, `LICENSE-APACHE` | MIT / Apache-2.0 | OK |
| XcratesX | `LICENSE` | MIT | OK |
| JFP Box v0.2 | `seedy_ontic/jfp-box-v0.2/LICENSE` | PolyForm Noncommercial 1.0.0 | **blokada do pisemnej zgody** |
| seedy_ontic v1.0.0 | `seedy_ontic/LICENSE` (dziedziczony) | **wszystkie prawa zastrzeżone, proprietary** | **blokada — nie PolyForm** |

**Korekta raportu:** raport twierdzi, że `seedy_ontic/v1.0.0` jest na PolyForm
Noncommercial. W rzeczywistości katalog `v1.0.0` nie ma własnego LICENSE i
dziedziczy `SEED MAD/seedy_ontic/LICENSE`, który mówi „All rights reserved /
proprietary and confidential / unauthorized use strictly prohibited". To jest
ostrzejsze niż PolyForm: nie ma tu żadnego dozwolonego użycia bez zgody.

## Konsekwencje

- `victor-integrations` dostarcza `SpeechToText`, `SpeechOutput`, `WebFetcher`
  oraz atrapy testowe. Podłączenie AionCore STT, Audioticon TTS i BALD fetch
  to osobne adaptery, każdy z testem kontraktowym.
- Manifest JFP Box można *przeczytać i zwalidować* dopiero po decyzji licencyjnej.
  Do tego czasu `victor-policy` pozostaje jedynym walidatorem polityki.
- `seed_vector_mem` może być najwyżej indeksem nad trwałą pamięcią w SQLite —
  jest RAM-only i nie ma retencji.
- `victor-context` powstał bez żadnej zależności od audytowanych projektów:
  scope, wrażliwość, retencja i budżet widoku są nasze. AionCore może później
  dostarczyć transport/FSM, ale nie definiuje pamięci ani zgód.
- **STT nie bierze `aionui-shell` jako dependency.** `aionui-shell` ciągnie
  `axum`, `aionui-auth`, `aionui-runtime` i `aionui-system`, a jego STT to
  wyłącznie klient OpenAI/Deepgram — każde rozpoznanie wysyła audio do chmury.
  Zamiast tego `WhisperCppSpeechToText` uruchamia lokalną binarkę whisper.cpp,
  a `victor-integrations` w ogóle nie linkuje klienta HTTP (test manifestu).
- **Chmura jest osobnym crate'em.** `victor-stt-cloud` ma feature flags `openai`
  i `deepgram` (domyślnie obie wyłączone), więc sama obecność crate'a nie dodaje
  HTTP do lokalnej ścieżki. API wymaga `AuthorizedCloudTranscriptionRequest`,
  a nie `TranscriptionRequest`; permit jest sprawdzany przed każdą próbą i retry.
  Endpointy przechodzą allowlistę (HTTPS + host + prefiks ścieżki), redirecty i
  proxy z env są wyłączone, a odpowiedzi mają twardy limit rozmiaru. Nie ma
  fallbacku OpenAI ↔ Deepgram: błąd jednego providera jest błędem, nie powodem
  do wysłania tego samego audio do drugiego.
- **Logowanie chmurowe.** `victor-stt-cloud` nie loguje i nie ma bezpośredniej
  zależności `log`/`tracing`; `Debug` requestu i `ApiKey` są zredagowane, a błędy
  nie zawierają body odpowiedzi. Aplikacja hostująca musi jednak nie włączać
  trace-logów `reqwest`, bo wewnętrzny trace klienta może pokazać bajty
  połączenia (w tym nagłówek `Authorization`).

### Runtime STT i retencja audio

`victor-runtime` jest właścicielem routingu STT i nie przyjmuje providera od UI:
zwykła ścieżka używa lokalnego adaptera, a chmurowa jest osobnym krokiem planu
`stt.cloud` i przechodzi przez `CriticalApproval`. Lokalny błąd jest końcowy —
nie uruchamia chmury.

Most runtime zapisuje do `victor-context` tylko finalny tekst i źródło
(`local`/`cloud`). Surowe audio może pojawić się wyłącznie jako referencja z
jawnym URI i retencją; bajty nie są kopiowane do `ContextItem`.

### Kontrakt narzędzi

victor-tools jest właścicielem registry, manifestu, wersji narzędzia, walidacji
podstawowego JSON Schema oraz kontraktu executora/audytu. Nie omija
victor-policy: runtime najpierw ocenia plan i approval, następnie waliduje
wejście, wykonuje narzędzie, waliduje wynik i zapisuje go do context oraz audytu.
Dynamicznie generowany kod i trwały storage pozostają poza tą warstwą.

`victor-storage` implementuje trwały backend SQLite za tym samym kontraktem
`ContextStore`. Ma klucz `(scope_type, scope_id, item_id)`, osobne kolumny dla
metadanych pamięci, retencji, provenance i hashy, indeksy scope/tier/czasu oraz
deterministyczne `purge_expired()`. Dodatkowy `ToolResultStore` zapisuje
`ContextItem` i `ToolAuditEvent` w jednej transakcji.

### Adapter BALD

victor-fetch-bald jest osobnym adapterem zależnym od bse-fetch; bazowy
victor-integrations nie dostaje klienta HTTP. Narzędzie web.fetch jest R0
wyłącznie dla jawnie skonfigurowanej allowlisty hostów i zapisuje wynik jako
MemoryTier::Evidence, z URL-em żądanym/końcowym, czasem pobrania oraz hashami
treści. bse-fetch egzekwuje SSRF, timeout, rozmiar, robots i ponowną walidację
każdego redirectu; dodatkowa allowlista hostów jest sprawdzana przed każdym
hopem, aby redirect nie mógł wyjść poza zgodę użytkownika.

Na granicy repozytoriów używamy vendoringu: `vendor/bse-fetch` jest
samowystarczalnym snapshotem potrzebnego crate'a z zachowaną licencją MIT.
Manifest V.I.C.T.O.R. nie wskazuje lokalnego checkoutu BALD ani repozytorium
Git BALD, a `vendor/bse-fetch/Cargo.toml` nie dziedziczy konfiguracji BALD.
`vendor/bse-fetch/ORIGIN.md` dokumentuje pochodzenie snapshotu.

### Skills deklaratywne

`victor-skills` jest właścicielem manifestu skill, jego wersji, hasha,
provenance, schematów JSON, grafu workflow i lifecycle. Skill nie zawiera kodu
wykonywalnego ani pola `capabilities`: capabilities, risk i zakres wynikają
wyłącznie z manifestów narzędzi obecnych w `victor-tools::ToolRegistry`.

Walidacja odrzuca nieznane narzędzia, błędne argumenty, cykle workflow,
cykle zależności i wyjście poza scope polityki. Pierwsza klasa skills jest
read-only i dopuszcza R0; przejście do `Validated` wymaga również oceny przez
`victor-policy`. Lifecycle wymusza kolejność `Draft → Validated → Approved →
Active → Disabled`, a aktualizacja zachowuje poprzedni manifest do rollbacku.

Pierwszym fixture'em jest `research.summary`: deklaratywny graf
`web.fetch → context.read → summary.create`. Generowanie WASM, subprocessów
lub dowolnego kodu pozostaje osobną, późniejszą klasą rozszerzeń i nie jest
częścią tego kontraktu.

Test integracyjny `victor-runtime/tests/skill_flow.rs` odtwarza przepływ
`manifest → Approved → Active → policy → web.fetch → context.read →
summary.create → SQLite → audit`. Executor web jest atrapą deterministyczną,
ale korzysta z produkcyjnego manifestu `victor-fetch-bald`; dzięki temu test nie
wykonuje sieci, a nadal wymusza granicę allowlisty. Test obejmuje także stany
Draft/Disabled, zatrzymanie po odmowie, izolację scope, rollback hasha i
odtworzenie aktywnego statusu po restarcie SQLite.

### Skill Forge

`victor-skill-forge` jest wyłącznie builderem deklaracji. Przyjmuje opis
użytkownika oraz ograniczony recipe kroków, ale każdy krok musi wskazywać
istniejące narzędzie z `ToolRegistry`; Forge odrzuca R1+ i nie posiada API
dodawania narzędzi, capabilities ani kodu wykonywalnego. Tworzy stan `Draft`,
dry-run waliduje kopię registry bez uruchamiania executorów, a przejścia przez
`Validated`, `Approved` i `Active` deleguje do `victor-skills`.

Warstwa produktowa zwraca `SkillReview` z manifestem, wynikiem dry-run i
strukturalnym `ManifestDiff`, dzięki czemu UI/API może pokazać zmianę przed
aktywacją. Decyzje `approve`, `activate`, `disable` i `rollback` są zapisywane
jako `SkillDecisionAudit` w osobnej tabeli SQLite; nie są mieszane z audytem
wykonania narzędzi. Draft i dry-run nie mają dostępu do executorów ani do
zapisu wyników.

`victor-skill-api` jest lokalnym, UI-agnostycznym interfejsem dla listy
Draftów/aktywnych wersji, review, lifecycle i historii auditów. Nie wykonuje
skillów i nie ufa stanowi przesłanemu przez UI: przed zmianą zawsze pobiera
manifest z registry, ponownie wykonuje walidację policy i zapisuje decyzję
przez backend auditowy.

### Transport lokalny

Pierwszym transportem dla klientów UI jest `victor-skill-transport` na Unix
domain socket. Serwer nie otwiera portu TCP i nie binduje się do interfejsu
sieciowego. Każde żądanie wymaga tokena sesji oraz zgodnego scope klienta;
obowiązują limit rozmiaru, timeout i idempotency key dla operacji lifecycle.
Ponowienie `approve`, `disable` lub `rollback` z tym samym kluczem zwraca
zapamiętaną odpowiedź zamiast wykonywać zmianę ponownie. Testy obejmują klienta
nieautoryzowanego, zły scope i retry approve.

### Klient referencyjny

`victor-skill-cli` jest celowo cienki: nie zawiera registry, policy ani
executorów. Komendy `list`, `review`, `draft`, `update`, `approve`, `disable`, `rollback`
i `audit` tylko serializują żądanie do Unix socketu i renderują odpowiedź
backendu. Może zostać zastąpiony przez Tauri, egui albo inny klient bez zmiany
granic bezpieczeństwa.

Smoke test transportu wykonuje pełny przepływ z uruchomionym serwerem: draft,
review, approve, list, audit, bezpieczne przygotowanie drugiej wersji, rollback,
restart i list. Po restarcie nowy `SkillApi::new` automatycznie ładuje registry
z ponownie otwartego SQLite store; test nie wywołuje ręcznego restore. Audit
pozostaje dostępny z tego samego SQLite store, a test obejmuje również ponowną
aktywację wersji przywróconej przez rollback.

## Otwarte punkty P0

0. Ustalić procedurę aktualizacji vendored `bse-fetch` z BALD upstream. Nie jest
   to zależność builda: V.I.C.T.O.R. buduje się z własnego snapshotu i nie
   wymaga checkoutu ani repozytorium BALD.
1. Pisemna zgoda na JFP Box v0.2 (PolyForm NC) albo zastąpienie go własnym
   walidatorem manifestu.
2. Decyzja: `viper-boxd` czy Rust Lab jako jedyny backend izolacji.
3. Potwierdzenie, czy AionCore session FSM wchodzi jako crate, czy tylko jako
   wzorzec (rekomendacja: wzorzec + własny `victor-runtime`, żeby nie ciągnąć
   całego grafu zależności).
