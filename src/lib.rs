//! The client half of `mcp`: the MCP protocol subset and the stdio and
//! Streamable HTTP connections. The `mcp` binary adds config and the
//! command surface; another frontend can use this library directly.
//!
//! The protocol pin, framing caps, and cancellation follow the same
//! rules the rhema MCP client used. The bridge and manager are not
//! here: a client that spawns or reaches its server needs neither.

pub mod client;
pub mod protocol;
