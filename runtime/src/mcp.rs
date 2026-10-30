//! Model Context Protocol client (stdio, streamable HTTP, and the older HTTP+SSE transport). Server
//! tools are exposed to the agent as `mcp__<server>__<tool>`, kept separate from the built-in
//! registry, and always go through the permission policy (permission class `network`, so they need
//! approval unless allow-listed).

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
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

/// One server, in the shape Claude Code (`mcpServers`) and VS Code (`servers`) use. Unknown fields
/// from those files (`envFile`, `gallery`, ...) are ignored.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    /// `stdio`, `http` (streamable HTTP), or `sse`. Default: `http` with a `url` and no `command`, else `stdio`.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default)]
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
    /// Server address for the `http` and `sse` transports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP headers for the `http` and `sse` transports (for example `Authorization`). Never logged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Stdio,
    Http,
    Sse,
}

impl ServerConfig {
    fn kind(&self) -> Result<Kind, String> {
        match self.transport.as_deref() {
            Some("stdio") => Ok(Kind::Stdio),
            Some("http" | "streamable-http" | "streamableHttp") => Ok(Kind::Http),
            Some("sse") => Ok(Kind::Sse),
            Some(t) => Err(format!("unknown transport type '{t}' (use stdio, http, or sse)")),
            None if self.url.is_some() && self.command.trim().is_empty() => Ok(Kind::Http),
            None => Ok(Kind::Stdio),
        }
    }

    /// The command line or URL, for status displays.
    fn target(&self) -> &str {
        if self.command.trim().is_empty() { self.url.as_deref().unwrap_or_default() } else { &self.command }
    }
}

/// Checks the URL and headers of a remote server. Headers (which often hold tokens) never go over
/// plain http to another machine.
fn remote(cfg: &ServerConfig) -> Result<(url::Url, HeaderMap), String> {
    let raw = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty()).ok_or("url is required for http and sse servers")?;
    let u = url::Url::parse(raw).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err("url must use http or https".into());
    }
    let loopback = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if u.scheme() == "http" && !loopback && !cfg.headers.is_empty() {
        return Err("refusing to send headers over plain http to a non-loopback host".into());
    }
    let mut h = HeaderMap::new();
    for (k, v) in &cfg.headers {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("invalid header name '{k}'"))?;
        let mut val = HeaderValue::from_str(v).map_err(|_| format!("invalid value for header '{k}'"))?;
        val.set_sensitive(true);
        h.insert(name, val);
    }
    Ok((u, h))
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().map_err(|e| format!("http client: {e}"))
}

/// Splits a `text/event-stream` body into `(event, data)` pairs.
#[derive(Default)]
struct SseReader {
    buf: Vec<u8>,
}

impl SseReader {
    fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buf.extend(chunk.iter().filter(|&&b| b != b'\r'));
        let mut out = Vec::new();
        while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = self.buf.drain(..end + 2).collect();
            let block = String::from_utf8_lossy(&block);
            let mut event = "message".to_owned();
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_owned();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_owned());
                }
            }
            if !data.is_empty() {
                out.push((event, data.join("\n")));
            }
        }
        out
    }
}

/// The answer to request `id` in a message (or a batch of messages), if it holds one.
fn answer(v: &Value, id: i64) -> Option<Result<Value, String>> {
    if let Some(list) = v.as_array() {
        return list.iter().find_map(|m| answer(m, id));
    }
    if v.get("id").and_then(Value::as_i64) != Some(id) || v.get("method").is_some() {
        return None;
    }
    Some(match v.get("error") {
        Some(e) => Err(e["message"].as_str().unwrap_or("server error").to_owned()),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    })
}

/// Routes one message from a stdio or SSE server: a response completes its request; a server
/// request gets the reply returned here (`ping` is answered, anything else declined).
fn dispatch(msg: &Value, pending: &Pending) -> Option<Value> {
    match (msg.get("id").and_then(Value::as_i64), msg.get("method").and_then(Value::as_str)) {
        (Some(id), None) => {
            if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                let _ = tx.send(match msg.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                });
            }
            None
        }
        (_, Some(method)) if msg.get("id").is_some() => {
            let id = msg["id"].clone();
            Some(if method == "ping" {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            } else {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "not supported by this client" } })
            })
        }
        _ => None,
    }
}

/// Asks the user for what an MCP server requests (`elicitation/create`). Set once by the server
/// loop; it sends `mcp/elicit` to the client and returns the MCP answer
/// (`{ action: "accept" | "decline" | "cancel", content? }`).
pub type Elicit = Arc<dyn Fn(String, Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> + Send + Sync>;
static ELICIT: std::sync::OnceLock<Elicit> = std::sync::OnceLock::new();

pub fn set_elicit(f: Elicit) {
    let _ = ELICIT.set(f);
}

/// A server request this client answers after asking the user (an elicitation). The reply is
/// sent when the user answers; without a handler the server gets "decline".
fn is_elicit(msg: &Value) -> bool {
    msg.get("id").is_some() && msg["method"] == "elicitation/create"
}

async fn elicit_reply(server: &str, msg: &Value) -> Value {
    let result = match ELICIT.get() {
        Some(f) => f(server.to_owned(), msg["params"].clone()).await,
        None => json!({ "action": "decline" }),
    };
    json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result })
}

fn fail_all(pending: &Pending) {
    for (_, tx) in pending.lock().expect("pending lock").drain() {
        let _ = tx.send(Err(json!({ "message": "server exited" })));
    }
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

/// A streamable-HTTP server: each request is one POST; the answer is JSON or an event stream.
struct HttpConn {
    name: String,
    http: reqwest::Client,
    url: url::Url,
    headers: HeaderMap,
    session: Mutex<Option<String>>,
}

impl HttpConn {
    async fn post(&self, msg: &Value) -> Result<reqwest::Response, String> {
        let mut req =
            self.http.post(self.url.clone()).headers(self.headers.clone()).header(ACCEPT, "application/json, text/event-stream").json(msg);
        if msg["method"] != "initialize" {
            req = req.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
        if let Some(s) = self.session.lock().expect("session lock").clone() {
            req = req.header("Mcp-Session-Id", s);
        }
        let r = req.send().await.map_err(|e| format!("could not reach {}: {e}", self.url))?;
        if let Some(s) = r.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().expect("session lock") = Some(s.to_owned());
        }
        match r.status() {
            s if s.is_success() => Ok(r),
            s if matches!(s.as_u16(), 401 | 403) => {
                Err(format!("{} answered HTTP {s}: the server needs sign-in; set an Authorization header for it", self.url))
            }
            s => Err(format!("{} answered HTTP {s}", self.url)),
        }
    }

    async fn request(&self, msg: &Value, id: i64) -> Result<Value, String> {
        let r = self.post(msg).await?;
        let stream = r.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
        if !stream {
            let v: Value = r.json().await.map_err(|e| format!("bad response from {}: {e}", self.url))?;
            return answer(&v, id).unwrap_or_else(|| Err("the server sent no response".into()));
        }
        let mut events = SseReader::default();
        let mut body = r.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| format!("reading from {}: {e}", self.url))?;
            for (_, data) in events.push(&chunk) {
                let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                // The server asks the user something while it works on this request.
                if is_elicit(&v) {
                    let reply = elicit_reply(&self.name, &v).await;
                    let _ = self.post(&reply).await;
                    continue;
                }
                if let Some(a) = answer(&v, id) {
                    return a;
                }
            }
        }
        Err("the server closed the stream without a response".into())
    }
}

enum Transport {
    Stdio {
        stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
        _child: Child,
    },
    Http(HttpConn),
    /// The older transport: one long GET event stream for answers, POSTs to the endpoint it names.
    Sse {
        http: reqwest::Client,
        headers: HeaderMap,
        endpoint: tokio::sync::watch::Receiver<Option<url::Url>>,
        reader: tokio::task::JoinHandle<()>,
    },
}

pub struct Client {
    name: String,
    transport: Transport,
    pending: Pending,
    next: AtomicI64,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<String>>,
    timeout: Duration,
    pub tools: Vec<McpTool>,
    /// Prompt templates and resources the server offers (shown in the chat's `/` and `#` menus).
    pub prompts: Vec<McpPrompt>,
    pub resources: Vec<McpResource>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: String,
    /// Argument names, required ones first.
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

const MAX_PROMPTS: usize = 100;
const MAX_RESOURCES: usize = 200;
/// Text of one prompt or resource given to the model.
const MAX_ITEM_CHARS: usize = 60_000;

impl Drop for Client {
    fn drop(&mut self) {
        if let Transport::Sse { reader, .. } = &self.transport {
            reader.abort();
        }
    }
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
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let stderr = Arc::new(Mutex::new(String::new()));
        let transport = match cfg.kind()? {
            Kind::Stdio => Self::spawn_stdio(name, cfg, root, &pending, &alive, &stderr)?,
            Kind::Http => {
                let (url, headers) = remote(cfg)?;
                Transport::Http(HttpConn { name: name.into(), http: http_client()?, url, headers, session: Mutex::new(None) })
            }
            Kind::Sse => {
                let (url, headers) = remote(cfg)?;
                Self::open_sse(name, url, headers, &pending, &alive).await?
            }
        };
        let timeout = Duration::from_millis(cfg.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1_000, 600_000));
        Ok(Self {
            name: name.into(),
            transport,
            pending,
            next: AtomicI64::new(1),
            alive,
            stderr,
            timeout,
            tools: Vec::new(),
            prompts: Vec::new(),
            resources: Vec::new(),
        })
    }

    fn spawn_stdio(
        name: &str,
        cfg: &ServerConfig,
        root: Option<&Path>,
        pending: &Pending,
        alive: &Arc<AtomicBool>,
        stderr: &Arc<Mutex<String>>,
    ) -> Result<Transport, String> {
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
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let (reply_to, pending, alive, server) = (stdin.clone(), pending.clone(), alive.clone(), name.to_owned());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                // An elicitation waits for the user, so it is answered on its own task.
                if is_elicit(&msg) {
                    let (reply_to, server) = (reply_to.clone(), server.clone());
                    tokio::spawn(async move {
                        let resp = elicit_reply(&server, &msg).await;
                        let mut w = reply_to.lock().await;
                        let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
                        let _ = w.flush().await;
                    });
                    continue;
                }
                if let Some(resp) = dispatch(&msg, &pending) {
                    let mut w = reply_to.lock().await;
                    let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
                    let _ = w.flush().await;
                }
            }
            alive.store(false, Ordering::SeqCst);
            fail_all(&pending);
        });
        Ok(Transport::Stdio { stdin, _child: child })
    }

    async fn open_sse(name: &str, url: url::Url, headers: HeaderMap, pending: &Pending, alive: &Arc<AtomicBool>) -> Result<Transport, String> {
        let server = name.to_owned();
        let http = http_client()?;
        let resp = http
            .get(url.clone())
            .headers(headers.clone())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| format!("could not reach {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url} answered HTTP {}", resp.status()));
        }
        let (tx, rx) = tokio::sync::watch::channel(None);
        let (pending, alive, post, post_headers) = (pending.clone(), alive.clone(), http.clone(), headers.clone());
        let reader = tokio::spawn(async move {
            let mut events = SseReader::default();
            let mut body = resp.bytes_stream();
            let mut endpoint: Option<url::Url> = None;
            while let Some(Ok(chunk)) = body.next().await {
                for (event, data) in events.push(&chunk) {
                    if event == "endpoint" {
                        // Requests (with their headers) only go back to the server's own origin.
                        endpoint = url.join(data.trim()).ok().filter(|e| e.origin() == url.origin());
                        let _ = tx.send(endpoint.clone());
                    } else if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                        if is_elicit(&msg) {
                            if let Some(ep) = endpoint.clone() {
                                let (post, headers, server) = (post.clone(), post_headers.clone(), server.clone());
                                tokio::spawn(async move {
                                    let resp = elicit_reply(&server, &msg).await;
                                    let _ = post.post(ep).headers(headers).json(&resp).send().await;
                                });
                            }
                            continue;
                        }
                        if let (Some(resp), Some(ep)) = (dispatch(&msg, &pending), endpoint.clone()) {
                            let _ = post.post(ep).headers(post_headers.clone()).json(&resp).send().await;
                        }
                    }
                }
            }
            alive.store(false, Ordering::SeqCst);
            fail_all(&pending);
        });
        Ok(Transport::Sse { http, headers, endpoint: rx, reader })
    }

    /// Sends one message without waiting for an answer (a request on stdio or SSE, or a notification).
    async fn send(&self, msg: &Value) -> Result<(), String> {
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut w = stdin.lock().await;
                w.write_all(format!("{msg}\n").as_bytes()).await.map_err(|e| format!("write to {}: {e}", self.name))?;
                w.flush().await.map_err(|e| e.to_string())
            }
            Transport::Http(h) => h.post(msg).await.map(drop),
            Transport::Sse { http, headers, endpoint, .. } => {
                let mut rx = endpoint.clone();
                let ep = match tokio::time::timeout(INIT_TIMEOUT, rx.wait_for(Option::is_some)).await {
                    Ok(Ok(r)) => r.clone(),
                    Ok(Err(_)) => None,
                    Err(_) => return Err(format!("{} sent no endpoint on its event stream", self.name)),
                };
                let ep = ep.ok_or_else(|| format!("the event stream of {} closed", self.name))?;
                let r = http.post(ep).headers(headers.clone()).json(msg).send().await.map_err(|e| format!("post to {}: {e}", self.name))?;
                if r.status().is_success() { Ok(()) } else { Err(format!("{} answered HTTP {}", self.name, r.status())) }
            }
        }
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration, cancel: &CancellationToken) -> Result<Value, String> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(format!("MCP server '{}' is not running. {}", self.name, self.stderr_tail()));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let cancelled =
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id, "reason": "cancelled" } });
        if let Transport::Http(h) = &self.transport {
            return tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = self.send(&cancelled).await;
                    Err("cancelled".to_owned())
                }
                r = tokio::time::timeout(timeout, h.request(&msg, id)) => {
                    r.unwrap_or_else(|_| Err(format!("{method} timed out after {}s", timeout.as_secs())))
                }
            };
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        if let Err(e) = self.send(&msg).await {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(e);
        }
        let res = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = self.send(&cancelled).await;
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
        // Elicitation is offered only when there is a user to ask.
        let caps = if ELICIT.get().is_some() { json!({ "elicitation": {} }) } else { json!({}) };
        let init = json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": caps, "clientInfo": { "name": "task-x", "version": env!("CARGO_PKG_VERSION") } });
        let info = self
            .request("initialize", init, INIT_TIMEOUT, &never)
            .await
            .map_err(|e| format!("initialize failed: {e} {}", self.stderr_tail()))?;
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
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
        // Prompts and resources are extras: a server that fails to list them still works for its tools.
        let has = |k: &str| info["capabilities"].get(k).is_some_and(|v| !v.is_null());
        if has("prompts") {
            if let Ok(r) = self.request("prompts/list", json!({}), INIT_TIMEOUT, &never).await {
                for p in r["prompts"].as_array().into_iter().flatten().take(MAX_PROMPTS) {
                    let Some(name) = p["name"].as_str() else { continue };
                    let mut args: Vec<(bool, String)> = p["arguments"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| Some((a["required"].as_bool() != Some(true), a["name"].as_str()?.to_owned())))
                        .collect();
                    args.sort();
                    self.prompts.push(McpPrompt {
                        server: self.name.clone(),
                        name: name.into(),
                        description: p["description"].as_str().unwrap_or_default().chars().take(300).collect(),
                        arguments: args.into_iter().map(|(_, n)| n).collect(),
                    });
                }
            }
        }
        if has("resources") {
            if let Ok(r) = self.request("resources/list", json!({}), INIT_TIMEOUT, &never).await {
                for x in r["resources"].as_array().into_iter().flatten().take(MAX_RESOURCES) {
                    let Some(uri) = x["uri"].as_str() else { continue };
                    self.resources.push(McpResource {
                        server: self.name.clone(),
                        uri: uri.into(),
                        name: x["name"].as_str().unwrap_or(uri).into(),
                        description: x["description"].as_str().unwrap_or_default().chars().take(300).collect(),
                        mime_type: x["mimeType"].as_str().map(str::to_owned),
                    });
                }
            }
        }
        Ok(())
    }

    /// A prompt template filled with `arguments`, as one text (the messages joined by role).
    pub async fn get_prompt(&self, name: &str, arguments: &serde_json::Map<String, Value>, cancel: &CancellationToken) -> Result<String, String> {
        let r = self.request("prompts/get", json!({ "name": name, "arguments": arguments }), self.timeout, cancel).await?;
        let text: Vec<String> = r["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let c = &m["content"];
                let t = match c["type"].as_str() {
                    Some("text") => c["text"].as_str()?.to_owned(),
                    Some("resource") => c["resource"]["text"].as_str()?.to_owned(),
                    _ => return None,
                };
                Some(if m["role"] == "assistant" { format!("(assistant) {t}") } else { t })
            })
            .collect();
        Ok(text.join("\n\n").chars().take(MAX_ITEM_CHARS).collect())
    }

    /// The text of a resource (binary contents are named, not included).
    pub async fn read_resource(&self, uri: &str, cancel: &CancellationToken) -> Result<String, String> {
        let r = self.request("resources/read", json!({ "uri": uri }), self.timeout, cancel).await?;
        let text: Vec<String> = r["contents"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| c["text"].as_str().map_or_else(|| format!("[{} content omitted]", c["mimeType"].as_str().unwrap_or("binary")), str::to_owned))
            .collect();
        Ok(text.join("\n").chars().take(MAX_ITEM_CHARS).collect())
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
    let p = [".task-x", ".anvil"]
        .iter()
        .map(|d| root.join(d).join("mcp.json"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.join(".task-x").join("mcp.json"));
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

    pub fn client(&self, server: &str) -> Option<Arc<Client>> {
        self.clients.get(server).filter(|c| c.alive()).cloned()
    }

    /// Every running server's prompts and resources.
    pub fn catalog(&self) -> Value {
        let mut prompts: Vec<&McpPrompt> = self.clients.values().filter(|c| c.alive()).flat_map(|c| &c.prompts).collect();
        let mut resources: Vec<&McpResource> = self.clients.values().filter(|c| c.alive()).flat_map(|c| &c.resources).collect();
        prompts.sort_by(|a, b| (&a.server, &a.name).cmp(&(&b.server, &b.name)));
        resources.sort_by(|a, b| (&a.server, &a.name).cmp(&(&b.server, &b.name)));
        json!({ "prompts": prompts, "resources": resources })
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
                    "command": cfg.target(),
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
            ServerConfig { command: "sh".into(), args: vec!["-c".into(), SERVER.into()], timeout_ms: Some(5_000), ..Default::default() },
        );
        cfgs.insert("broken".into(), ServerConfig { command: "definitely-missing-mcp-xyz".into(), ..Default::default() });
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

    /// A server with prompts and resources, and a tool that asks the user (elicitation) before it answers.
    const RICH: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"prompts":{},"resources":{}},"serverInfo":{"name":"r","version":"1"}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ask","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"prompts/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"prompts":[{"name":"review","description":"Review a file","arguments":[{"name":"style"},{"name":"file","required":true}]}]}}\n' "$id" ;;
    *'"prompts/get"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"messages":[{"role":"user","content":{"type":"text","text":"Review main.rs"}}]}}\n' "$id" ;;
    *'"resources/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"uri":"db://schema","name":"schema"}]}}\n' "$id" ;;
    *'"resources/read"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"contents":[{"uri":"db://schema","text":"create table t"}]}}\n' "$id" ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":900,"method":"elicitation/create","params":{"message":"Name?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}}}}}\n'
      IFS= read -r reply
      action=$(printf '%s' "$reply" | sed -n 's/.*"action":"\([a-z]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$action" ;;
  esac
done
"#;

    #[tokio::test]
    async fn lists_prompts_and_resources_and_answers_elicitation() {
        let mut m = Manager::default();
        let mut cfgs = BTreeMap::new();
        cfgs.insert("rich".into(), ServerConfig { command: "sh".into(), args: vec!["-c".into(), RICH.into()], timeout_ms: Some(5_000), ..Default::default() });
        m.configure(cfgs, None).await;
        let cat = m.catalog();
        assert_eq!(cat["prompts"][0]["name"], "review");
        assert_eq!(cat["prompts"][0]["arguments"], json!(["file", "style"]), "required arguments first");
        assert_eq!(cat["resources"][0]["uri"], "db://schema");
        let c = m.client("rich").unwrap();
        let cancel = CancellationToken::new();
        assert_eq!(c.get_prompt("review", &serde_json::Map::new(), &cancel).await.unwrap(), "Review main.rs");
        assert_eq!(c.read_resource("db://schema", &cancel).await.unwrap(), "create table t");
        // The server's question is answered (by the user handler, or declined without one), and the call finishes.
        let (c, t) = m.find("mcp__rich__ask").unwrap();
        let action = c.call(&t, json!({}), &cancel).await.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(["accept", "decline", "cancel"].contains(&action.as_str()), "{action}");
    }

    #[test]
    fn reads_claude_and_vscode_server_shapes() {
        let cfg = |v: Value| serde_json::from_value::<ServerConfig>(v).unwrap();
        let chrome = cfg(json!({ "type": "stdio", "command": "npx", "args": ["chrome-devtools-mcp@latest"], "envFile": ".env" }));
        assert_eq!(chrome.kind(), Ok(Kind::Stdio));
        let figma = cfg(json!({ "type": "http", "url": "http://127.0.0.1:3845/mcp" }));
        assert_eq!(figma.kind(), Ok(Kind::Http));
        assert_eq!(figma.target(), "http://127.0.0.1:3845/mcp");
        assert_eq!(cfg(json!({ "url": "https://mcp.example.com/mcp" })).kind(), Ok(Kind::Http), "a url alone means http");
        assert_eq!(cfg(json!({ "type": "sse", "url": "https://x.dev/sse" })).kind(), Ok(Kind::Sse));
        assert!(cfg(json!({ "type": "ws", "url": "ws://x" })).kind().is_err());
    }

    #[test]
    fn remote_servers_keep_headers_safe() {
        let cfg = |url: &str, auth: bool| ServerConfig {
            url: Some(url.into()),
            headers: if auth { BTreeMap::from([("Authorization".into(), "Bearer t".into())]) } else { BTreeMap::new() },
            ..Default::default()
        };
        assert!(remote(&cfg("https://mcp.figma.com/mcp", true)).is_ok());
        assert!(remote(&cfg("http://127.0.0.1:3845/mcp", true)).is_ok(), "loopback may use plain http");
        assert!(remote(&cfg("http://example.com/mcp", false)).is_ok());
        assert!(remote(&cfg("http://example.com/mcp", true)).is_err(), "no token over plain http to another host");
        assert!(remote(&cfg("file:///etc/passwd", false)).is_err());
        assert!(remote(&ServerConfig { transport: Some("http".into()), ..Default::default() }).is_err(), "http needs a url");
        let (_, h) = remote(&cfg("https://x.dev/mcp", true)).unwrap();
        assert!(h["authorization"].is_sensitive());
    }

    #[test]
    fn splits_event_streams_and_finds_answers() {
        let mut r = SseReader::default();
        assert!(r.push(b"event: endpoint\r\ndata: /messages?s=1").is_empty(), "an event waits for its blank line");
        assert_eq!(r.push(b"\r\n\r\ndata: {\"id\":2,"), vec![("endpoint".into(), "/messages?s=1".into())]);
        let ev = r.push("\"result\":{\"ok\":\"é\"}}\n\n".as_bytes());
        assert_eq!(ev.len(), 1);
        let v: Value = serde_json::from_str(&ev[0].1).unwrap();
        assert_eq!(answer(&v, 2), Some(Ok(json!({ "ok": "é" }))));
        assert_eq!(answer(&v, 3), None);
        assert_eq!(answer(&json!([{ "id": 4, "error": { "message": "nope" } }]), 4), Some(Err("nope".into())));
        assert_eq!(answer(&json!({ "id": 5, "method": "ping" }), 5), None, "a server request is not an answer");
    }

    /// One streamable-HTTP MCP server on a loopback port, answering in JSON or as an event stream.
    async fn http_server() -> String {
        use tokio::io::AsyncReadExt as _;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let body = loop {
                        let n = s.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let len = text[..i]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if buf.len() >= i + 4 + len {
                                break text[i + 4..i + 4 + len].to_owned();
                            }
                        }
                    };
                    let msg: Value = serde_json::from_str(&body).unwrap_or_default();
                    let id = msg["id"].clone();
                    let (ctype, out) = match msg["method"].as_str().unwrap_or_default() {
                        "initialize" => (
                            "application/json",
                            json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": PROTOCOL_VERSION, "capabilities": {} } })
                                .to_string(),
                        ),
                        "tools/list" => (
                            "text/event-stream",
                            format!(
                                "event: message\ndata: {}\n\n",
                                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "get_screenshot", "inputSchema": { "type": "object" } }] } })
                            ),
                        ),
                        "tools/call" => (
                            "application/json",
                            json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "frame" }] } })
                                .to_string(),
                        ),
                        _ => {
                            let _ = s.write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                            return;
                        }
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\nmcp-session-id: s1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        out.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(out.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    #[tokio::test]
    async fn talks_to_a_streamable_http_server() {
        let url = http_server().await;
        let mut m = Manager::default();
        m.configure(
            BTreeMap::from([("figma".into(), ServerConfig { transport: Some("http".into()), url: Some(url), ..Default::default() })]),
            None,
        )
        .await;
        assert_eq!(m.status()["servers"][0]["error"], Value::Null, "{}", m.status());
        let (c, t) = m.find("mcp__figma__get_screenshot").unwrap();
        assert_eq!(c.call(&t, json!({}), &CancellationToken::new()).await.unwrap()["content"], "frame");
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
