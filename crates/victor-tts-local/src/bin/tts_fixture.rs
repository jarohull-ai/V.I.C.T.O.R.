use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "ok".into());
    let mut out = io::BufWriter::new(io::stdout().lock());
    match mode.as_str() {
        "bad-handshake" => {
            writeln!(out, "{{\"kind\":\"hello\",\"protocol_version\":99}}").unwrap();
            out.flush().unwrap();
            std::process::exit(0);
        }
        "bad-json" => {
            writeln!(out, "{{\"kind\":\"hello\",\"protocol_version\":1}}").unwrap();
            out.flush().unwrap();
            for line in io::stdin().lock().lines().map_while(Result::ok) {
                if line.contains("synthesize") {
                    writeln!(out, "{{not-json").unwrap();
                    out.flush().unwrap();
                    return;
                }
            }
        }
        "timeout" => {
            writeln!(out, "{{\"kind\":\"hello\",\"protocol_version\":1}}").unwrap();
            out.flush().unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        "crash" => std::process::exit(23),
        "crash-once" => {
            let marker = std::env::args().nth(2).expect("marker");
            if std::fs::File::open(&marker).is_err() {
                std::fs::write(&marker, b"used").unwrap();
                std::process::exit(23);
            }
            write_ok(&mut out);
        }
        "bad-format" => write_audio(&mut out, 0, vec![1, 2, 3]),
        "oversized" => write_audio(&mut out, 16_000, vec![0; 128]),
        _ => write_ok(&mut out),
    }
}

fn write_ok(out: &mut impl Write) {
    write_audio(out, 16_000, vec![1, 2, 3, 4]);
}

fn write_audio(out: &mut impl Write, sample_rate_hz: u32, bytes: Vec<u8>) {
    writeln!(out, "{{\"kind\":\"hello\",\"protocol_version\":1}}").unwrap();
    out.flush().unwrap();
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        if line.contains("synthesize") {
            let body = serde_json::json!({
                "kind": "audio",
                "format": "wav",
                "sample_rate_hz": sample_rate_hz,
                "bytes": bytes,
            });
            writeln!(out, "{body}").unwrap();
            out.flush().unwrap();
            return;
        }
    }
}
