//! The standalone server config. One JSON file, servers keyed by name.
//!
//! Secrets are never stored here: values can name environment
//! variables (`${TOKEN}`), and the process that launches the server
//! expands them. A missing variable is a launch error naming the key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One server entry.
#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    /// Prompt disclosure text, optional.
    #[serde(default)]
    pub description: Option<String>,
    /// The stdio program and its arguments.
    #[serde(default)]
    command: Option<Command>,
    /// Extra stdio arguments when `command` is a bare program name.
    #[serde(default)]
    args: Vec<String>,
    /// Extra stdio environment, layered over the inherited one.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// The Streamable HTTP endpoint.
    #[serde(default)]
    url: Option<String>,
    /// Headers sent with every HTTP request.
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// `command` accepts an argv array or a bare program name.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Command {
    /// The whole argv.
    Line(Vec<String>),
    /// The program; `args` carries the rest.
    Program(String),
}

/// What to launch for one server.
#[derive(Debug, Clone)]
pub enum Launch {
    /// A child over stdio.
    Stdio {
        /// The program and its arguments.
        command: Vec<String>,
        /// Extra environment.
        env: BTreeMap<String, String>,
    },
    /// A Streamable HTTP endpoint.
    Http {
        /// The endpoint URL.
        url: String,
        /// Headers.
        headers: BTreeMap<String, String>,
    },
}

impl Server {
    /// The launched transport, with `${VAR}` references expanded.
    pub fn launch(&self) -> Result<Launch, String> {
        match (&self.command, &self.url) {
            (Some(command), None) => {
                let mut argv = match command {
                    Command::Line(argv) => argv.clone(),
                    Command::Program(program) => {
                        let mut argv = vec![program.clone()];
                        argv.extend(self.args.iter().cloned());
                        argv
                    }
                };
                if argv.is_empty() || argv[0].is_empty() {
                    return Err("command is empty".to_owned());
                }
                argv = argv
                    .iter()
                    .map(|arg| expand(arg))
                    .collect::<Result<_, _>>()?;
                let env = self
                    .env
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), expand(value)?)))
                    .collect::<Result<_, String>>()?;
                Ok(Launch::Stdio { command: argv, env })
            }
            (None, Some(url)) => {
                let headers = self
                    .headers
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), expand(value)?)))
                    .collect::<Result<_, String>>()?;
                Ok(Launch::Http {
                    url: expand(url)?,
                    headers,
                })
            }
            (Some(_), Some(_)) => Err("set either `command` or `url`, not both".to_owned()),
            (None, None) => Err("set `command` (stdio) or `url` (http)".to_owned()),
        }
    }
}

/// The whole file.
#[derive(Debug, Clone)]
pub struct Config {
    /// Servers in file order.
    pub servers: BTreeMap<String, Server>,
    /// Why entries or the file were skipped.
    pub notices: Vec<String>,
}

impl Config {
    /// Loads a config file.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    servers: BTreeMap::new(),
                    notices: vec![format!("no config at {}", path.display())],
                });
            }
            Err(err) => return Err(format!("reading {}: {err}", path.display())),
        };
        let root: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|err| format!("parsing {}: {err}", path.display()))?;
        let (entries, notices) = split(&root);
        let mut servers = BTreeMap::new();
        let mut notices = notices;
        for (name, value) in entries {
            match serde_json::from_value::<Server>(value) {
                Ok(server) => {
                    servers.insert(name, server);
                }
                Err(err) => notices.push(format!("mcp server `{name}` skipped: {err}")),
            }
        }
        Ok(Self { servers, notices })
    }

    /// The config path: `--config`, `$MCP_CONFIG`, `./mcp.json`, then
    /// the XDG config dir. A missing file is an empty config.
    pub fn discover(explicit: Option<&Path>) -> Result<PathBuf, String> {
        if let Some(path) = explicit {
            return Ok(path.to_path_buf());
        }
        if let Some(path) = std::env::var_os("MCP_CONFIG") {
            return Ok(path.into());
        }
        for name in ["mcp.json", ".mcp.json"] {
            let local = PathBuf::from(name);
            if local.is_file() {
                return Ok(local);
            }
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or_else(|| "no config path: set --config, MCP_CONFIG, or HOME".to_owned())?;
        Ok(base.join("mcp").join("config.json"))
    }
}

/// The server map: `{"servers": {...}}`, Claude's `{"mcpServers":
/// {...}}`, or the flat map itself.
fn split(root: &serde_json::Value) -> (Vec<(String, serde_json::Value)>, Vec<String>) {
    let map = match root {
        serde_json::Value::Object(map) => map,
        _ => {
            return (
                Vec::new(),
                vec!["the config root is not an object".to_owned()],
            );
        }
    };
    for key in ["servers", "mcpServers"] {
        if let Some(serde_json::Value::Object(servers)) = map.get(key) {
            return (
                servers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                Vec::new(),
            );
        }
    }
    (
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Vec::new(),
    )
}

/// Expands `${NAME}` and `$NAME`. An unset variable is an error; this
/// is how a secret reaches the server without living in the file.
fn expand(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            if (next.is_ascii_alphanumeric() || next == '_') && !(braced && next == '}') {
                name.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.next() != Some('}') {
            return Err(format!("unterminated ${{{name}}} in {text:?}"));
        }
        if name.is_empty() {
            if braced {
                return Err(format!("empty variable name in {text:?}"));
            }
            out.push('$');
            continue;
        }
        let value = std::env::var(&name).map_err(|_| format!("`${name}` is not set"))?;
        out.push_str(&value);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_map_is_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"blender":{"command":["uvx","blender-mcp"],"description":"Blender"}}"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.servers.contains_key("blender"));
        assert!(matches!(
            config.servers["blender"].launch().unwrap(),
            Launch::Stdio { .. }
        ));
    }

    #[test]
    fn a_missing_variable_names_itself() {
        let server = Server {
            description: None,
            command: Some(Command::Line(vec!["echo".into()])),
            args: Vec::new(),
            env: BTreeMap::from([("TOKEN".into(), "${MCP_CLI_TEST_UNSET}".into())]),
            url: None,
            headers: BTreeMap::new(),
        };
        let err = server.launch().unwrap_err();
        assert!(err.contains("MCP_CLI_TEST_UNSET"), "{err}");
    }
}
