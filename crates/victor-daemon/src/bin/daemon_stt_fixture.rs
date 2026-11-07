use std::env;
use std::fs;

fn main() {
    let args: Vec<String> = env::args().collect();
    let prefix = args
        .windows(2)
        .find(|pair| pair[0] == "-of")
        .map(|pair| pair[1].clone())
        .expect("-of");
    fs::write(
        format!("{prefix}.json"),
        r#"{"transcription":[{"text":"pokaż status systemu"}],"result":{"language":"pl"}}"#,
    )
    .unwrap();
}
