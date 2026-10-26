//! MCP server mode (`task-x-runtime --mcp-server --root <dir>`): serves the runtime's tool registry
//! over stdio (newline-delimited JSON-RPC, the MCP stdio transport). The Claude Code engine starts
//! it so Claude reads, searches, and edits through the same Rust tools as the built-in agent, with
//! the same protections: workspace-only paths, stale-edit checks, atomic
//! writes, and the command sandbox.

use crate::claude_cli::APPROVE_TOOL;
use crate::context::Workspace;
use crate::rpc::{self, RpcError};
use crate::tools::{self, EditLog, ToolCtx};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

pub const SERVER_NAME: &str = "task-x";
const LATEST_PROTOCOL: &str = "2025-06-18";
const MAX_LINE: usize = 32 * 1024 * 1024;
/// Other processes (builds, the user) change files too; search tools rescan at most this often.
const RESCAN_AFTER: Duration = Duration::from_secs(2);
const SEARCH_TOOLS: &[&str] = &[
    "search_text",
    "search_regex",
    "find_all",
    "find_symbol",
    "find_definition",
    "find_references",
    "search_files_by_content",
    "find_files",
    "list_files",
    "get_file_tree",
    "get_dependencies",
];

pub struct McpServer {
    workspace: Arc<RwLock<Option<Workspace>>>,
    seen: Arc<Mutex<HashMap<String, u64>>>,
    edits: Arc<Mutex<EditLog>>,
    root: PathBuf,
    scanned: Mutex<Instant>,
    /// Serve the tool registry (off with `--no-tools`: only the approval tool).
    tools: bool,
    /// Where the `approve` tool sends permission prompts: the runtime's address and token.
    approval: Option<(String, String)>,
}

impl McpServer {
    pub fn open(root: PathBuf, tools: bool, approval: Option<(String, String)>) -> Result<Self, String> {
        let ws = Workspace::open(&root, &[])?;
        let root = ws.index.root().to_path_buf();
        Ok(Self {
            workspace: Arc::new(RwLock::new(Some(ws))),
            seen: Arc::default(),
            edits: Arc::default(),
            root,
            scanned: Mutex::new(Instant::now()),
            tools,
            approval,
        })
    }

    fn list(&self) -> Vec<Value> {
        let mut list: Vec<Value> = if self.tools {
            tools::infos().iter().map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema })).collect()
        } else {
            Vec::new()
        };
        if self.approval.is_some() {
            list.push(json!({
                "name": APPROVE_TOOL,
                "description": "Asks the task-x user to approve or deny a tool call. Claude Code calls it for permission prompts.",
                "inputSchema": { "type": "object", "properties": { "tool_name": { "type": "string" }, "input": { "type": "object" }, "tool_use_id": { "type": "string" } }, "required": ["tool_name", "input"] },
            }));
        }
        list
    }

    fn rescan_if_stale(&self) {
        let mut at = self.scanned.lock().expect("scan lock");
        if at.elapsed() >= RESCAN_AFTER {
            if let Some(w) = self.workspace.write().expect("workspace lock").as_mut() {
                w.refresh();
            }
            *at = Instant::now();
        }
    }

    async fn call(&self, name: &str, args: Value) -> Value {
        if let Some((addr, token)) = self.approval.as_ref().filter(|_| name == APPROVE_TOOL) {
            let answer = crate::claude_cli::request_approval(addr, token, &args).await;
            return json!({ "content": [{ "type": "text", "text": answer.to_string() }], "isError": false });
        }
        let Some(def) = tools::find(name).filter(|_| self.tools) else {
            return json!({ "content": [{ "type": "text", "text": format!("Unknown tool '{name}'.") }], "isError": true });
        };
        if SEARCH_TOOLS.contains(&name) {
            self.rescan_if_stale();
        }
        let ctx = Arc::new(ToolCtx {
            root: self.root.clone(),
            workspace: self.workspace.clone(),
            cancel: CancellationToken::new(),
            seen: self.seen.clone(),
            edits: self.edits.clone(),
            task_id: None,
        });
        let input = if args.is_null() { json!({}) } else { args };
        match tools::call(def, ctx, input).await {
            Ok(v) => json!({ "content": [{ "type": "text", "text": v.to_string() }], "isError": false }),
            Err(e) => json!({ "content": [{ "type": "text", "text": json!({ "error": e }).to_string() }], "isError": true }),
        }
    }

    /// Handles one message; returns the reply line for requests (notifications get none).
    pub async fn handle(&self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(rpc::error(&Value::Null, &RpcError::new(rpc::PARSE_ERROR, format!("parse error: {e}")))),
        };
        let id = msg.get("id").cloned()?;
        let p = &msg["params"];
        let result = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": p["protocolVersion"].as_str().unwrap_or(LATEST_PROTOCOL),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Workspace tools from task-x. Use them to read, search, and edit files: edits are atomic, checked against what you last read, and limited to the workspace.",
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.list() })),
            "tools/call" => match p["name"].as_str() {
                Some(name) => Ok(self.call(name, p["arguments"].clone()).await),
                None => Err(RpcError::new(rpc::INVALID_PARAMS, "tools/call needs a tool name")),
            },
            m => Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("unknown method: {m}"))),
        };
        Some(match result {
            Ok(v) => rpc::response(&id, v),
            Err(e) => rpc::error(&id, &e),
        })
    }
}

/// Runs the server until stdin closes.
pub async fn serve(root: PathBuf, tools: bool, approval: Option<(String, String)>) -> ExitCode {
    let server = match tokio::task::spawn_blocking(move || McpServer::open(root, tools, approval)).await.map_err(|e| e.to_string()).and_then(|r| r) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[task-x-runtime] mcp server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = tokio::io::stdout();
    let mut lines = FramedRead::new(tokio::io::stdin(), LinesCodec::new_with_max_length(MAX_LINE));
    while let Some(Ok(line)) = lines.next().await {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(mut reply) = server.handle(&line).await {
            reply.push('\n');
            if stdout.write_all(reply.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::index::tests::temp_repo;

    async fn rpc(s: &McpServer, id: u32, method: &str, params: Value) -> Value {
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        serde_json::from_str(&s.handle(&line).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn serves_the_tool_registry_over_mcp() {
        let root = temp_repo(&[("src/app.ts", "export const color = 'red';\n"), (".env", "KEY=1\n")]);
        let s = McpServer::open(root.clone(), true, None).unwrap();
        let init = rpc(&s, 1, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t" } })).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "task-x");
        assert!(s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await.is_none());
        let list = rpc(&s, 2, "tools/list", json!({})).await;
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"replace_text") && names.contains(&"search_text") && names.contains(&"get_dependencies"));
        // Reads must come before edits (stale-edit check), exactly as in the built-in agent.
        let read = rpc(&s, 3, "tools/call", json!({ "name": "read_file", "arguments": { "path": "src/app.ts" } })).await;
        assert_eq!(read["result"]["isError"], false);
        let edit = rpc(&s, 4, "tools/call", json!({ "name": "replace_text", "arguments": { "path": "src/app.ts", "oldText": "red", "newText": "blue" } })).await;
        assert_eq!(edit["result"]["isError"], false, "{edit}");
        assert_eq!(std::fs::read_to_string(root.join("src/app.ts")).unwrap(), "export const color = 'blue';\n");
        let secret = rpc(&s, 5, "tools/call", json!({ "name": "read_file", "arguments": { "path": ".env" } })).await;
        assert_eq!(secret["result"]["isError"], false, "secret files are readable");
        let outside = rpc(&s, 6, "tools/call", json!({ "name": "read_file", "arguments": { "path": "../x" } })).await;
        assert_eq!(outside["result"]["isError"], true);
        assert_eq!(rpc(&s, 7, "nope", json!({})).await["error"]["code"], rpc::METHOD_NOT_FOUND);
        assert!(!names.contains(&APPROVE_TOOL), "no approval address, no approve tool");
        let only = McpServer::open(root.clone(), false, Some(("127.0.0.1:1".into(), "t".into()))).unwrap();
        let list = rpc(&only, 1, "tools/list", json!({})).await;
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 1, "--no-tools serves only the approve tool");
        let denied = rpc(&only, 2, "tools/call", json!({ "name": "approve", "arguments": { "tool_name": "Bash", "input": {} } })).await;
        assert!(denied["result"]["content"][0]["text"].as_str().unwrap().contains("\"deny\""), "no runtime to ask counts as deny");
        assert_eq!(rpc(&only, 3, "tools/call", json!({ "name": "read_file", "arguments": { "path": "src/app.ts" } })).await["result"]["isError"], true);
        std::fs::remove_dir_all(root).ok();
    }
}
