//! `mcp`, a standalone MCP client exposed as a shell command.
//!
//! One config file names the servers. A stdio entry is a child this
//! process spawns; an HTTP entry is a Streamable HTTP endpoint. The
//! tool catalog becomes clap subcommands through `mcp-to-cli`, so a
//! call is `mcp <server> <tool> --flag value`, and `--json` carries
//! the whole argument object.
//!
//! Exit codes: 0 ok; 2 usage; 3 unknown server; 4 transport; 5 the
//! server reported failure; 6 timeout.

mod config;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use config::{Config, Launch};
use mcp_cli::client::{Cancel, Client, ClientError, HttpParams, StdioParams};
use mcp_to_cli::{
    Payload, ToolInfo, args as map_args, command as build_command, list_json, schema_of,
};
use serde_json::{Value, json};

/// The startup budget for a server.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// The budget for one call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

async fn run() -> i32 {
    let mut argv = std::env::args().skip(1).peekable();
    let mut explicit: Option<PathBuf> = None;
    let mut rest = Vec::new();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" => match argv.next() {
                Some(path) => explicit = Some(path.into()),
                None => {
                    eprintln!("mcp: `--config` needs a path");
                    return 2;
                }
            },
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            "-V" | "--version" => {
                println!("mcp-cli {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            _ => {
                rest.push(arg);
                rest.extend(argv);
                break;
            }
        }
    }
    let Some(verb) = rest.first().cloned() else {
        print_usage();
        return 0;
    };
    let path = match Config::discover(explicit.as_deref()) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("mcp: {err}");
            return 4;
        }
    };
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("mcp: {err}");
            return 4;
        }
    };
    for notice in &config.notices {
        eprintln!("mcp: {notice}");
    }
    if verb == "list" || verb == "servers" {
        return list(&config, &rest[1..]);
    }
    server_mode(&config, &verb, &rest[1..]).await
}

fn print_usage() {
    println!(
        "usage: mcp [--config PATH] list\n\
         \x20      mcp [--config PATH] <server> [--list] [--schema TOOL]\n\
         \x20      mcp [--config PATH] <server> <tool> [flags...] [--json JSON|-]\n\
         \n\
         \x20--config PATH   the server config (default: $MCP_CONFIG, ./mcp.json,\n\
         \x20                $XDG_CONFIG_HOME/mcp/config.json)\n\
         \x20<server> --list  the server's tools as JSON\n\
         \x20<server> --help  the generated commands\n\
         \x20--json JSON      the argument object verbatim; `-` reads stdin"
    );
}

fn list(config: &Config, rest: &[String]) -> i32 {
    let _ = rest;
    let servers: Vec<Value> = config
        .servers
        .iter()
        .map(|(name, server)| {
            let transport = match (&server.launch(), server.description.as_deref()) {
                (Ok(Launch::Stdio { .. }), _) => "stdio",
                (Ok(Launch::Http { .. }), _) => "http",
                (Err(_), _) => "invalid",
            };
            json!({
                "name": name,
                "transport": transport,
                "description": server.description,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&servers).unwrap_or_else(|_| "[]".to_owned())
    );
    0
}

async fn server_mode(config: &Config, server: &str, rest: &[String]) -> i32 {
    let Some(entry) = config.servers.get(server) else {
        eprintln!("mcp: unknown server `{server}`");
        return 3;
    };
    let launch = match entry.launch() {
        Ok(launch) => launch,
        Err(err) => {
            eprintln!("mcp: {server}: {err}");
            return 2;
        }
    };
    let connection = match connect(launch).await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("mcp: {server}: {err}");
            return transport_exit(&err);
        }
    };
    let cancel = Cancel::new();
    let catalog = match connection.tools_list(CALL_TIMEOUT, &cancel).await {
        Ok(tools) => tools,
        Err(err) => {
            eprintln!("mcp: {server}: {err}");
            return transport_exit(&err);
        }
    };
    let infos: Vec<ToolInfo> = catalog
        .iter()
        .map(|tool| ToolInfo {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            input_schema: tool.input_schema.clone(),
        })
        .collect();

    let wants_list = rest
        .first()
        .is_some_and(|arg| arg == "--list" || arg == "list");
    let wants_schema = rest.first().is_some_and(|arg| arg == "--schema");
    if wants_list || rest.is_empty() {
        println!(
            "{}",
            serde_json::to_string_pretty(&list_json(&infos)).unwrap_or_else(|_| "[]".to_owned())
        );
        return 0;
    }
    if wants_schema {
        let Some(tool) = rest.get(1) else {
            eprintln!("mcp: `--schema` needs a tool name");
            return 2;
        };
        let Some(schema) = schema_of(&infos, tool) else {
            eprintln!("mcp: unknown tool `{tool}`");
            return 2;
        };
        println!(
            "{}",
            serde_json::to_string_pretty(schema).unwrap_or_else(|_| "{}".to_owned())
        );
        return 0;
    }

    let mut tree = build_command(server, &infos);
    let mut argv_for_clap = vec![server.to_owned()];
    argv_for_clap.extend(rest.iter().cloned());
    let matches = match tree.try_get_matches_from_mut(argv_for_clap) {
        Ok(matches) => matches,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() { 2 } else { 0 };
        }
    };
    let Some(tool) = matches.subcommand_name().map(str::to_owned) else {
        println!(
            "{}",
            serde_json::to_string_pretty(&list_json(&infos)).unwrap_or_else(|_| "[]".to_owned())
        );
        return 0;
    };
    let arguments = match map_args(&tree, &matches) {
        Ok(Payload::Mapped(value)) => value,
        Ok(Payload::Overridden(text)) if text == "-" => {
            let mut input = String::new();
            if let Err(err) = std::io::stdin().read_to_string(&mut input) {
                eprintln!("mcp: reading stdin: {err}");
                return 2;
            }
            match parse_json(&input) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("mcp: {err}");
                    return 2;
                }
            }
        }
        Ok(Payload::Overridden(text)) => match parse_json(&text) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("mcp: {err}");
                return 2;
            }
        },
        Ok(Payload::Schema { tool }) => {
            let Some(schema) = schema_of(&infos, &tool) else {
                return 2;
            };
            println!(
                "{}",
                serde_json::to_string_pretty(schema).unwrap_or_else(|_| "{}".to_owned())
            );
            return 0;
        }
        Ok(_) => {
            eprintln!("mcp: unsupported arguments");
            return 2;
        }
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    match connection
        .call_tool(&tool, arguments, CALL_TIMEOUT, &cancel)
        .await
    {
        Ok(result) => {
            let failed = result.is_error == Some(true);
            let mut value = serde_json::to_value(&result).unwrap_or_else(|_| json!({}));
            stage_media(&mut value);
            match serde_json::to_string_pretty(&value) {
                Ok(text) => println!("{text}"),
                Err(_) => println!("{}", serde_json::to_string(&value).unwrap_or_default()),
            }
            if failed { 5 } else { 0 }
        }
        Err(err) => {
            eprintln!("mcp: {err}");
            transport_exit(&err)
        }
    }
}

async fn connect(launch: Launch) -> Result<Client, ClientError> {
    match launch {
        Launch::Stdio { command, env } => {
            let mut command = command.into_iter();
            let Some(program) = command.next() else {
                return Err(ClientError::Stdio(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "empty command",
                )));
            };
            Client::connect_stdio(
                StdioParams {
                    program,
                    args: command.collect(),
                    env: env.into_iter().collect::<BTreeMap<_, _>>(),
                },
                STARTUP_TIMEOUT,
            )
            .await
        }
        Launch::Http { url, headers } => {
            Client::connect_http_with_timeout(
                HttpParams {
                    url,
                    headers: headers.into_iter().collect(),
                },
                STARTUP_TIMEOUT,
            )
            .await
        }
    }
}

fn transport_exit(err: &ClientError) -> i32 {
    match err {
        ClientError::Rpc { .. } => 5,
        ClientError::Timeout => 6,
        _ => 4,
    }
}

/// Writes image and audio blocks under `$MCP_MEDIA_DIR` and replaces
/// them with the path, so the shell's image tools can attach them. With
/// no directory, a text placeholder carries the byte count instead of
/// a base64 blob.
fn stage_media(envelope: &mut Value) {
    let Some(content) = envelope.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let dir = std::env::var_os("MCP_MEDIA_DIR").map(PathBuf::from);
    for block in content.iter_mut() {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "image" && kind != "audio" {
            continue;
        }
        let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("");
        let data = block.get("data").and_then(Value::as_str).unwrap_or("");
        let Some(ext) = media_ext(mime) else {
            *block = json!({
                "type": "text",
                "text": format!("[{kind} ({mime}) omitted: unsupported media type]"),
            });
            continue;
        };
        let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
        else {
            *block = json!({
                "type": "text",
                "text": format!("[{kind} ({mime}) omitted: the payload does not decode]"),
            });
            continue;
        };
        let count = bytes.len();
        let written = dir.as_ref().and_then(|dir| {
            std::fs::create_dir_all(dir).ok()?;
            let digest = {
                use sha2::Digest as _;
                let digest = sha2::Sha256::digest(&bytes);
                digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            };
            let path = dir.join(format!("{digest}.{ext}"));
            write_private(&path, &bytes).ok()?;
            Some(path)
        });
        *block = match written {
            Some(path) => json!({
                "type": kind,
                "path": path.to_string_lossy(),
                "mimeType": mime,
                "bytes": count,
            }),
            None => json!({
                "type": "text",
                "text": format!("[{kind} ({mime}) omitted: {count} bytes; set MCP_MEDIA_DIR]"),
            }),
        };
    }
}

fn media_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/ogg" => Some("ogg"),
        _ => None,
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

fn parse_json(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(text).map_err(|err| format!("args JSON: {err}"))
}
