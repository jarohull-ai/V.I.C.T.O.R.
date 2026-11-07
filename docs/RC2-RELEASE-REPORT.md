# RC2 release report

## Identity and publication boundary

- candidate: `v0.1.0-rc.2`
- tag target: `86dc4d06c0df9b35cf4476f65d17bf1d11c835fa`
- `v0.1.0-rc.1` remains unchanged at `cd4a326ddeba8cba5fbb51192be3f89575620ced`
- GitHub Release: not created; publication requires explicit owner approval
- license: source-available BUSL-1.1; `vendor/bse-fetch` remains MIT with its own
  `LICENSE` and `ORIGIN.md`

## Release artifacts

Build from a clean checkout, then generate the manifest without configuration,
tokens, models or audio:

```sh
cargo build --release --workspace --all-features --locked
python3 scripts/local/sha256_manifest.py --output target/release/SHA256SUMS \
  target/release/victor-daemon target/release/victor-skill
sha256sum -c target/release/SHA256SUMS
```

The manifest is the publication authority for the two binaries. It must be
uploaded beside the exact files used to generate it.

## Verification record

The release gate consists of workspace build/test, Clippy with warnings denied,
rustfmt check, repository/license/path-dependency checker, installer tests,
trusted-definition tests, diff check and checksum verification. The operator
validator is read-only and reports only definition identifiers, hash values and
environment key names, never environment values.

The deterministic voice smoke test uses fixtures and no network. On this host
`/usr/bin/bwrap` is available, while `whisper-cli`, a Whisper model and a local
TTS executable/model are not configured. Therefore the real executable test is
**NotConfigured**, with no download and no cloud fallback. This is an explicit
safe state, not a passing claim for a real audio engine.

Before accepting a real run, the operator must provide trusted definitions for
all three local components, verify executable/model hashes with
`scripts/local/validate_trusted.py`, and confirm daemon restart, provenance,
SQLite audit/context records and absence of raw input/output audio in SQLite.
