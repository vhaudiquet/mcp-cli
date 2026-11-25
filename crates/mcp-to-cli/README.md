# mcp-to-cli

Part of the [`mcp-cli`](../..) workspace.

MCP servers advertise their tools as `(name, description, JSON Schema)`
triples. `mcp-to-cli` turns each CLI-safe tool into a clap subcommand
whose flags come from the schema's top-level properties, and turns
parsed argv back into the `{property: value}` object the server
expects.

It is pure mapping: no I/O, no processes, no MCP protocol. `clap`,
`serde_json`, and `thiserror` are the whole dependency list.

## Use

```rust
use mcp_to_cli::{Payload, ToolInfo, args, command};

fn main() {
    let catalog = [ToolInfo {
        name: "create_pull_request".into(),
        description: "Create a new pull request".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "draft": {"type": "boolean", "default": false}
            },
            "required": ["title"]
        }),
    }];

    let cmd = command("github", &catalog);
    let matches = cmd
        .try_get_matches_from(["github", "create_pull_request", "--title", "Ship it"])
        .expect("argv parses");

    match args(&cmd, &matches).expect("argv maps") {
        Payload::Mapped(value) => println!("{value}"),  // {"title":"Ship it"}
        Payload::Overridden(raw) => println!("{raw}"),  // the --json string, verbatim
        Payload::Schema { tool } => println!("print the schema of {tool}"),
        _ => unreachable!(),
    }
}
```

The mapping is conservative, with an escape hatch at every level of
schema complexity:

- enums become clap `possible_values`;
- scalar arrays become repeatable flags;
- every shape the flag form cannot express collapses to a
  `--<property>_json` flag;
- `--json` carries the whole argument object verbatim, for scripts;
- schema defaults are never pre-filled: an omitted flag omits the key,
  and the server stays the validator.

Discovery rides along: a bare invocation answers with the tool list
(`list_json`), and `--schema` asks for one tool's `input_schema`
instead of calling it (`Payload::Schema`).

## Examples

```
cargo run --example github      # the real GitHub create_pull_request schema
cargo run --example blender     # a Blender MCP server's tools
cargo run --example catalog     # every tool of a JSON catalog
```

`cargo test` drives the mapping matrix against vendored real schemas.
