use std::io::{self, BufRead, Write};
use victor_plugin_contract::{
    Hello, PluginManifest, PluginToolBinding, Request, Response, WireMessage, PROTOCOL_VERSION,
};

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();
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
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<WireMessage>(&line) else {
            continue;
        };
        match message {
            WireMessage::HelloAck { .. } => {}
            WireMessage::Heartbeat { nonce } => {
                write_message(&mut out, WireMessage::HeartbeatAck { nonce })
            }
            WireMessage::Request(Request { request_id, .. }) => write_message(
                &mut out,
                WireMessage::Response(Response {
                    request_id,
                    output: serde_json::json!({"cpu_percent": 0.0, "memory_bytes": 0}),
                    summary: "plugin_sysinfo: CPU/RAM odczytane".into(),
                    finished_at_unix_ms: 1,
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
