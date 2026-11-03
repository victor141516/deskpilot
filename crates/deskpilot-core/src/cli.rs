//! Command line: `deskpilot mcp` (MCP server on stdio) or
//! `deskpilot call <tool> [json]` (one-shot, JSON on stdout).

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::dispatch::Dispatcher;
use crate::error::{Error, Result};
use crate::platform::Platform;
use crate::policy::{Policy, Strategy};
use crate::tools::ToolOutput;

#[derive(Debug, Parser)]
#[command(
    name = "deskpilot",
    version,
    about = "Desktop automation for agents: accessibility first, real input last."
)]
struct Cli {
    /// Config file (default: <config dir>/deskpilot/config.json).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Never act on windows of this process (repeatable; adds to the config deny list).
    #[arg(long = "deny-process", global = true, value_name = "NAME")]
    deny_process: Vec<String>,
    /// Describe mutating actions instead of performing them.
    #[arg(long, global = true)]
    dry_run: bool,
    /// Most invasive strategy allowed (accessibility, messages, keyboard, mouse).
    #[arg(long, global = true, value_enum)]
    max_strategy: Option<Strategy>,
    /// Do not write the audit log.
    #[arg(long, global = true)]
    no_audit: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the Model Context Protocol on stdin/stdout.
    Mcp,
    /// Run one tool and print the result as JSON.
    Call {
        /// Tool name (see `deskpilot tools`).
        tool: String,
        /// Arguments as JSON, `@file.json`, or `-` for stdin.
        args: Option<String>,
        /// Extra argument `path=value` (dotted paths nest; values parse as JSON when possible). Repeatable.
        #[arg(short = 'a', long = "arg", value_name = "PATH=VALUE")]
        pairs: Vec<String>,
        /// Run the tool as administrator (UAC prompt).
        #[arg(long)]
        elevated: bool,
        /// Directory for returned images (default: <temp>/deskpilot).
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Print the agent text instead of JSON.
        #[arg(long)]
        text: bool,
    },
    /// List tools (names and descriptions, or full JSON schemas).
    Tools {
        #[arg(long)]
        schema: bool,
    },
    /// Internal: elevated worker connected to a parent instance.
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        endpoint: String,
    },
}

/// Entry point used by the platform binaries.
pub fn run(platform: Box<dyn Platform>) -> ExitCode {
    let cli = Cli::parse();
    let policy = match build_policy(&cli) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("deskpilot: {e}");
            return ExitCode::from(2);
        }
    };
    let worker_args = worker_args(&cli);
    match &cli.command {
        Command::Tools { .. } => {}
        // The MCP client is waiting for `initialize`: never block it.
        Command::Mcp | Command::Worker { .. } => platform.request_permissions(false),
        Command::Call { .. } => platform.request_permissions(true),
    }
    match cli.command {
        Command::Mcp => {
            let dispatcher = Dispatcher::new(platform, policy, worker_args);
            match crate::mcp::serve(&dispatcher) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("deskpilot mcp: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Tools { schema } => {
            let dispatcher = Dispatcher::new(platform, policy, worker_args);
            let list: Vec<Value> = dispatcher
                .tools()
                .iter()
                .map(|t| {
                    if schema {
                        json!({ "name": t.name, "description": t.description, "mutating": t.mutating, "inputSchema": t.input_schema })
                    } else {
                        json!({ "name": t.name, "description": t.description, "mutating": t.mutating })
                    }
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&list).unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Command::Worker { endpoint } => {
            let channel = match platform.connect_worker(&endpoint) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("deskpilot worker: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let dispatcher = Dispatcher::new(platform, policy, worker_args);
            match crate::worker::serve(&dispatcher, channel) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("deskpilot worker: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Call {
            tool,
            args,
            pairs,
            elevated,
            out_dir,
            text,
        } => {
            let dispatcher = Dispatcher::new(platform, policy, worker_args);
            let result = parse_args(args.as_deref(), &pairs).and_then(|mut v| {
                if elevated {
                    v["elevated"] = Value::Bool(true);
                }
                dispatcher.call(&tool, v)
            });
            print_result(&tool, result, out_dir, text)
        }
    }
}

fn build_policy(cli: &Cli) -> Result<Policy> {
    let mut policy = Policy::load(cli.config.as_ref())?;
    policy
        .deny_processes
        .extend(cli.deny_process.iter().cloned());
    policy.dry_run = cli.dry_run;
    if let Some(s) = cli.max_strategy {
        policy.max_strategy = s;
    }
    if cli.no_audit {
        policy.audit_path = None;
    }
    Ok(policy)
}

/// Flags that make an elevated worker apply the same policy as this process.
fn worker_args(cli: &Cli) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(c) = &cli.config {
        args.extend(["--config".to_string(), c.display().to_string()]);
    }
    for d in &cli.deny_process {
        args.extend(["--deny-process".to_string(), d.clone()]);
    }
    if cli.dry_run {
        args.push("--dry-run".into());
    }
    if let Some(s) = cli.max_strategy {
        args.extend([
            "--max-strategy".to_string(),
            format!("{s:?}").to_lowercase(),
        ]);
    }
    if cli.no_audit {
        args.push("--no-audit".into());
    }
    args
}

fn parse_args(raw: Option<&str>, pairs: &[String]) -> Result<Value> {
    let mut value = match raw {
        None => json!({}),
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(Error::platform)?;
            parse_json(&s)?
        }
        Some(r) if r.starts_with('@') => {
            let s = std::fs::read_to_string(&r[1..])
                .map_err(|e| Error::InvalidArgument(format!("{}: {e}", &r[1..])))?;
            parse_json(&s)?
        }
        Some(r) => parse_json(r)?,
    };
    if !value.is_object() {
        return Err(Error::InvalidArgument(
            "arguments must be a JSON object".into(),
        ));
    }
    for pair in pairs {
        let (path, raw_value) = pair.split_once('=').ok_or_else(|| {
            Error::InvalidArgument(format!("--arg expects PATH=VALUE, got '{pair}'"))
        })?;
        let parsed = serde_json::from_str::<Value>(raw_value)
            .unwrap_or_else(|_| Value::String(raw_value.to_string()));
        set_path(&mut value, path, parsed)?;
    }
    Ok(value)
}

fn parse_json(s: &str) -> Result<Value> {
    serde_json::from_str(s.trim())
        .map_err(|e| Error::InvalidArgument(format!("arguments are not valid JSON: {e}")))
}

fn set_path(root: &mut Value, path: &str, value: Value) -> Result<()> {
    let mut cur = root;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "bad argument path '{path}'"
            )));
        }
        let obj = cur.as_object_mut().ok_or_else(|| {
            Error::InvalidArgument(format!("'{path}' crosses a non-object value"))
        })?;
        if i == parts.len() - 1 {
            obj.insert((*part).to_string(), value);
            return Ok(());
        }
        cur = obj.entry((*part).to_string()).or_insert_with(|| json!({}));
    }
    Ok(())
}

fn print_result(
    tool: &str,
    result: Result<ToolOutput>,
    out_dir: Option<PathBuf>,
    text_only: bool,
) -> ExitCode {
    match result {
        Ok(out) => {
            if text_only {
                println!("{}", out.agent_text());
            }
            let mut images = Vec::new();
            for (n, img) in out.images.iter().enumerate() {
                match save_image(tool, n, &img.mime, &img.data, out_dir.as_ref()) {
                    Ok(path) => {
                        if text_only {
                            println!("image: {}", path.display());
                        }
                        images.push(json!({ "mime": img.mime, "path": path }));
                    }
                    Err(e) => images.push(json!({ "mime": img.mime, "error": e.to_string() })),
                }
            }
            if !text_only {
                let mut doc = json!({ "ok": true, "data": out.data });
                if let Some(t) = &out.text {
                    doc["text"] = json!(t);
                }
                if !images.is_empty() {
                    doc["images"] = json!(images);
                }
                println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if text_only {
                eprintln!("{e}");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "ok": false, "error": e.to_json() }))
                        .unwrap_or_default()
                );
            }
            ExitCode::FAILURE
        }
    }
}

fn save_image(
    tool: &str,
    n: usize,
    mime: &str,
    b64: &str,
    out_dir: Option<&PathBuf>,
) -> Result<PathBuf> {
    let dir = out_dir
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("deskpilot"));
    std::fs::create_dir_all(&dir).map_err(Error::platform)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let ext = if mime == "image/jpeg" { "jpg" } else { "png" };
    let path = dir.join(format!("{tool}-{ts}-{n}.{ext}"));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(Error::platform)?;
    std::fs::write(&path, bytes).map_err(Error::platform)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_build_nested_arguments_and_parse_json_values() {
        let v = parse_args(
            Some(r#"{"action":"select"}"#),
            &[
                "window.title=Sound".into(),
                "window.pid=42".into(),
                "name=Playback".into(),
                "elevated=true".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            v,
            json!({ "action": "select", "window": { "title": "Sound", "pid": 42 }, "name": "Playback", "elevated": true })
        );
    }

    #[test]
    fn pairs_reject_paths_through_scalars() {
        assert!(parse_args(Some(r#"{"window":"x"}"#), &["window.title=a".into()]).is_err());
    }
}
