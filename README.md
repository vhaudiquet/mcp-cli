# mcp-cli

A standalone MCP client. Configured servers become shell commands:

```sh
mcp --config ./mcp.json list
mcp --config ./mcp.json blender --list
mcp --config ./mcp.json blender get_scene_info --query chair
mcp --config ./mcp.json blender execute_blender_code --code 'print("hi")'
```

A stdio entry is a child this process starts. An HTTP entry is a
Streamable HTTP endpoint. The client speaks the MCP subset
`initialize`, `tools/list`, and `tools/call`, with the same framing
caps and cancellation the rhema client uses.

There is no host bridge. A server that runs in the sandbox is called
from the sandbox. If a server needs live state across calls, run it as
a sandbox-local daemon later; this binary starts one per invocation.

## Config

```json
{
  "servers": {
    "blender": {
      "description": "The user's live Blender",
      "command": ["uvx", "blender-mcp"],
      "env": {"BLENDER_HOST": "localhost"}
    },
    "remote": {
      "url": "https://example.test/mcp",
      "headers": {"Authorization": "Bearer ${REMOTE_TOKEN}"}
    }
  }
}
```

`command` is an argv array, or a program name with `args`. `url` is a
Streamable HTTP endpoint. Set one, not both. Values may name
environment variables with `${NAME}` or `$NAME`; a missing variable is
an error at launch, so the config file never holds the secret. The
top-level object may also be the flat server map, or Claude's
`mcpServers` object.

The path comes from `--config`, then `$MCP_CONFIG`, then `./mcp.json`,
then `$XDG_CONFIG_HOME/mcp/config.json`.

## Surface

```
mcp [--config PATH] list
mcp [--config PATH] <server> [--list] [--schema TOOL]
mcp [--config PATH] <server> <tool> [flags...] [--json JSON|-]
```

A bare server invocation and `--list` print the tool catalog as JSON.
`--help` prints the generated commands. `--json` replaces the whole
argument object; `-` reads it from stdin. The mapping from schemas to
flags lives in [`mcp-to-cli`](../mcp-to-cli), which stays a pure
library with no I/O.

Exit codes: `0` ok; `2` usage; `3` unknown server; `4` transport; `5`
the server reported failure (`isError` or a JSON-RPC error); `6`
timeout.

## Layout

- `src/protocol.rs`: JSON-RPC and the MCP subset.
- `src/client.rs`, `src/client/`: stdio and Streamable HTTP.
- `src/config.rs`: the config file and `${VAR}` expansion.
- `src/main.rs`: the `mcp` command.

The client is also a library (`mcp_cli::client`), so a frontend that
wants to own the process can depend on it instead of reimplementing
the transport.
