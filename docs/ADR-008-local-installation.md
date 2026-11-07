# ADR-008 — Bezpieczna instalacja lokalna

**Status:** przyjęta  
**Data:** 7 listopada 2026 r.

## Decyzja

V.I.C.T.O.R. dostarcza instalator użytkownika bez `sudo`, napisany wyłącznie w
standardowej bibliotece Pythona. Instalator kopiuje binaria release do
`XDG_DATA_HOME/victor/bin`, trusted definitions do
`XDG_CONFIG_HOME/victor/trusted`, SQLite do `XDG_STATE_HOME/victor`, a socket
do `XDG_RUNTIME_DIR/victor`.

Nie ma fallbacku do `/tmp`, nie ma pobierania zależności, providerów ani modeli.
Istniejące katalogi i pliki są sprawdzane przez `lstat`: symlinki, obcy
właściciel, pliki zamiast katalogów oraz dostęp grupy/innych kończą instalację
fail-closed. Katalogi są 0700, token 0600. Token i trusted definitions nie są
nadpisywane przy ponownym uruchomieniu.

## Systemd i sandbox

Opcjonalny user-level unit jest tylko generowany. Instalator nie wykonuje
`enable`, `start` ani operacji wymagających roota. `NoNewPrivileges`,
`PrivateTmp`, `ProtectSystem=strict` i `ProtectHome=read-only` ograniczają
daemon, ale nie zastępują sandboxa procesów. Wymaganie bubblewrap pozostaje
jawne w trusted definitions i jest egzekwowane przez daemon; whisper.cpp,
lokalny model i TTS muszą być dostarczone i zweryfikowane przez operatora.

Odinstalowanie usuwa tylko zainstalowane binaria oraz opcjonalny unit. Stan,
token i konfiguracja są zachowywane, aby uniknąć nieodwracalnego usunięcia
danych bez osobnej decyzji użytkownika.
