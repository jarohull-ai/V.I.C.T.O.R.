# V.I.C.T.O.R. — release readiness

Aktualny kandydat: `v0.1.0-rc.2` na commitcie
`86dc4d06c0df9b35cf4476f65d17bf1d11c835fa`. Istniejący `v0.1.0-rc.1` wskazuje
bez zmian `cd4a326ddeba8cba5fbb51192be3f89575620ced`.

## Clean checkout gate

Uruchamiane z katalogu repozytorium, bez zależności od innych checkoutów:

```sh
python3 scripts/ci/check_repo.py
cargo build --release --workspace --all-features --locked
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all --check
git diff --check
python3 scripts/local/test_install.py
python3 scripts/local/sha256_manifest.py --output target/release/SHA256SUMS \
  target/release/victor-daemon target/release/victor-skill
sha256sum target/release/victor-daemon target/release/victor-skill
```

Instalator testowy działa wyłącznie w `tempfile.TemporaryDirectory`, nie używa
sudo, nie aktywuje usług i nie modyfikuje katalogów użytkownika.

## Instalacja lokalna

Po zbudowaniu release:

```sh
python3 scripts/local/install.py install --release-dir target/release
```

Wymagane jest ustawione `XDG_RUNTIME_DIR`; projekt celowo nie używa wspólnego
`/tmp`. Domyślne lokalizacje to:

- `XDG_CONFIG_HOME/victor/trusted` — trusted definitions i token 0600;
- `XDG_STATE_HOME/victor/state.sqlite` — stan SQLite;
- `XDG_RUNTIME_DIR/victor` — prywatny socket i katalog 0700;
- `XDG_DATA_HOME/victor/bin` — binaria release.

Brakujące katalogi są tworzone z trybem 0700. Istniejące symlinki, obcy
właściciel, pliki zamiast katalogów i uprawnienia grupy/innych kończą instalację
odmową. Instalator nie nadpisuje tokena ani istniejących trusted definitions.
Pliki `.json` są bezpiecznymi placeholderami: przed startem trzeba podać
zweryfikowane ścieżki executable/modeli i ich hashe. Nie wolno wpisywać sekretów
do definicji ani środowiska procesu.

Odinstalowanie usuwa tylko binaria z katalogu instalacji:

```sh
python3 scripts/local/install.py uninstall
```

Konfiguracja, token i SQLite są zachowywane. Usuwanie danych jest osobną,
świadomą czynnością użytkownika.

## User-level systemd

Opcjonalnie można wygenerować unit bez roota:

```sh
python3 scripts/local/install.py install --release-dir target/release --systemd-user
systemctl --user daemon-reload
systemctl --user enable --now victor.service
```

Instalator nie włącza usługi automatycznie. Unit używa `NoNewPrivileges`,
`PrivateTmp`, `ProtectSystem=strict`, `ProtectHome=read-only` i ograniczonych
`ReadWritePaths`. To nie zastępuje sandboxa pluginu/modelu/STT/TTS: jeśli trusted
definition wymaga bubblewrap, `/usr/bin/bwrap` musi istnieć i być osobno
zweryfikowany przez daemon. Whisper.cpp, lokalny model i TTS są wymaganiami
operatora; instalator ich nie pobiera i nie uruchamia.

## Release checklist

- [ ] wersja workspace i `Cargo.lock` są zgodne i zapisane w commicie;
- [ ] root `LICENSE` pozostaje BUSL-1.1;
- [ ] `vendor/bse-fetch` zachowuje MIT, `LICENSE` i `ORIGIN.md`;
- [ ] `scripts/ci/check_repo.py` nie wykrywa path dependency poza workspace;
- [ ] brak sekretów, tokenów i ścieżek do innych checkoutów;
- [ ] pełny gate clean-checkout przechodzi;
- [ ] binaria i konfiguracje release mają opublikowane SHA-256;
- [ ] smoke test voice przechodzi na fixture bez sieci;
- [x] trwała ochrona idempotency po restarcie jest zamknięta przed RC.

## RC.2 tag command

Po akceptacji właściciela, bez force push:

```sh
git tag -a v0.1.0-rc.2 -m "V.I.C.T.O.R. v0.1.0-rc.2" 86dc4d06c0df9b35cf4476f65d17bf1d11c835fa
```

Nie tworzyć taga ani GitHub Release automatycznie.

Manifest sum jest deterministyczny, sortowany po nazwie artefaktu i odrzuca
symlinki. Generuj go dopiero po zakończeniu builda release; nie umieszczaj w nim
tokenów, trusted definitions ani plików konfiguracyjnych zawierających sekrety.
