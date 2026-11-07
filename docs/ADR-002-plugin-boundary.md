# ADR-002: Bezpieczna granica pluginów

## Status

Zaakceptowana.

## Decyzja

Pluginy V.I.C.T.O.R. działają jako osobne procesy uruchamiane przez
`victor-plugin-host` przez stdin/stdout. `victor-plugin-contract` definiuje
wersjonowane wiadomości JSON, jedną ramkę na linię, limit rozmiaru i timeouty.

Manifest wire opisuje wyłącznie identyfikator pluginu, wersję oraz bindingi
narzędzi i schematów. Nie może nadawać sobie capabilities, risk, scope ani
zgody. Te dane pochodzą z lokalnego `victor-tools::ToolManifest` i są oceniane
przez `victor-policy` w `victor-runtime`.

Host nie udostępnia HTTP, proxy, powłoki ani alternatywnej ścieżki wykonania.
Awaria procesu kończy wykonanie; host może wykonać najwyżej jeden kontrolowany
restart. Każde udane wykonanie pozostaje w standardowym przepływie runtime:
manifest → policy → executor → context + audit.

## Odrzucone alternatywy

- WASM i dynamiczne biblioteki — poza pierwszą wersją granicy pluginów;
- generowany kod — nie jest pluginem i nie dostaje zaufania;
- HTTP/plugin proxy — zwiększa powierzchnię ataku i omija lokalny lifecycle;
- capabilities deklarowane przez plugin — plugin nie może być źródłem autoryzacji.

## Konsekwencje

Procesowa awaria jest izolowana od hosta, ale plugin nie ma obietnicy dostępu
do systemu. Dodanie narzędzia wymaga lokalnego manifestu, testów polityki i
rejestracji executora w runtime.
