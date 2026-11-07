use std::io::{self, BufRead, Write};
use victor_plugin_contract::{
    Hello, PluginManifest, PluginToolBinding, Request, Response, WireMessage, PROTOCOL_VERSION,
};

fn main() {
    let files = std::env::args().nth(1).as_deref() == Some("files");
    let mut out = io::BufWriter::new(io::stdout().lock());
    let manifest = PluginManifest {
        protocol_version: PROTOCOL_VERSION,
        plugin_id: if files {
            "files.list"
        } else {
            "plugin_sysinfo"
        }
        .into(),
        plugin_version: "1.0.0".into(),
        tools: vec![PluginToolBinding {
            id: if files {
                "files.list"
            } else {
                "plugin_sysinfo"
            }
            .into(),
            schema_version: 1,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
        }],
    };
    write(&mut out, WireMessage::Hello(Hello { manifest }));
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<WireMessage>(&line) else {
            return;
        };
        match message {
            WireMessage::Request(Request { request_id, .. }) => write(
                &mut out,
                WireMessage::Response(Response {
                    request_id,
                    output: if files {
                        serde_json::json!({"provenance":"files.inspect.local","count":0,"entries":[]})
                    } else {
                        serde_json::json!({"cpu_percent":0.0,"memory_bytes":0})
                    },
                    summary: if files {
                        "lista plików".into()
                    } else {
                        "status systemu".into()
                    },
                    finished_at_unix_ms: 1,
                }),
            ),
            WireMessage::Heartbeat { nonce } => {
                write(&mut out, WireMessage::HeartbeatAck { nonce })
            }
            WireMessage::Shutdown => return,
            _ => {}
        }
    }
}

fn write(out: &mut impl Write, message: WireMessage) {
    out.write_all(&victor_plugin_contract::encode(&message).unwrap())
        .unwrap();
    out.flush().unwrap();
}
