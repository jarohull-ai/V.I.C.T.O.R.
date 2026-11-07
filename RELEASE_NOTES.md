# V.I.C.T.O.R. v0.1.0-rc.2

Status: kandydat przygotowany do akceptacji właściciela. Taga ani GitHub
Release nie utworzono i nie wypchnięto.

## Zweryfikowany target

- aktualny `HEAD == origin/main`:
  `86dc4d06c0df9b35cf4476f65d17bf1d11c835fa`;
- historia zawiera `407b240` (korekta release notes), `5ed2022`
  (trwałe idempotency) i `86dc4d0` (weryfikacja taga rc.1);
- istniejący tag `v0.1.0-rc.1` pozostaje bez zmian i wskazuje:
  `cd4a326ddeba8cba5fbb51192be3f89575620ced`;
- proponowany `v0.1.0-rc.2` ma wskazywać wyłącznie zweryfikowany HEAD:
  `86dc4d06c0df9b35cf4476f65d17bf1d11c835fa`.

## Zakres rc.2

Rc.2 zawiera lokalny, fail-closed fundament V.I.C.T.O.R. oraz zamyka lukę
trwałego replay protection:

- rozmowa tekstowa i lokalny model przez ograniczony JSON-over-stdio;
- lokalne STT Whisper oraz lokalne TTS przez zaufane procesy;
- przepływ `policy → runtime → plugin → context/audit`;
- read-only `plugin_sysinfo`, `files.list`, `files.read` i `files.inspect`;
- deklaratywne skills z lifecycle, review, approval, rollback i SQLite;
- daemon, prywatny Unix socket, auth/scope i restore po restarcie;
- `transport_idempotency` w tym samym SQLite co stan daemona;
- rekord idempotency związany z principalem, scope i pełnym request hash;
- status i odpowiedź ograniczone do 4 MiB, TTL 24 godziny i purge wygasłych
  rekordów;
- atomowy zapis rekordu idempotency i audytu transportu;
- replay po restarcie zwraca tę samą odpowiedź bez ponownego wykonania;
- bezpieczna instalacja użytkownika bez `sudo`, ścieżki XDG i opcjonalny
  user-level systemd;
- vendored `bse-fetch` wyłącznie przez `vendor/bse-fetch`.

Nie dodano HTTP, WASM, dynamicznych bibliotek, providerów chmurowych ani
arbitralnego wykonywania kodu.

## Weryfikacja clean checkout

Przeszły wszystkie kontrole release:

```text
cargo build --release --workspace --all-features --locked       OK
cargo test --workspace --all-features --locked                 OK
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings  OK
cargo fmt --all --check                                        OK
python3 scripts/local/test_install.py                        OK
python3 scripts/ci/check_repo.py                              OK
git diff --check                                                OK
sha256sum -c target/release/SHA256SUMS                         OK
```

## Artefakty

Manifest `target/release/SHA256SUMS` należy publikować razem z dokładnie tymi
binariami zbudowanymi dla tego kandydata.

```text
82ae244b6d44001087c18f435a5a5336385d9048184dd32d31af069872627598  target/release/victor-daemon
1c8f46a166de1a203e3f43b9616faa2f4f31340954b0317d998e881971023c1d  target/release/victor-skill
```

## Instalacja i wymagania operatora

```sh
python3 scripts/local/install.py install --release-dir target/release --systemd-user
systemctl --user daemon-reload
systemctl --user enable --now victor.service
```

Instalator nie używa `sudo`, wymaga `XDG_RUNTIME_DIR`, odrzuca symlinki,
obcego właściciela i niebezpieczne uprawnienia, a token zapisuje jako 0600.
Trusted definitions pozostają placeholderami do ręcznego uzupełnienia i
zweryfikowania. Whisper.cpp, lokalny model, TTS oraz bubblewrap są wymaganiami
operatora; instalator ich nie pobiera.

## Przygotowanie taga po akceptacji właściciela

Nie wykonano tej komendy:

```sh
git tag -a v0.1.0-rc.2 -m "V.I.C.T.O.R. v0.1.0-rc.2" 86dc4d06c0df9b35cf4476f65d17bf1d11c835fa
```

Po utworzeniu taga push i GitHub Release wymagają osobnej, wyraźnej decyzji
właściciela. Nie używać force push i nie zmieniać `v0.1.0-rc.1`.
