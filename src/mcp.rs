//! MCP stdio transport: one JSON-RPC message per line, no stdout diagnostics.
use crate::{Command, Store};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};

const LATEST_PROTOCOL: &str = "2025-11-25";
const MAX_FRAME: u64 = 8 * 1024 * 1024;

pub fn command_schema() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(Command)).expect("static schema");
    schema["type"] = json!("object");
    schema
}
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn tool_result(result: Result<Value, crate::Error>) -> Value {
    match result {
        Ok(value) => {
            json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":{"result":value},"isError":false})
        }
        Err(error) => {
            json!({"content":[{"type":"text","text":error.json().to_string()}],"structuredContent":{"error":error.json()},"isError":true})
        }
    }
}
fn resource(uri: &str, value: Value) -> Value {
    json!({"contents":[{"uri":uri,"mimeType":"application/json","text":value.to_string()}]})
}

pub struct Server {
    store: Store,
    actor: String,
    initialized: bool,
    api_key: Option<String>,
}
impl Server {
    pub fn new(store: Store, actor: String) -> Self {
        Self {
            store,
            actor,
            initialized: false,
            api_key: None,
        }
    }
    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }
    /// Notifications never produce a response. Invalid requests preserve JSON-RPC errors.
    pub fn handle(&mut self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object()
            || request.get("jsonrpc") != Some(&json!("2.0"))
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|id| !id.is_string() && !id.is_number())
            || request
                .get("params")
                .is_some_and(|p| !p.is_object() && !p.is_array())
        {
            return Some(rpc_error(
                id.filter(|v| v.is_string() || v.is_number())
                    .unwrap_or(Value::Null),
                -32600,
                "invalid JSON-RPC request",
            ));
        }
        let method = method.expect("checked");
        let id = id?;
        if let Some(key) = &self.api_key {
            match self.store.authenticate(key) {
                Ok(Some(agent)) => self.actor = agent.id,
                Ok(None) => {
                    return Some(rpc_error(
                        id,
                        -32001,
                        "API key is invalid, revoked, or belongs to an inactive agent",
                    ));
                }
                Err(_) => return Some(rpc_error(id, -32603, "authentication unavailable")),
            }
        }
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let result = match method {
            "initialize" => {
                if self.initialized {
                    return Some(rpc_error(id, -32600, "already initialized"));
                }
                if !params.get("clientInfo").is_some_and(Value::is_object)
                    || !params.get("capabilities").is_some_and(Value::is_object)
                    || !params.get("protocolVersion").is_some_and(Value::is_string)
                {
                    return Some(rpc_error(
                        id,
                        -32602,
                        "initialize requires protocolVersion, capabilities and clientInfo",
                    ));
                }
                let requested = params["protocolVersion"].as_str().expect("checked");
                let protocol = match requested {
                    "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25" => requested,
                    _ => LATEST_PROTOCOL,
                };
                self.initialized = true;
                json!({"protocolVersion":protocol,"capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false},"prompts":{"listChanged":false}},"serverInfo":{"name":"soulsteads-agent-workboard","version":env!("CARGO_PKG_VERSION")},"instructions":"Use workboard_execute for all operations. Register the configured actor before mutations. Discover command variants in inputSchema. Read revisions before editing; retain lease tokens from claims. This server coordinates trusted local agents."})
            }
            "ping" => json!({}),
            _ if !self.initialized => return Some(rpc_error(id, -32002, "initialize first")),
            "tools/list" => {
                json!({"tools":[{"name":"workboard_execute","description":"Manage agent projects, epics, stories, tasks, bugs, subtasks, spikes, dependencies, assignments, state transitions, atomic leased claims, comments, events, boards and sprints. All mutations are attributed to the server's configured actor. Issue edits require expected_revision. Inspect the op alternatives in this schema.","inputSchema":command_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}}]})
            }
            "tools/call" => {
                if params.get("name").and_then(Value::as_str) != Some("workboard_execute") {
                    return Some(rpc_error(id, -32602, "unknown tool"));
                }
                match serde_json::from_value::<Command>(
                    params.get("arguments").cloned().unwrap_or(json!({})),
                ) {
                    Ok(command) => {
                        if self.api_key.is_some() && command.is_key_management() {
                            return Some(rpc_error(
                                id,
                                -32001,
                                "key administration requires local operator mode",
                            ));
                        }
                        tool_result(self.store.execute(&self.actor, command))
                    }
                    Err(error) => tool_result(Err(crate::Error::Invalid(error.to_string()))),
                }
            }
            "resources/list" => {
                json!({"resources":[{"uri":"workboard://workflow","name":"Issue workflow","mimeType":"application/json"},{"uri":"workboard://projects","name":"Projects","mimeType":"application/json"},{"uri":"workboard://agents","name":"Agents","mimeType":"application/json"}]})
            }
            "resources/templates/list" => {
                json!({"resourceTemplates":[{"uriTemplate":"workboard://issues/{key}","name":"Issue context","mimeType":"application/json"},{"uriTemplate":"workboard://projects/{key}/board","name":"Project board","mimeType":"application/json"}]})
            }
            "resources/read" => {
                let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                    return Some(rpc_error(id, -32602, "uri required"));
                };
                let command = match uri {
                    "workboard://workflow" => Some(Command::Workflow {}),
                    "workboard://projects" => Some(Command::ProjectList {}),
                    "workboard://agents" => Some(Command::AgentList {}),
                    _ => {
                        if let Some(key) = uri
                            .strip_prefix("workboard://issues/")
                            .filter(|s| !s.is_empty() && !s.contains('/'))
                        {
                            Some(Command::IssueGet { key: key.into() })
                        } else {
                            uri.strip_prefix("workboard://projects/")
                                .and_then(|s| s.strip_suffix("/board"))
                                .filter(|s| !s.is_empty() && !s.contains('/'))
                                .map(|key| Command::Board {
                                    project: key.into(),
                                    sprint: None,
                                })
                        }
                    }
                };
                let Some(c) = command else {
                    return Some(rpc_error(id, -32002, "resource not found"));
                };
                match self.store.execute(&self.actor, c) {
                    Ok(v) => resource(uri, v),
                    Err(e) => {
                        return Some(rpc_error(
                            id,
                            if e.code() == "not_found" {
                                -32002
                            } else {
                                -32603
                            },
                            &e.to_string(),
                        ));
                    }
                }
            }
            "prompts/list" => {
                json!({"prompts":[{"name":"work-issue","description":"Claim and complete work safely with revisions and leases","arguments":[{"name":"project","description":"Project key","required":true}]}]})
            }
            "prompts/get" => {
                if params.get("name").and_then(Value::as_str) != Some("work-issue") {
                    return Some(rpc_error(id, -32602, "unknown prompt"));
                }
                let Some(project) = params.pointer("/arguments/project").and_then(Value::as_str)
                else {
                    return Some(rpc_error(id, -32602, "project argument required"));
                };
                json!({"description":"Agent work loop","messages":[{"role":"user","content":{"type":"text","text":format!("Work on project {project}. Inspect the board and claim a ready issue using issue_claim. If claimed is null, report no available work. Read full issue context, linked dependencies and acceptance criteria. Retain the claim token, heartbeat before expiry, and use the latest revision and lease_token for edits/transitions. Add evidence in a comment, transition through in_review to done only after validation. Release interrupted work; never submit work with an expired or revoked token. Use events for handoff context.")}}]})
            }
            _ => return Some(rpc_error(id, -32601, "method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
}

pub fn serve(reader: impl BufRead, mut writer: impl Write, mut server: Server) -> io::Result<()> {
    let mut reader = reader;
    loop {
        let mut frame = Vec::new();
        let count = (&mut reader)
            .take(MAX_FRAME + 1)
            .read_until(b'\n', &mut frame)?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP frame exceeds 8 MiB",
            ));
        }
        if frame.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice(&frame) {
            Ok(request) => server.handle(request),
            Err(_) => Some(rpc_error(Value::Null, -32700, "parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
    Ok(())
}
