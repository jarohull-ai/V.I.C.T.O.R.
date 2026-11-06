use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::Value;
use victor_skill_forge::ForgeRequest;
use victor_skill_transport::{SkillOperation, SkillRequest, SkillResponse};
use victor_skills::{SkillId, SkillManifest};

fn usage() -> &'static str {
    "victor-skill --socket PATH --token TOKEN --scope SCOPE <list|review|draft|update|approve|disable|rollback|audit> [args]"
}

fn main() {
    if let Err(error) = run() {
        eprintln!("victor-skill: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    let socket = take_option(&mut args, "--socket")
        .ok_or_else(|| format!("brak --socket PATH\n{}", usage()))?;
    let explicit_token = take_option(&mut args, "--token");
    if explicit_token.is_some() {
        eprintln!("ostrzeżenie: --token może trafić do historii shella lub listy procesów; użyj VICTOR_SKILL_TOKEN");
    }
    let token = explicit_token
        .or_else(|| env::var("VICTOR_SKILL_TOKEN").ok())
        .ok_or_else(|| format!("brak --token TOKEN lub VICTOR_SKILL_TOKEN\n{}", usage()))?;
    let scope = take_option(&mut args, "--scope").unwrap_or_else(|| "default".into());
    let operation = args.first().cloned().ok_or_else(|| String::from(usage()))?;
    let rest = &args[1..];
    let operation = build_operation(&operation, rest)?;
    let request = SkillRequest {
        token,
        scope,
        idempotency_key: idempotency_key(&operation),
        operation,
    };
    let response = send(&PathBuf::from(socket), request)?;
    let rendered = serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?;
    println!("{rendered}");
    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "operacja odrzucona".into()));
    }
    Ok(())
}

fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.remove(index);
    if index < args.len() {
        Some(args.remove(index))
    } else {
        None
    }
}

fn build_operation(name: &str, args: &[String]) -> Result<SkillOperation, String> {
    match name {
        "list" => Ok(SkillOperation::List),
        "review" => Ok(SkillOperation::Review {
            skill_id: skill_id(args, 0)?,
        }),
        "draft" => {
            let path = args
                .first()
                .ok_or("draft wymaga ścieżki do pliku ForgeRequest JSON")?;
            let request: ForgeRequest = serde_json::from_str(
                &fs::read_to_string(path).map_err(|e| format!("odczyt {path}: {e}"))?,
            )
            .map_err(|e| format!("nieprawidłowy ForgeRequest: {e}"))?;
            Ok(SkillOperation::CreateDraft { request })
        }
        "update" => {
            let path = args
                .first()
                .ok_or("update wymaga ścieżki do pliku SkillManifest JSON")?;
            let manifest: SkillManifest = serde_json::from_str(
                &fs::read_to_string(path).map_err(|e| format!("odczyt {path}: {e}"))?,
            )
            .map_err(|e| format!("nieprawidłowy SkillManifest: {e}"))?;
            Ok(SkillOperation::Update { manifest })
        }
        "approve" | "disable" | "rollback" => {
            let skill_id = skill_id(args, 0)?;
            let actor = named_arg(args, "--actor").unwrap_or_else(|| "cli-user".into());
            let reason = named_arg(args, "--reason").unwrap_or_else(|| "CLI decision".into());
            let at_unix_ms = named_arg(args, "--at")
                .ok_or("operacja lifecycle wymaga --at UNIX_MS")?
                .parse()
                .map_err(|_| "--at musi być liczbą UNIX_MS")?;
            match name {
                "approve" => Ok(SkillOperation::ApproveActivate {
                    skill_id,
                    actor,
                    reason,
                    at_unix_ms,
                }),
                "disable" => Ok(SkillOperation::Disable {
                    skill_id,
                    actor,
                    reason,
                    at_unix_ms,
                }),
                _ => Ok(SkillOperation::Rollback {
                    skill_id,
                    actor,
                    reason,
                    at_unix_ms,
                }),
            }
        }
        "audit" => Ok(SkillOperation::AuditHistory {
            skill_id: args
                .first()
                .map(|id| SkillId::parse(id))
                .transpose()
                .map_err(|e| e.to_string())?,
        }),
        _ => Err(format!("nieznana operacja {name}\n{}", usage())),
    }
}

fn skill_id(args: &[String], index: usize) -> Result<SkillId, String> {
    args.get(index)
        .ok_or_else(|| "operacja wymaga SKILL_ID".into())
        .and_then(|id| SkillId::parse(id).map_err(|e| e.to_string()))
}

fn named_arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn idempotency_key(operation: &SkillOperation) -> Option<String> {
    match operation {
        SkillOperation::ApproveActivate {
            skill_id,
            at_unix_ms,
            ..
        }
        | SkillOperation::Disable {
            skill_id,
            at_unix_ms,
            ..
        }
        | SkillOperation::Rollback {
            skill_id,
            at_unix_ms,
            ..
        } => Some(format!("cli:{}:{at_unix_ms}", skill_id)),
        _ => None,
    }
}

fn send(path: &PathBuf, request: SkillRequest) -> Result<SkillResponse, String> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| format!("połączenie z socketem: {e}"))?;
    let payload = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    stream.write_all(&payload).map_err(|e| e.to_string())?;
    stream.write_all(b"\n").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

#[allow(dead_code)]
fn _response_result(response: &SkillResponse) -> Option<&Value> {
    response.result.as_ref()
}
