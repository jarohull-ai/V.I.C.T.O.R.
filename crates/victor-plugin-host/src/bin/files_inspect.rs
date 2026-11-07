use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use victor_plugin_contract::{
    Hello, PluginManifest, PluginToolBinding, Request, Response, WireMessage, PROTOCOL_VERSION,
};
use victor_policy::PathGuard;

const MAX_ITEMS: usize = 512;
const MAX_DEPTH: usize = 4;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

fn main() {
    let root = argument("--root").unwrap_or_else(|| std::process::exit(2));
    let root = PathBuf::from(root);
    let guard = match PathGuard::new([root.clone()]) {
        Ok(value) => value,
        Err(_) => return,
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    write_message(
        &mut out,
        WireMessage::Hello(Hello {
            manifest: PluginManifest {
                protocol_version: PROTOCOL_VERSION,
                plugin_id: "files.inspect".into(),
                plugin_version: "1.0.0".into(),
                tools: vec![
                    binding(
                        "files.inspect",
                        serde_json::json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
                    ),
                    binding(
                        "files.list",
                        serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
                    ),
                    binding(
                        "files.inspect",
                        serde_json::json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
                    ),
                    binding(
                        "files.read",
                        serde_json::json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
                    ),
                ],
            },
        }),
    );
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<WireMessage>(&line) else {
            continue;
        };
        match message {
            WireMessage::HelloAck { .. } => {}
            WireMessage::Heartbeat { nonce } => {
                write_message(&mut out, WireMessage::HeartbeatAck { nonce })
            }
            WireMessage::Request(Request {
                request_id,
                tool_id,
                args,
            }) => {
                let response = execute(&guard, &root, &tool_id, &args).unwrap_or_else(|message| {
                    WireMessage::Error(victor_plugin_contract::ErrorMessage {
                        request_id: Some(request_id.clone()),
                        code: "files.inspect.denied".into(),
                        message,
                    })
                });
                let response = match response {
                    WireMessage::Response(mut value) => {
                        value.request_id = request_id;
                        WireMessage::Response(value)
                    }
                    other => other,
                };
                write_message(&mut out, response);
            }
            WireMessage::Shutdown => break,
            _ => {}
        }
    }
}

fn binding(id: &str, input_schema: serde_json::Value) -> PluginToolBinding {
    PluginToolBinding {
        id: id.into(),
        schema_version: 1,
        input_schema,
        output_schema: serde_json::json!({"type":"object"}),
    }
}

fn execute(
    guard: &PathGuard,
    root: &Path,
    tool: &str,
    args: &serde_json::Value,
) -> Result<WireMessage, String> {
    let raw = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.to_path_buf());
    let path = guard.validate(&raw).map_err(|e| e.to_string())?;
    match tool {
        "files.inspect" => inspect(guard, &path),
        "files.list" => list(guard, &path),
        "files.read" => read(guard, &path),
        _ => Err("nieznane narzędzie files.inspect".into()),
    }
}

fn inspect(guard: &PathGuard, path: &Path) -> Result<WireMessage, String> {
    let canonical = guard.validate(path).map_err(|e| e.to_string())?;
    let metadata = fs::metadata(&canonical).map_err(|e| e.to_string())?;
    let bytes = if metadata.is_file() {
        fs::read(&canonical).map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    let output = serde_json::json!({"provenance":"files.inspect.local","logical_path":canonical,"kind":if metadata.is_dir(){"directory"}else if metadata.is_file(){"file"}else{"other"},"bytes":metadata.len(),"sha256":hex_hash(&bytes)});
    bounded_response(output, "files.inspect: sprawdzenie zakończone".into())
}

fn list(guard: &PathGuard, root: &Path) -> Result<WireMessage, String> {
    if !root.is_dir() {
        return Err("ścieżka nie jest katalogiem".into());
    }
    let mut entries = Vec::new();
    collect(guard, root, root, 0, &mut entries)?;
    let count = entries.len();
    let output = serde_json::json!({"provenance":"files.inspect.local","root":root,"entries":entries,"count":count});
    bounded_response(output, format!("files.list: {count} elementów"))
}

fn collect(
    guard: &PathGuard,
    root: &Path,
    current: &Path,
    depth: usize,
    entries: &mut Vec<serde_json::Value>,
) -> Result<(), String> {
    if depth > MAX_DEPTH || entries.len() >= MAX_ITEMS {
        return Ok(());
    }
    for item in fs::read_dir(current).map_err(|e| e.to_string())? {
        if entries.len() >= MAX_ITEMS {
            break;
        }
        let item = item.map_err(|e| e.to_string())?;
        let raw = item.path();
        let path = guard.validate(&raw).map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let (kind, size, hash) = if metadata.is_dir() {
            ("directory", 0, None)
        } else if metadata.is_file() {
            ("file", metadata.len(), Some(hash_file(&path)?))
        } else {
            ("other", 0, None)
        };
        entries.push(serde_json::json!({"logical_path":relative,"kind":kind,"bytes":size,"sha256":hash,"provenance":"local.filesystem"}));
        if metadata.is_dir() {
            collect(guard, root, &path, depth + 1, entries)?;
        }
    }
    Ok(())
}

fn read(guard: &PathGuard, path: &Path) -> Result<WireMessage, String> {
    let canonical = guard.validate(path).map_err(|e| e.to_string())?;
    let metadata = fs::symlink_metadata(&canonical).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("files.read wymaga zwykłego pliku".into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("plik przekracza limit 1 MiB".into());
    }
    let bytes = fs::read(&canonical).map_err(|e| e.to_string())?;
    let content = String::from_utf8(bytes.clone())
        .map_err(|_| "plik nie jest poprawnym UTF-8".to_string())?;
    let output = serde_json::json!({"provenance":"files.inspect.local","logical_path":canonical,"bytes":bytes.len(),"sha256":hex_hash(&bytes),"content":content});
    bounded_response(output, format!("files.read: {} bajtów", bytes.len()))
}

fn bounded_response(output: serde_json::Value, summary: String) -> Result<WireMessage, String> {
    let bytes = serde_json::to_vec(&output).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("wynik przekracza limit odpowiedzi".into());
    }
    Ok(WireMessage::Response(Response {
        request_id: String::new(),
        output,
        summary,
        finished_at_unix_ms: 0,
    }))
}
fn hash_file(path: &Path) -> Result<String, String> {
    Ok(hex_hash(&fs::read(path).map_err(|e| e.to_string())?))
}
fn hex_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn argument(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(value) = args.next() {
        if value == name {
            return args.next();
        }
    }
    None
}
fn write_message(out: &mut impl Write, message: WireMessage) {
    if let Ok(bytes) = victor_plugin_contract::encode(&message) {
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }
}
