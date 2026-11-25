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

use mcp_cli::client::{Cancel, Client, ClientError, HttpParams, StdioParams};
use config::{Config, Launch};
use mcp_to_cli::{Payload, ToolInfo, args as map_args, command as build_command, list_json, schema_of};
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
    let mut connection = match connect(launch).await {
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

    let wants_list = rest.first().is_some_and(|arg| arg == "--list" || arg == "list");
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
    match connection.call_tool(&tool, arguments, CALL_TIMEOUT, &cancel).await {
        Ok(result) => {
            let failed = result.is_error == Some(true);
            match serde_json::to_string_pretty(&result) {
                Ok(text) => println!("{text}"),
                Err(_) => println!("{}", serde_json::to_string(&result).unwrap_or_default()),
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

fn parse_json(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(text).map_err(|err| format!("args JSON: {err}"))
}
