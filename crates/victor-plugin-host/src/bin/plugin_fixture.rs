use std::io::{self, BufRead, Write};
use victor_plugin_contract::{
    Hello, PluginManifest, PluginToolBinding, Request, Response, WireMessage, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "normal".into());
    let mut out = io::BufWriter::new(io::stdout().lock());
    if mode == "bad-handshake" {
        write_message(
            &mut out,
            WireMessage::Hello(Hello {
                manifest: PluginManifest {
                    protocol_version: 999,
                    plugin_id: "bad".into(),
                    plugin_version: "1".into(),
                    tools: vec![],
                },
            }),
        );
        return;
    }
    if mode == "forbidden-capability" {
        let _ = out.write_all(br#"{"kind":"hello","payload":{"manifest":{"protocol_version":1,"plugin_id":"plugin_sysinfo","plugin_version":"1.0.0","tools":[{"id":"plugin_sysinfo","schema_version":1,"input_schema":{"type":"object"},"output_schema":{"type":"object"},"capabilities":["filesystem_write"]}]}}}
"#);
        let _ = out.flush();
        return;
    }
    if mode == "forbidden-risk" {
        let _ = out.write_all(br#"{"kind":"hello","payload":{"manifest":{"protocol_version":1,"plugin_id":"plugin_sysinfo","plugin_version":"1.0.0","tools":[{"id":"plugin_sysinfo","schema_version":1,"input_schema":{"type":"object"},"output_schema":{"type":"object"},"risk":"r3"}]}}}
"#);
        let _ = out.flush();
        return;
    }
    if mode == "oversized" {
        let _ = out.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
        return;
    }
    if mode == "oversized-no-newline" {
        let _ = out.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]);
        let _ = out.flush();
        return;
    }
    if mode == "malformed" {
        let _ = out.write_all(b"{not-json}\n");
        let _ = out.flush();
        return;
    }
    let manifest = PluginManifest {
        protocol_version: PROTOCOL_VERSION,
        plugin_id: "plugin_sysinfo".into(),
        plugin_version: "1.0.0".into(),
        tools: vec![PluginToolBinding {
            id: "plugin_sysinfo".into(),
            schema_version: 1,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
        }],
    };
    write_message(&mut out, WireMessage::Hello(Hello { manifest }));
    let marker = std::env::args().nth(2);
    if mode == "pid" {
        if let Some(path) = &marker {
            let _ = std::fs::write(path, std::process::id().to_string());
        }
    }
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<WireMessage>(&line) else {
            continue;
        };
        match message {
            WireMessage::HelloAck { .. } if mode == "crash-once" => {
                if let Some(path) = &marker {
                    if std::fs::metadata(path).is_err() {
                        let _ = std::fs::write(path, format!("{}\n", std::process::id()));
                        return;
                    }
                    let _ = std::fs::OpenOptions::new()
                        .append(true)
                        .open(path)
                        .and_then(|mut file| {
                            use std::io::Write;
                            writeln!(file, "{}", std::process::id())
                        });
                }
            }
            WireMessage::Heartbeat { nonce } => {
                write_message(&mut out, WireMessage::HeartbeatAck { nonce })
            }
            WireMessage::Request(Request { .. }) if mode == "timeout" => {
                if let Some(path) = &marker {
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .and_then(|mut file| {
                            use std::io::Write;
                            writeln!(file, "{}", std::process::id())
                        });
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            WireMessage::Request(Request { request_id, .. }) if mode == "env" => write_message(
                &mut out,
                WireMessage::Response(Response {
                    request_id,
                    output: serde_json::json!({
                        "secret_present": std::env::var("VICTOR_TEST_SECRET").is_ok()
                    }),
                    summary: "environment checked".into(),
                    finished_at_unix_ms: 2,
                }),
            ),
            WireMessage::Request(Request { request_id, .. }) if mode == "cwd-env" => write_message(
                &mut out,
                WireMessage::Response(Response {
                    request_id,
                    output: serde_json::json!({
                        "cwd": std::env::current_dir().unwrap().display().to_string(),
                        "allowed": std::env::var("VICTOR_ALLOWED").unwrap_or_default(),
                        "secret_present": std::env::var("VICTOR_TEST_SECRET").is_ok()
                    }),
                    summary: "launch spec checked".into(),
                    finished_at_unix_ms: 2,
                }),
            ),
            WireMessage::Request(Request { request_id, .. }) => write_message(
                &mut out,
                WireMessage::Response(Response {
                    request_id,
                    output: serde_json::json!({"cpu_percent": 0.0, "memory_bytes": 0}),
                    summary: "plugin_sysinfo: CPU/RAM odczytane".into(),
                    finished_at_unix_ms: 2,
                }),
            ),
            WireMessage::Shutdown => break,
            _ => {}
        }
    }
}

fn write_message(out: &mut impl Write, message: WireMessage) {
    let bytes = victor_plugin_contract::encode(&message).unwrap();
    let _ = out.write_all(&bytes);
    let _ = out.flush();
}
