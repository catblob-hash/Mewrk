//! The in-process MCP server that publishes the host's tools (`MewrkMcpServer`).
//!
//! The SDK carries an in-process server's JSON-RPC inside `mcp_message` control requests on the CLI's stdio. Hand-written
//! rather than the SDK's `tool()` helper there too: the host schema must reach the model verbatim.

use tokio::sync::oneshot;

use crate::jobj;
use crate::json::Json;

use super::shapes::SERVER_NAME;

/// The MCP protocol revision answered when the client's own is not a string.
const MCP_PROTOCOL_FALLBACK: &str = "2025-06-18";
/// The revision the SDK's manifest capture offers its own in-process servers.
const MANIFEST_PROTOCOL: &str = "2025-11-25";
const MCP_METHOD_NOT_FOUND: i64 = -32601;

/// What a message from the CLI asks of the server.
pub(super) enum McpHandled {
    /// A JSON-RPC reply to send now.
    Reply(Json),
    /// `tools/call`: the session decides when it is answered.
    Call { rpc_id: Json, tool_use_id: Option<String>, arguments: Option<Json> },
    /// A notification, or a reply: nothing to send.
    Silent,
}

pub(super) struct MewrkMcpServer {
    tools: Vec<Json>,
    closed: bool,
    /// Waiters for the CLI's next `tools/list`, which follows a `list_changed`.
    relisted: Vec<oneshot::Sender<()>>,
}

fn respond(id: &Json, result: Json) -> Json {
    jobj! { "jsonrpc": "2.0", "id": id.clone(), "result": result }
}

fn respond_error(id: &Json, code: i64, message: String) -> Json {
    jobj! { "jsonrpc": "2.0", "id": id.clone(), "error": jobj! { "code": code, "message": message } }
}

/// The JSON-RPC response carrying a finished tool call.
pub(super) fn call_response(rpc_id: &Json, result: Json) -> Json {
    respond(rpc_id, result)
}

impl MewrkMcpServer {
    pub fn new(tools: &[Json]) -> Self {
        Self { tools: tools.to_vec(), closed: false, relisted: Vec::new() }
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.relisted.clear();
    }

    /// `publish`: adds the tools this server has not listed yet. `Ok(Some)` means there are new ones and the receiver
    /// fires once the CLI has listed again, after it is told with [`LIST_CHANGED`]. Never withdraws a tool: the CLI would
    /// tell the model about a withdrawal in words of its own.
    pub fn publish(&mut self, tools: &[Json]) -> Result<Option<oneshot::Receiver<()>>, String> {
        let added: Vec<Json> = tools
            .iter()
            .filter(|tool| !self.tools.iter().any(|known| known.str_at("name") == tool.str_at("name")))
            .cloned()
            .collect();
        if added.is_empty() {
            return Ok(None);
        }
        self.tools.extend(added);
        if self.closed {
            return Err("MCP 传输已关闭".to_owned());
        }
        let (wake, relisted) = oneshot::channel();
        self.relisted.push(wake);
        Ok(Some(relisted))
    }

    fn initialize_result(requested: Option<&str>) -> Json {
        jobj! {
            "protocolVersion": requested.unwrap_or(MCP_PROTOCOL_FALLBACK),
            // `listChanged`: a tool that joins mid-session is announced rather than rebuilt in.
            "capabilities": jobj! { "tools": jobj! { "listChanged": true } },
            "serverInfo": jobj! { "name": SERVER_NAME, "version": "1.0.0" },
        }
    }

    fn tools_list(&self) -> Json {
        let tools: Vec<Json> = self
            .tools
            .iter()
            .map(|tool| {
                jobj! {
                    "name"?: tool.get("name").cloned(),
                    "description"?: tool.get("description").cloned(),
                    // Verbatim host schema; the CLI validates it against what the API accepts.
                    "inputSchema"?: tool.get("inputSchema").cloned(),
                    // Keep every host tool in the model's context regardless of the CLI's deferred-tool heuristics.
                    "_meta": jobj! { "anthropic/alwaysLoad": true },
                }
            })
            .collect();
        jobj! { "tools": tools }
    }

    /// The manifest the SDK captures before `initialize` (`captureSdkMcpServerManifests`): the server's `initialize`
    /// answer to the SDK's own offer, and its tool list, so the CLI need not handshake over the control channel.
    pub fn manifest(&self) -> Json {
        jobj! {
            "initializeResult": Self::initialize_result(Some(MANIFEST_PROTOCOL)),
            "toolsListResult": self.tools_list(),
        }
    }

    /// `handle`.
    pub fn handle(&mut self, message: &Json) -> McpHandled {
        let Some(method) = message.str_at("method") else { return McpHandled::Silent };
        let id = message.get("id").filter(|id| !id.is_null());
        let empty = Json::obj();
        let params = message.get("params").filter(|params| params.is_obj()).unwrap_or(&empty);
        match method {
            "initialize" => match id {
                Some(id) => McpHandled::Reply(respond(id, Self::initialize_result(params.str_at("protocolVersion")))),
                None => McpHandled::Silent,
            },
            "ping" => match id {
                Some(id) => McpHandled::Reply(respond(id, Json::obj())),
                None => McpHandled::Silent,
            },
            "tools/list" => {
                let list = self.tools_list();
                for wake in self.relisted.drain(..) {
                    let _ = wake.send(());
                }
                match id {
                    Some(id) => McpHandled::Reply(respond(id, list)),
                    None => McpHandled::Silent,
                }
            }
            "tools/call" => {
                let tool_use_id = params.path(&["_meta", "claudecode/toolUseId"]).and_then(Json::as_str).map(str::to_owned);
                McpHandled::Call {
                    rpc_id: id.cloned().unwrap_or(Json::Null),
                    tool_use_id,
                    arguments: params.get("arguments").cloned(),
                }
            }
            // Notifications (`notifications/initialized`, `notifications/cancelled`) need no reply; unknown requests are
            // refused rather than guessed.
            _ => match id {
                Some(id) => McpHandled::Reply(respond_error(id, MCP_METHOD_NOT_FOUND, format!("Method not found: {method}"))),
                None => McpHandled::Silent,
            },
        }
    }
}

/// What tells the CLI to list the tools again.
pub(super) fn list_changed() -> Json {
    jobj! { "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<Json> {
        vec![
            Json::parse(r#"{"name":"read","description":"Read","inputSchema":{"type":"object","z":1,"a":2}}"#).unwrap(),
            Json::parse(r#"{"name":"bash","inputSchema":{"type":"object"}}"#).unwrap(),
        ]
    }

    fn reply(handled: McpHandled) -> String {
        match handled {
            McpHandled::Reply(reply) => reply.to_text(),
            _ => panic!("no reply"),
        }
    }

    #[test]
    fn the_handshake_answers_what_the_client_asked_for() {
        let mut server = MewrkMcpServer::new(&tools());
        let message = Json::parse(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":"2030-01-01"}}"#).unwrap();
        assert_eq!(
            reply(server.handle(&message)),
            r#"{"jsonrpc":"2.0","id":7,"result":{"protocolVersion":"2030-01-01","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"mewrk","version":"1.0.0"}}}"#
        );
        let message = Json::parse(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#).unwrap();
        assert_eq!(reply(server.handle(&message)), r#"{"jsonrpc":"2.0","id":"p","result":{}}"#);
    }

    #[test]
    fn a_listed_schema_is_verbatim_and_always_loaded() {
        let mut server = MewrkMcpServer::new(&tools());
        let message = Json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert_eq!(
            reply(server.handle(&message)),
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"read","description":"Read","inputSchema":{"type":"object","z":1,"a":2},"_meta":{"anthropic/alwaysLoad":true}},{"name":"bash","inputSchema":{"type":"object"},"_meta":{"anthropic/alwaysLoad":true}}]}}"#
        );
    }

    #[test]
    fn unknown_requests_are_refused_and_notifications_ignored() {
        let mut server = MewrkMcpServer::new(&[]);
        let message = Json::parse(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#).unwrap();
        assert_eq!(reply(server.handle(&message)), r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found: resources/list"}}"#);
        let message = Json::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(matches!(server.handle(&message), McpHandled::Silent));
    }

    #[test]
    fn a_call_carries_its_tool_use_id() {
        let mut server = MewrkMcpServer::new(&tools());
        let message = Json::parse(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{"p":1},"_meta":{"claudecode/toolUseId":"toolu_1"}}}"#,
        )
        .unwrap();
        match server.handle(&message) {
            McpHandled::Call { rpc_id, tool_use_id, arguments } => {
                assert_eq!(rpc_id, Json::from(3u64));
                assert_eq!(tool_use_id.as_deref(), Some("toolu_1"));
                assert_eq!(arguments.unwrap().to_text(), r#"{"p":1}"#);
            }
            _ => panic!("no call"),
        }
    }

    #[test]
    fn publishing_waits_for_the_next_list() {
        let mut server = MewrkMcpServer::new(&tools());
        assert!(server.publish(&tools()).unwrap().is_none());
        let late = Json::parse(r#"{"name":"late","inputSchema":{}}"#).unwrap();
        let mut relisted = server.publish(&[late]).unwrap().unwrap();
        assert!(relisted.try_recv().is_err());
        let message = Json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert!(reply(server.handle(&message)).contains(r#""name":"late""#));
        assert!(relisted.try_recv().is_ok());
        server.close();
        assert_eq!(server.publish(&[Json::parse(r#"{"name":"later"}"#).unwrap()]).unwrap_err(), "MCP 传输已关闭");
    }
}
