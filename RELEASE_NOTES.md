# V.I.C.T.O.R. v0.1.0-rc.1

Status: release candidate przygotowany do przeglądu właściciela. Tag nie został
utworzony ani wypchnięty.

## Zakres

Ten release candidate zawiera lokalny, fail-closed fundament V.I.C.T.O.R.:

- rozmowę tekstową i lokalny model przez ograniczony JSON-over-stdio;
- lokalne STT Whisper oraz lokalne TTS przez zaufane procesy;
- przepływ `policy → runtime → plugin → context/audit`;
- read-only `plugin_sysinfo`, `files.list`, `files.read` i `files.inspect`;
- deklaratywne skills z lifecycle, review, approval, rollback i SQLite;
- lokalny daemon, Unix socket, auth/scope, idempotency i restore po restarcie;
- instalację użytkownika bez `sudo`, ścieżki XDG i opcjonalny user-level systemd;
- vendored `bse-fetch` wyłącznie przez `vendor/bse-fetch`.

Nie dodano w tym RC nowych funkcji, HTTP, WASM, dynamicznych bibliotek ani
arbitralnego wykonywania kodu.

## Zweryfikowany stan repozytorium

- `HEAD == origin/main`: `5d6d6db8a1be98d1a8f1bb18996b9bd20396272c`;
- checkout czysty przed przygotowaniem release notes;
- licencja workspace V.I.C.T.O.R.: BUSL-1.1;
- licencja MIT występuje wyłącznie w `vendor/bse-fetch`;
- `scripts/ci/check_repo.py` potwierdza granice repozytorium i licencje;
- Cargo nie wymaga BALD, AionCore, Audioticon, SEED ani zewnętrznego checkoutu;
- brak sekretów w konfiguracjach release; token jest tworzony lokalnie jako
  plik `0600` i nie trafia do argumentów procesu.

## Weryfikacja RC

Przeszły wszystkie wymagane kontrole:

```text
cargo build --release --workspace --all-features --locked       OK
cargo test --workspace --all-features --locked                 OK (155 testów)
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings  OK
cargo fmt --all --check                                        OK
python3 scripts/local/test_install.py                        OK
python3 scripts/ci/check_repo.py                              OK
git diff --check                                                OK
sha256sum -c target/release/SHA256SUMS                          OK
```

## Artefakty

Manifest znajduje się w `target/release/SHA256SUMS`.

```text
6d84106bbbc6b2d0989d411c34192ed0468358d3cb1930474c54ffc2d0324b97  target/release/victor-daemon
7b362b9535df555cf48c7cd9a5a74168a6acd8a8a9a54ec158ceb8728e88d966  target/release/victor-skill
```

Sumy dotyczą binariów zbudowanych przez `cargo build --release --workspace
--all-features --locked`. Manifest należy publikować razem z dokładnie tymi
artefaktami.

## Instalacja i wymagania operatora

Instalacja jest lokalna i nie używa `sudo`:

```sh
python3 scripts/local/install.py install --release-dir target/release --systemd-user
systemctl --user daemon-reload
systemctl --user enable --now victor.service
```

Instalator wymaga `XDG_RUNTIME_DIR`; nie używa wspólnego `/tmp`. Tworzy:

- konfigurację w `XDG_CONFIG_HOME/victor`;
- stan SQLite w `XDG_STATE_HOME/victor`;
- prywatny runtime/socket w `XDG_RUNTIME_DIR/victor`;
- binaria w `XDG_DATA_HOME/victor/bin`.

Katalogi mają `0700`, token `0600`, a unit systemd jest generowany bez
automatycznego startu, jeśli nie podano polecenia operatora. Trusted definitions
muszą zostać ręcznie uzupełnione i zweryfikowane przed uruchomieniem.

Operator musi dostarczyć lokalne, zaufane executable i modele dla Whispera,
lokalnego modelu oraz TTS. Definicje muszą zawierać poprawne hashe executable i
modeli; wymagający izolacji proces odmówi startu bez dostępnego sandboxa.
Instalator niczego nie pobiera i nie zapisuje sekretów do konfiguracji.

## Znane ograniczenia i ryzyka

- To release candidate, nie deklaracja gotowości produkcyjnej.
- Lokalna jakość i wydajność STT/modelu/TTS zależy od dostarczonych binariów,
  modeli i zasobów hosta.
- Sandbox procesów zależy od jawnie skonfigurowanego i zweryfikowanego
  mechanizmu systemowego, np. bubblewrap; brak mechanizmu oznacza odmowę.
- Placeholdery trusted definitions nie są gotową konfiguracją produkcyjną.
- User-level systemd nie zastępuje polityki V.I.C.T.O.R. ani sandboxa procesów.
- Instalator zachowuje SQLite, token i konfigurację przy odinstalowaniu; ich
  usunięcie wymaga osobnej, świadomej operacji.
- `--token` nie jest używany przez instalator; token ma pozostać poza historią
  shella i listą procesów, preferowane są plik `0600` lub bezpieczny FD.

## Przygotowanie taga po akceptacji właściciela

Nie wykonano tej komendy. Po wyraźnym potwierdzeniu właściciela:

```sh
git tag -a v0.1.0-rc.1 -m "V.I.C.T.O.R. v0.1.0-rc.1" 5d6d6db8a1be98d1a8f1bb18996b9bd20396272c
git push origin v0.1.0-rc.1
```

Komenda nie używa force push.
