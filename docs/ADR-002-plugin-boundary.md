# ADR-002: Bezpieczna granica pluginów

## Status

Zaakceptowana.

## Decyzja

Pluginy V.I.C.T.O.R. działają jako osobne procesy uruchamiane przez
`victor-plugin-host` przez stdin/stdout. `victor-plugin-contract` definiuje
wersjonowane wiadomości JSON, jedną ramkę na linię, limit rozmiaru i timeouty.
Host czyta ramki limitowanym readerem: przekroczenie limitu przed newline,
EOF bez kompletnej ramki i uszkodzony JSON są błędami fail-closed.

Manifest wire opisuje wyłącznie identyfikator pluginu, wersję oraz bindingi
narzędzi i schematów. Nie może nadawać sobie capabilities, risk, scope ani
zgody. Te dane pochodzą z lokalnego `victor-tools::ToolManifest` i są oceniane
przez `victor-policy` w `victor-runtime`.

Host nie udostępnia HTTP, proxy, powłoki ani alternatywnej ścieżki wykonania.
Child startuje z wyczyszczonym środowiskiem, jawny jest katalog roboczy, a
sekrety i zmienne proxy nie są dziedziczone. Awaria procesu kończy wykonanie;
host może wykonać najwyżej jeden kontrolowany restart, po uprzednim kill + wait
starego procesu. Restart może zostać zapisany w `AuditSink`. Każde udane
wykonanie pozostaje w standardowym przepływie runtime:
manifest → policy → executor → context + audit.

`PersistentPluginHost` przechowuje lokalny `ToolManifest`, jego SHA-256,
komendę, wymaganie sandboxa i status `active`/`disabled` w SQLite. Przy
restarcie host odtwarza wyłącznie aktywne wpisy i odmawia startu, gdy JSON,
identyfikator lub hash manifestu nie pasują. Po rejestracji executor jest
również wpisywany do `ToolRegistry`; plugin nie ma osobnej ścieżki omijającej
policy.

### Granica sandboxa

`PluginLaunchPolicy` rozróżnia plugin niewymagający izolacji od pluginu, który
wymaga sandboxa. Dla tego drugiego host używa wykrytego `bubblewrap` (lub jawnie
wskazanej ścieżki), z `--die-with-parent`, `--clearenv`, `--unshare-net`,
read-only rootem i tymczasowym `/tmp`. Brak dostępnego bubblewrap kończy się
odmową uruchomienia. To jest granica systemowa, nie obietnica sandboxa w samym
Rust; WASM i dynamiczne biblioteki nie są obsługiwane.

## Odrzucone alternatywy

- WASM i dynamiczne biblioteki — poza pierwszą wersją granicy pluginów;
- generowany kod — nie jest pluginem i nie dostaje zaufania;
- HTTP/plugin proxy — zwiększa powierzchnię ataku i omija lokalny lifecycle;
- capabilities deklarowane przez plugin — plugin nie może być źródłem autoryzacji.

## Konsekwencje

Procesowa awaria jest izolowana od hosta, ale plugin nie ma obietnicy dostępu
do systemu. Dodanie narzędzia wymaga lokalnego manifestu, testów polityki i
rejestracji executora w runtime.
