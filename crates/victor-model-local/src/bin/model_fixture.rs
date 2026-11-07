use std::io::{self, BufRead, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "reply".into());
    let mut out = io::BufWriter::new(io::stdout().lock());
    let hello = if mode == "bad-handshake" {
        r#"{"kind":"hello","protocol_version":999,"model_id":"fixture"}"#
    } else {
        r#"{"kind":"hello","protocol_version":1,"model_id":"fixture"}"#
    };
    writeln!(out, "{hello}").unwrap();
    out.flush().unwrap();
    let stdin = io::stdin();
    for _line in stdin.lock().lines().map_while(Result::ok) {
        if mode == "bad-json" {
            writeln!(out, "{{not-json}}").unwrap();
            out.flush().unwrap();
            continue;
        }
        if mode == "injection" {
            let response = r#"{"kind":"plan_request","tool":"plugin_sysinfo","description":"x","args":{},"capabilities":["filesystem_write"]}"#;
            writeln!(out, "{response}").unwrap();
            out.flush().unwrap();
            continue;
        }
        if mode == "timeout-once" {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        if mode == "disconnect" {
            return;
        }
        if mode == "oversized" {
            writeln!(out, "{}", "x".repeat(200_000)).unwrap();
            out.flush().unwrap();
            continue;
        }
        let response = if mode == "plan" {
            r#"{"kind":"plan_request","tool":"plugin_sysinfo","description":"status","args":{}}"#
        } else if mode == "files" {
            r#"{"kind":"plan_request","tool":"files.list","description":"files","args":{}}"#
        } else {
            r#"{"kind":"reply","text":"lokalnie"}"#
        };
        writeln!(out, "{response}").unwrap();
        out.flush().unwrap();
    }
}
