use std::io::{self, BufRead, Write};

fn main() {
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(out, r#"{{"kind":"hello","protocol_version":1}}"#).unwrap();
    out.flush().unwrap();
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        if line.contains("synthesize") {
            writeln!(
                out,
                r#"{{"kind":"audio","format":"wav","sample_rate_hz":16000,"bytes":[1,2,3,4]}}"#
            )
            .unwrap();
            out.flush().unwrap();
            return;
        }
    }
}
