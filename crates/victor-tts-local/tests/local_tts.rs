use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;
use victor_integrations::{SpeechOutput, SpeechRequest};
use victor_tts_local::{LocalTts, LocalTtsConfig};

fn config(dir: &Path, mode: &str) -> LocalTtsConfig {
    let model = dir.join("model.bin");
    std::fs::write(&model, b"trusted model").unwrap();
    LocalTtsConfig {
        command: vec![env!("CARGO_BIN_EXE_tts_fixture").into(), mode.into()],
        voice: "default".into(),
        model,
        language: "pl".into(),
        working_dir: dir.to_path_buf(),
        timeout: Duration::from_millis(250),
        max_frame_bytes: 4096,
        max_audio_bytes: 64,
        max_stderr_bytes: 1024,
        max_restart_attempts: 0,
        ..LocalTtsConfig::default()
    }
}

fn temp() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn synthesizes_locally_with_deterministic_cache_key() {
    let dir = temp();
    let tts = LocalTts::new(config(dir.path(), "ok")).unwrap();
    let request = SpeechRequest::new("Dzień dobry", "pl");
    let first = tts.synthesize(request.clone()).await.unwrap();
    let second = tts.synthesize(request).await.unwrap();
    assert_eq!(first.bytes, vec![1, 2, 3, 4]);
    assert_eq!(first.cache_key, second.cache_key);
}

#[tokio::test]
async fn rejects_bad_handshake_json_format_and_audio_limit() {
    for (mode, expected) in [
        ("bad-handshake", "nieprawidłowy handshake"),
        ("bad-json", "JSON"),
        ("bad-format", "format lub rozmiar"),
    ] {
        let dir = temp();
        let tts = LocalTts::new(config(dir.path(), mode)).unwrap();
        let error = tts
            .synthesize(SpeechRequest::new("tekst", "pl"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{mode}: {error}");
    }

    let dir = temp();
    let mut cfg = config(dir.path(), "oversized");
    cfg.max_audio_bytes = 2;
    let tts = LocalTts::new(cfg).unwrap();
    assert!(tts
        .synthesize(SpeechRequest::new("tekst", "pl"))
        .await
        .is_err());
}

#[tokio::test]
async fn timeout_and_crash_are_bounded_and_restart_is_explicit() {
    let dir = temp();
    let mut cfg = config(dir.path(), "timeout");
    cfg.timeout = Duration::from_millis(20);
    let tts = LocalTts::new(cfg).unwrap();
    assert!(tts
        .synthesize(SpeechRequest::new("tekst", "pl"))
        .await
        .is_err());

    let marker = dir.path().join("crash.marker");
    let mut cfg = config(dir.path(), "crash-once");
    cfg.command.push(marker.display().to_string());
    cfg.max_restart_attempts = 1;
    let tts = LocalTts::new(cfg).unwrap();
    let audio = tts
        .synthesize(SpeechRequest::new("tekst", "pl"))
        .await
        .unwrap();
    assert_eq!(audio.bytes, vec![1, 2, 3, 4]);
    assert!(marker.exists());
}

#[test]
fn sandbox_is_fail_closed_without_executable() {
    let dir = temp();
    let mut cfg = config(dir.path(), "ok");
    cfg.sandbox_required = true;
    cfg.sandbox_executable = None;
    assert!(LocalTts::new(cfg).is_err());
}
