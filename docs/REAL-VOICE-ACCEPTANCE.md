# Real local voice acceptance

This is the operator-side acceptance test for real local executables. It is
deliberately separate from the fixture-based workspace tests: no model or
executable is downloaded, and a missing or invalid trusted definition returns
`NotConfigured` without cloud fallback.

## Preconditions

Prepare private, user-owned files and reviewed definitions:

- `whisper-cli` and its local model;
- local TTS executable and model;
- `plugin_sysinfo` trusted definition;
- `bubblewrap` when any definition sets `sandbox_required: true`;
- daemon token with mode `0600`, definitions and runtime directories with mode
  `0700`.

The daemon must be started from the release binaries using the trusted
definitions and one SQLite file. The client never supplies an executable,
model, capability, risk or sandbox setting.

## Run

```sh
python3 scripts/local/test_real_voice_acceptance.py \
  --definitions "$XDG_CONFIG_HOME/victor/trusted" \
  --token-file "$XDG_CONFIG_HOME/victor/token" \
  --socket "$XDG_RUNTIME_DIR/victor/victor.sock" \
  --sqlite "$XDG_STATE_HOME/victor/state.sqlite" \
  --audio /path/to/private/16k-mono.wav
```

The script verifies executable/model hashes from trusted definitions, private
permissions, sandbox availability, local Whisper provenance, synthesized local
TTS, SQLite audit, and absence of raw audio in `context_items`. It uses only a
Unix socket and the local filesystem.

## Acceptance criteria

1. `audio → STT → local model → policy/runtime → plugin → SQLite/audit → TTS`
   returns success with `provider=whisper.cpp`, `location=local` and synthesized
   TTS.
2. Restart the daemon with the same definitions and SQLite file, then repeat
   the command. The active definitions, plugin registration, context and audit
   must be restored; a changed executable/model hash must fail closed.
3. Remove or rename an executable/model and repeat. The daemon/test must report
   `NotConfigured`, must not download anything and must not invoke a cloud
   provider.
4. Inspect the SQLite database: context may contain transcript/provenance and
   hashes, but never the input WAV bytes or a raw audio URI.
5. If sandboxing is required but unavailable, startup/execution must be
   denied; the test must not silently continue unsandboxed.

The fixture-based equivalent remains `cargo test --workspace --all-features
--locked`; this acceptance test is the gate for operator-provided executables.
