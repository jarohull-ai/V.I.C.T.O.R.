use std::io::{self, BufRead, Write};

fn main() {
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(
        out,
        r#"{{"kind":"hello","protocol_version":1,"model_id":"daemon-fixture"}}"#
    )
    .unwrap();
    out.flush().unwrap();
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        if line.contains("hello_ack") {
            continue;
        }
        let response = if line.contains("pliki") || line.contains("files") {
            r#"{"kind":"plan_request","tool":"files.list","description":"lista plików","args":{}}"#
        } else {
            r#"{"kind":"plan_request","tool":"plugin_sysinfo","description":"status systemu","args":{}}"#
        };
        writeln!(out, "{response}").unwrap();
        out.flush().unwrap();
    }
}
