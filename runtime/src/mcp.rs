//! Model Context Protocol client (stdio transport). Server tools are exposed to the agent as
//! `mcp__<server>__<tool>`, kept separate from the built-in registry, and always go through the
//! permission policy (permission class `network`, so they need approval unless allow-listed).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TOOLS: usize = 200;
const MAX_STDERR: usize = 8 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub qualified: String,
    pub description: String,
    pub input_schema: Value,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>;

pub struct Client {
    name: String,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next: AtomicI64,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<String>>,
    timeout: Duration,
    pub tools: Vec<McpTool>,
    _child: Child,
}

pub fn qualify(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let q = format!("mcp__{}__{}", clean(server), clean(tool));
    q.chars().take(64).collect()
}

/// Checks the input against the top level of a JSON schema: object type, required keys, and
/// primitive property types. Full validation stays with the server.
pub fn check_input(schema: &Value, input: &Value) -> Result<(), String> {
    let obj = input.as_object().ok_or("input must be a JSON object")?;
    for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !obj.contains_key(r) {
            return Err(format!("missing required field '{r}'"));
        }
    }
    if let Some(props) = schema["properties"].as_object() {
        for (k, v) in obj {
            let Some(t) = props.get(k).and_then(|p| p["type"].as_str()) else { continue };
            let ok = match t {
                "string" => v.is_string(),
                "number" => v.is_number(),
                "integer" => v.is_i64() || v.is_u64(),
                "boolean" => v.is_boolean(),
                "array" => v.is_array(),
                "object" => v.is_object(),
                "null" => v.is_null(),
                _ => true,
            };
            if !ok {
                return Err(format!("field '{k}' must be of type {t}"));
            }
        }
        if schema["additionalProperties"] == json!(false) {
            if let Some(k) = obj.keys().find(|k| !props.contains_key(*k)) {
                return Err(format!("unknown field '{k}'"));
            }
        }
    }
    Ok(())
}

impl Client {
    pub async fn start(name: &str, cfg: &ServerConfig, root: Option<&Path>) -> Result<Self, String> {
        if cfg.command.trim().is_empty() || cfg.args.iter().any(|a| a.contains('\0')) {
            return Err("command must be non-empty".into());
        }
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args).envs(&cfg.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        if let Some(d) = cfg.cwd.as_deref().or(root) {
            cmd.current_dir(d);
        }
        let mut child = cmd.spawn().map_err(|e| format!("could not start '{}': {e}", cfg.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut stderr_pipe = child.stderr.take().ok_or("no stderr")?;
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let stderr = Arc::new(Mutex::new(String::new()));
        {
            let stderr = stderr.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stderr_pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut s = stderr.lock().expect("stderr lock");
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if s.len() > MAX_STDERR {
                        let cut = s.len() - MAX_STDERR;
                        let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
                        s.drain(..cut);
                    }
                }
            });
        }
        let client = Self {
            name: name.into(),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            pending: pending.clone(),
            next: AtomicI64::new(1),
            alive: alive.clone(),
            stderr,
            timeout: Duration::from_millis(cfg.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1_000, 600_000)),
            tools: Vec::new(),
            _child: child,
        };
        let reply_to = client.stdin.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match (msg.get("id").and_then(Value::as_i64), msg.get("method").and_then(Value::as_str)) {
                    (Some(id), None) => {
                        if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                            let _ = tx.send(match msg.get("error") {
                                Some(e) => Err(e.clone()),
                                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                            });
                        }
                    }
                    (_, Some(method)) if msg.get("id").is_some() => {
                        // Server → client requests: answer ping; decline everything else.
                        let id = msg["id"].clone();
                        let resp = if method == "ping" {
                            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                        } else {
                            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "not supported by this client" } })
                        };
                        let mut w = reply_to.lock().await;
                        let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
                        let _ = w.flush().await;
                    }
                    _ => {}
                }
            }
            alive.store(false, Ordering::SeqCst);
            for (_, tx) in pending.lock().expect("pending lock").drain() {
                let _ = tx.send(Err(json!({ "message": "server exited" })));
            }
        });
        Ok(client)
    }

    async fn write(&self, msg: Value) -> Result<(), String> {
        let mut w = self.stdin.lock().await;
        w.write_all(format!("{msg}\n").as_bytes()).await.map_err(|e| format!("write to {}: {e}", self.name))?;
        w.flush().await.map_err(|e| e.to_string())
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration, cancel: &CancellationToken) -> Result<Value, String> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(format!("MCP server '{}' is not running. {}", self.name, self.stderr_tail()));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        let res = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = self.write(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id, "reason": "cancelled" } })).await;
                Err("cancelled".to_owned())
            }
            r = tokio::time::timeout(timeout, rx) => match r {
                Err(_) => Err(format!("{method} timed out after {}s", timeout.as_secs())),
                Ok(Err(_)) => Err("server closed the request".into()),
                Ok(Ok(Err(e))) => Err(e["message"].as_str().unwrap_or("server error").to_owned()),
                Ok(Ok(Ok(v))) => Ok(v),
            },
        };
        self.pending.lock().expect("pending lock").remove(&id);
        res
    }

    fn stderr_tail(&self) -> String {
        let s = self.stderr.lock().expect("stderr lock");
        s.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    pub async fn initialize(&mut self) -> Result<(), String> {
        let never = CancellationToken::new();
        let init = json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": { "name": "task-x", "version": env!("CARGO_PKG_VERSION") } });
        self.request("initialize", init, INIT_TIMEOUT, &never)
            .await
            .map_err(|e| format!("initialize failed: {e} {}", self.stderr_tail()))?;
        self.write(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({ "cursor": c }));
            let r = self.request("tools/list", params, INIT_TIMEOUT, &never).await?;
            for t in r["tools"].as_array().into_iter().flatten() {
                let Some(name) = t["name"].as_str() else { continue };
                if self.tools.len() >= MAX_TOOLS {
                    break;
                }
                let schema = if t["inputSchema"].is_object() { t["inputSchema"].clone() } else { json!({ "type": "object" }) };
                self.tools.push(McpTool {
                    server: self.name.clone(),
                    name: name.into(),
                    qualified: qualify(&self.name, name),
                    description: format!("[MCP {}] {}", self.name, t["description"].as_str().unwrap_or("")).chars().take(1_024).collect(),
                    input_schema: schema,
                });
            }
            cursor = r["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() || self.tools.len() >= MAX_TOOLS {
                break;
            }
        }
        Ok(())
    }

    pub async fn call(&self, tool: &McpTool, input: Value, cancel: &CancellationToken) -> Result<Value, String> {
        check_input(&tool.input_schema, &input)?;
        let r = self.request("tools/call", json!({ "name": tool.name, "arguments": input }), self.timeout, cancel).await?;
        let text: Vec<String> = r["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| match c["type"].as_str() {
                Some("text") => c["text"].as_str().unwrap_or_default().to_owned(),
                Some(t) => format!("[{t} content omitted]"),
                None => String::new(),
            })
            .collect();
        let out = json!({ "content": text.join("\n"), "structured": r.get("structuredContent") });
        if r["isError"].as_bool() == Some(true) { Err(text.join("\n")) } else { Ok(out) }
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub struct Manager {
    clients: HashMap<String, Arc<Client>>,
    errors: BTreeMap<String, String>,
    configs: BTreeMap<String, ServerConfig>,
}

pub fn load_workspace(root: &Path) -> Result<BTreeMap<String, ServerConfig>, String> {
    // .task-x/mcp.json; .anvil/mcp.json is the old location.
    let p = [".task-x", ".anvil"].iter().map(|d| root.join(d).join("mcp.json")).find(|p| p.is_file()).unwrap_or_else(|| root.join(".task-x").join("mcp.json"));
    if !p.is_file() {
        return Ok(BTreeMap::new());
    }
    #[derive(Deserialize)]
    struct File {
        servers: BTreeMap<String, ServerConfig>,
    }
    let t = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str::<File>(&t).map(|f| f.servers).map_err(|e| format!("{}: {e}", p.display()))
}

impl Manager {
    /// Starts the configured servers (in parallel), replacing any running ones.
    pub async fn configure(&mut self, configs: BTreeMap<String, ServerConfig>, root: Option<PathBuf>) {
        self.clients.clear();
        self.errors.clear();
        let starts = configs.iter().filter(|(_, c)| !c.disabled).map(|(name, cfg)| {
            let (name, cfg, root) = (name.clone(), cfg.clone(), root.clone());
            async move {
                let r = async {
                    let mut c = Client::start(&name, &cfg, root.as_deref()).await?;
                    c.initialize().await?;
                    Ok::<_, String>(c)
                }
                .await;
                (name, r)
            }
        });
        for (name, r) in futures_util::future::join_all(starts).await {
            match r {
                Ok(c) => {
                    self.clients.insert(name, Arc::new(c));
                }
                Err(e) => {
                    self.errors.insert(name, e);
                }
            }
        }
        self.configs = configs;
    }

    pub fn tools(&self) -> Vec<McpTool> {
        let mut t: Vec<McpTool> = self.clients.values().filter(|c| c.alive()).flat_map(|c| c.tools.clone()).collect();
        t.sort_by(|a, b| a.qualified.cmp(&b.qualified));
        t
    }

    pub fn find(&self, qualified: &str) -> Option<(Arc<Client>, McpTool)> {
        self.clients.values().find_map(|c| c.tools.iter().find(|t| t.qualified == qualified).map(|t| (c.clone(), t.clone())))
    }

    pub fn status(&self) -> Value {
        let servers: Vec<Value> = self
            .configs
            .iter()
            .map(|(name, cfg)| {
                let c = self.clients.get(name);
                json!({
                    "name": name,
                    "command": cfg.command,
                    "disabled": cfg.disabled,
                    "running": c.is_some_and(|c| c.alive()),
                    "tools": c.map(|c| c.tools.iter().map(|t| t.qualified.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                    "error": self.errors.get(name),
                })
            })
            .collect();
        json!({ "servers": servers })
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    /// A tiny MCP server written in POSIX shell: answers initialize, tools/list, and tools/call.
    pub const SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"t","version":"1"}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}\n' "$id" ;;
    *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
  esac
done
"#;

    #[tokio::test]
    async fn starts_lists_and_calls_tools() {
        let mut m = Manager::default();
        let mut cfgs = BTreeMap::new();
        cfgs.insert(
            "demo".into(),
            ServerConfig {
                command: "sh".into(),
                args: vec!["-c".into(), SERVER.into()],
                env: BTreeMap::new(),
                cwd: None,
                timeout_ms: Some(5_000),
                disabled: false,
            },
        );
        cfgs.insert(
            "broken".into(),
            ServerConfig {
                command: "definitely-missing-mcp-xyz".into(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
                timeout_ms: None,
                disabled: false,
            },
        );
        m.configure(cfgs, None).await;
        let tools = m.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].qualified, "mcp__demo__echo");
        let (c, t) = m.find("mcp__demo__echo").unwrap();
        let cancel = CancellationToken::new();
        assert_eq!(c.call(&t, json!({ "text": "ping" }), &cancel).await.unwrap()["content"], "pong");
        assert!(c.call(&t, json!({}), &cancel).await.unwrap_err().contains("missing required field"));
        let st = m.status();
        assert!(st["servers"].as_array().unwrap().iter().any(|s| s["name"] == "broken" && s["error"].is_string()));
    }

    #[test]
    fn qualifies_and_checks_inputs() {
        assert_eq!(qualify("my server", "do.it"), "mcp__my_server__do_it");
        let schema = json!({ "type": "object", "properties": { "n": { "type": "integer" } }, "additionalProperties": false });
        assert!(check_input(&schema, &json!({ "n": 1 })).is_ok());
        assert!(check_input(&schema, &json!({ "n": "x" })).is_err());
        assert!(check_input(&schema, &json!({ "m": 1 })).is_err());
        assert!(check_input(&schema, &json!([])).is_err());
    }
}
