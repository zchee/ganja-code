//! The MCP server ganja is, from the CLI's side of the wire.
//!
//! Spec: the recording's `mcp_message` exchanges — run 1's `initialize`,
//! `notifications/initialized`, `tools/list` and `tools/call`, each with the
//! answer the driver sent and the CLI accepted.
//!
//! # Four shapes, hand-rolled
//!
//! **No `rmcp`** (ADR 6). What this side of the protocol needs is four result
//! objects and a JSON-RPC envelope; a full MCP implementation would bring a
//! transport, a client, a session model and a schema registry to produce
//! them, and none of that is reachable from here — the transport is the CLI's
//! own `control_request` frame. So the four are declared as structs with
//! `camelCase` serialization, each one the dict the recording proves
//! accepted, and the envelope is three fields.
//!
//! # Names are bare
//!
//! `tools/list` declares `ganja_ping`, not `mcp__ganja__ganja_ping` (M6): the
//! model-facing name is the CLI's own prefixing of what this side declared,
//! and `tools/call` comes back **bare** again. So the roster goes out under
//! the registry's own names and a call is matched back by
//! `_meta["claudecode/toolUseId"]` rather than by the name it arrives under.

use serde::Serialize;

use crate::tool::ToolDefinition;

/// The MCP protocol version this side answers under.
///
/// Echoed from the CLI's own `initialize` rather than asserted: the recording
/// shows the driver echoing `2025-11-25` back and the dial completing, and a
/// server that named a version of its own would be negotiating where the
/// reference simply agrees. [`DEFAULT_PROTOCOL`] is what a request carrying
/// none is answered with.
pub const DEFAULT_PROTOCOL: &str = "2025-11-25";

/// The server name this side declares, and the one `mcp_message`'s
/// `server_name` must equal.
pub const SERVER: &str = "ganja";

/// One tool as `tools/list` declares it.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// The registry name, bare.
    pub name: String,
    /// What the model reads to decide whether to call it.
    pub description: String,
    /// The arguments' JSON Schema.
    pub input_schema: serde_json::Value,
}

impl From<&ToolDefinition> for Tool {
    fn from(definition: &ToolDefinition) -> Self {
        Self {
            name: definition.name.clone(),
            description: definition.description.clone(),
            input_schema: definition.schema.clone(),
        }
    }
}

/// What `initialize` is answered with.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// The version the CLI named, echoed.
    pub protocol_version: String,
    /// What this server does. Declaring `tools` is what makes the CLI ask for
    /// a roster at all (W1a Q3.3), so an empty object here would be a server
    /// nothing is ever called on.
    pub capabilities: Capabilities,
    /// Who this server is.
    pub server_info: ServerInfo,
}

/// The one capability this server declares.
#[derive(Debug, Serialize, PartialEq)]
pub struct Capabilities {
    /// Present, which is what makes the CLI ask for a roster at all.
    pub tools: ToolsCapability,
}

/// What this server says about its roster (`i5oi`).
#[derive(Debug, Serialize, PartialEq)]
pub struct ToolsCapability {
    /// That the roster may change while the process lives, and that this side
    /// will say so with [`LIST_CHANGED`]. The CLI subscribes to that
    /// notification only for a server declaring this (W1a Q3.9-3.10, read off
    /// the bundle's code path), and the notification is the one way a live
    /// process hears a roster that grew. **Unmeasured live**: no recorded run
    /// declared it, so what the CLI does on receipt — re-list and advertise
    /// the new tools from its next request — is the bundle's reading and no
    /// frame's.
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

/// The JSON-RPC notification this side sends when the roster moves (`i5oi`).
pub const LIST_CHANGED: &str = "notifications/tools/list_changed";

/// The [`LIST_CHANGED`] notification's JSON-RPC envelope: no `id`, because a
/// notification is answered with nothing.
#[must_use]
pub fn list_changed() -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "method": LIST_CHANGED})
}

/// The name and version this server answers under.
#[derive(Debug, Serialize, PartialEq)]
pub struct ServerInfo {
    /// [`SERVER`].
    pub name: String,
    /// This build's version.
    pub version: String,
}

/// What `tools/list` is answered with.
#[derive(Debug, Serialize, PartialEq)]
pub struct ListToolsResult {
    /// The request's roster, in the order the engine advertised it.
    pub tools: Vec<Tool>,
}

/// What `tools/call` is answered with.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    /// The blocks the model reads. **One** on the allow path — the tool
    /// part's output, byte-identical (rev 6, change 4): a second block
    /// carrying a user's words is delivered and then named as injection in
    /// the reply the person reads (M19 (b)), so an owed message rides the
    /// next turn instead.
    pub content: Vec<Content>,
    /// Whether the call failed.
    pub is_error: bool,
}

/// One content block of a tool's answer.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Content {
    /// Always `text` on this wire.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// What the tool produced.
    pub text: String,
}

impl Content {
    /// One text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self { kind: "text", text: text.into() }
    }
}

/// What answering one `mcp_message` produced.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// A JSON-RPC response to write back.
    Reply(serde_json::Value),
    /// A notification, which JSON-RPC answers with nothing at all — but the
    /// CLI's own envelope still wants an empty success, which is what the
    /// recording's `notifications/initialized` exchange shows.
    Empty,
    /// A `tools/call` the router must resolve against a tool part rather than
    /// answer here.
    Call(ToolCall),
}

/// A `tools/call` the CLI sent, reduced to what the router matches on.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The JSON-RPC id its answer must echo.
    pub id: serde_json::Value,
    /// The tool's registry name, bare.
    pub name: String,
    /// The id the ask carried, off `_meta["claudecode/toolUseId"]` — the
    /// correlator, and the only thing that reliably says *which* call this
    /// is. [`None`] for a call carrying no `_meta`, which is matched FIFO by
    /// name instead.
    pub tool_use_id: Option<String>,
}

/// Answers one JSON-RPC message addressed to this server.
///
/// `tools/call` is not answered here: it is returned as [`Answer::Call`] for
/// the bridge to resolve against the engine's own tool part, because nothing
/// in this crate runs a tool.
#[must_use]
pub fn answer(message: &serde_json::Value, tools: &[ToolDefinition], version: &str) -> Answer {
    let id = message["id"].clone();
    let method = message["method"].as_str().unwrap_or_default();

    match method {
        "initialize" => {
            let protocol = message["params"]["protocolVersion"]
                .as_str()
                .unwrap_or(DEFAULT_PROTOCOL)
                .to_owned();

            Answer::Reply(reply(
                &id,
                &InitializeResult {
                    protocol_version: protocol,
                    capabilities: Capabilities { tools: ToolsCapability { list_changed: true } },
                    server_info: ServerInfo {
                        name: SERVER.to_owned(),
                        version: version.to_owned(),
                    },
                },
            ))
        }
        // A JSON-RPC notification carries no id and earns no response; the
        // CLI's own envelope still expects the empty success.
        method if method.starts_with("notifications/") => Answer::Empty,
        "tools/list" => Answer::Reply(reply(
            &id,
            &ListToolsResult { tools: tools.iter().map(Tool::from).collect() },
        )),
        "ping" => Answer::Reply(reply(&id, &serde_json::Map::new())),
        "tools/call" => Answer::Call(ToolCall {
            id,
            name: message["params"]["name"].as_str().unwrap_or_default().to_owned(),
            tool_use_id: message["params"]["_meta"]["claudecode/toolUseId"]
                .as_str()
                .map(str::to_owned),
        }),
        other => Answer::Reply(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")},
        })),
    }
}

/// A JSON-RPC success envelope around one result.
#[must_use]
pub fn reply(id: &serde_json::Value, result: &impl Serialize) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": serde_json::to_value(result).expect("every result shape here serializes"),
    })
}

/// What the CLI's frame wraps a JSON-RPC reply in.
#[must_use]
pub fn wrapped(reply: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"mcp_response": reply})
}

#[cfg(test)]
#[path = "rpc_tests.rs"]
mod tests;
