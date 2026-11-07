use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};
use victor_domain::{Approval, ToolId};
use victor_plugin_host::{PluginLaunchRequest, SandboxRequirement};
use victor_skill_forge::ForgeRequest;
use victor_skill_transport::{PluginOperation, SkillOperation, SkillRequest, SkillResponse};
use victor_skills::{SkillId, SkillManifest};

fn usage() -> &'static str {
    "victor-skill --socket PATH --token TOKEN --scope SCOPE <chat|list|review|draft|update|approve|disable|rollback|audit|plugin> [args]"
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
        "chat" => {
            let conversation_id = args.first().ok_or("chat wymaga CONVERSATION_ID")?.clone();
            let at_unix_ms = named_arg(args, "--at")
                .ok_or("chat wymaga --at UNIX_MS")?
                .parse()
                .map_err(|_| "--at musi być liczbą UNIX_MS")?;
            let input = args[1..]
                .iter()
                .take_while(|arg| arg.as_str() != "--at")
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            if input.trim().is_empty() {
                return Err("chat wymaga tekstu wiadomości".into());
            }
            Ok(SkillOperation::Chat {
                conversation_id,
                input,
                at_unix_ms,
                approval: None,
            })
        }
        "plugin" => Ok(SkillOperation::Plugin {
            plugin_operation: build_plugin_operation(args)?,
        }),
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

fn build_plugin_operation(args: &[String]) -> Result<PluginOperation, String> {
    let action = args.first().ok_or("plugin wymaga akcji")?;
    let rest = &args[1..];
    match action.as_str() {
        "list" => Ok(PluginOperation::List),
        "review" => Ok(PluginOperation::Review {
            plugin_id: plugin_id(rest, 0)?,
        }),
        "register" => {
            let manifest_path = rest.first().ok_or("plugin register wymaga MANIFEST_JSON")?;
            let executable = named_arg(rest, "--executable")
                .ok_or("plugin register wymaga --executable PATH")?;
            let working_dir = named_arg(rest, "--working-dir")
                .ok_or("plugin register wymaga --working-dir PATH")?;
            let manifest: victor_tools::ToolManifest = serde_json::from_str(
                &fs::read_to_string(manifest_path)
                    .map_err(|error| format!("odczyt {manifest_path}: {error}"))?,
            )
            .map_err(|error| format!("nieprawidłowy ToolManifest: {error}"))?;
            Ok(PluginOperation::Register {
                manifest: Box::new(manifest),
                launch: Box::new(launch_request(rest, executable, working_dir)?),
                approval: approval_arg(rest)?,
                actor: named_arg(rest, "--actor").unwrap_or_else(|| "cli-user".into()),
                reason: named_arg(rest, "--reason").unwrap_or_else(|| "CLI decision".into()),
                at_unix_ms: at_arg(rest)?,
            })
        }
        "activate" => Ok(PluginOperation::Activate {
            plugin_id: plugin_id(rest, 0)?,
            approval: approval_arg(rest)?,
            actor: named_arg(rest, "--actor").unwrap_or_else(|| "cli-user".into()),
            reason: named_arg(rest, "--reason").unwrap_or_else(|| "CLI decision".into()),
            at_unix_ms: at_arg(rest)?,
        }),
        "disable" => Ok(PluginOperation::Disable {
            plugin_id: plugin_id(rest, 0)?,
            actor: named_arg(rest, "--actor").unwrap_or_else(|| "cli-user".into()),
            reason: named_arg(rest, "--reason").unwrap_or_else(|| "CLI decision".into()),
            at_unix_ms: at_arg(rest)?,
        }),
        "restart" => Ok(PluginOperation::Restart {
            plugin_id: plugin_id(rest, 0)?,
            approval: approval_arg(rest)?,
            actor: named_arg(rest, "--actor").unwrap_or_else(|| "cli-user".into()),
            reason: named_arg(rest, "--reason").unwrap_or_else(|| "CLI decision".into()),
            at_unix_ms: at_arg(rest)?,
        }),
        "unregister" => Ok(PluginOperation::Unregister {
            plugin_id: plugin_id(rest, 0)?,
            explicit_decision: has_flag(rest, "--confirm"),
            approval: approval_arg(rest)?,
            actor: named_arg(rest, "--actor").unwrap_or_else(|| "cli-user".into()),
            reason: named_arg(rest, "--reason").unwrap_or_else(|| "CLI decision".into()),
            at_unix_ms: at_arg(rest)?,
        }),
        "audit" => Ok(PluginOperation::AuditHistory {
            plugin_id: rest
                .first()
                .map(|id| ToolId::parse(id))
                .transpose()
                .map_err(|e| e.to_string())?,
        }),
        _ => Err(format!("nieznana akcja plugin: {action}")),
    }
}

fn launch_request(
    args: &[String],
    executable: String,
    working_dir: String,
) -> Result<PluginLaunchRequest, String> {
    let mut command = vec![executable];
    let mut environment = std::collections::BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--arg" {
            command.push(args.get(index + 1).ok_or("--arg wymaga wartości")?.clone());
            index += 2;
        } else if args[index] == "--env" {
            let value = args.get(index + 1).ok_or("--env wymaga KEY=VALUE")?;
            let (key, value) = value.split_once('=').ok_or("--env wymaga KEY=VALUE")?;
            environment.insert(key.into(), value.into());
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(PluginLaunchRequest {
        command,
        working_dir,
        environment,
        sandbox: if has_flag(args, "--sandbox-required") {
            SandboxRequirement::Required
        } else {
            SandboxRequirement::NotRequired
        },
        sandbox_executable: named_arg(args, "--sandbox-executable"),
        request_timeout_ms: named_arg(args, "--timeout")
            .unwrap_or_else(|| "30000".into())
            .parse()
            .map_err(|_| "--timeout musi być liczbą")?,
        max_frame_bytes: named_arg(args, "--max-frame")
            .unwrap_or_else(|| "262144".into())
            .parse()
            .map_err(|_| "--max-frame musi być liczbą")?,
        max_restart_attempts: named_arg(args, "--max-restarts")
            .unwrap_or_else(|| "1".into())
            .parse()
            .map_err(|_| "--max-restarts musi być liczbą")?,
    })
}

fn plugin_id(args: &[String], index: usize) -> Result<ToolId, String> {
    args.get(index)
        .ok_or_else(|| "operacja plugin wymaga PLUGIN_ID".into())
        .and_then(|id| ToolId::parse(id).map_err(|e| e.to_string()))
}

fn approval_arg(args: &[String]) -> Result<Option<Approval>, String> {
    named_arg(args, "--approval")
        .map(|path| {
            serde_json::from_str(
                &fs::read_to_string(&path).map_err(|error| format!("odczyt {path}: {error}"))?,
            )
            .map_err(|error| format!("nieprawidłowa Approval: {error}"))
        })
        .transpose()
}

fn at_arg(args: &[String]) -> Result<u64, String> {
    named_arg(args, "--at")
        .ok_or("operacja lifecycle wymaga --at UNIX_MS")?
        .parse()
        .map_err(|_| "--at musi być liczbą UNIX_MS".into())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
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
    if matches!(operation, SkillOperation::Plugin { .. }) {
        let bytes = serde_json::to_vec(operation).ok()?;
        let digest = Sha256::digest(bytes);
        return Some(format!(
            "cli-plugin:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
    }
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
        SkillOperation::Chat {
            conversation_id,
            at_unix_ms,
            ..
        } => Some(format!("cli-chat:{conversation_id}:{at_unix_ms}")),
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
