//! Browser automation over the Chrome DevTools Protocol (CDP).
//!
//! task-x drives a browser the user already has open, through its DevTools endpoint: a Chrome or
//! Edge started with `--remote-debugging-port=9222`, or VS Code itself started with the same flag.
//! In VS Code the page inside a *Simple Browser* view is an out-of-process iframe, so it is listed
//! as an `iframe` target and is driven like a tab. VS Code's own UI (the workbench and the webview
//! hosts, `vscode-file:` and `vscode-webview:` URLs) is never offered as a target.
//!
//! Under the VS Code extension there is a second way in, which needs no restart and no debug port:
//! the extension serves a loopback bridge (`TASK_X_VSCODE_BROWSER`, with a bearer token in
//! `TASK_X_VSCODE_BROWSER_TOKEN`) that calls VS Code's own integrated-browser tools
//! (`open_browser_page`, `read_page`, `click_element`, ...). It is used unless `TASK_X_CDP_ENDPOINT`
//! is set.
//!
//! The endpoint is the one place where reaching loopback is required, so it is checked by its own
//! rule: only a local http endpoint is accepted. `web_fetch` keeps refusing loopback.

use super::schema::{int, object};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9222";
const ENV_ENDPOINT: &str = "TASK_X_CDP_ENDPOINT";
const VSCODE_ENV: &str = "TASK_X_VSCODE_BROWSER";
const VSCODE_TOKEN_ENV: &str = "TASK_X_VSCODE_BROWSER_TOKEN";
/// VS Code may ask the user before a page opens, so a bridge call can wait on a person.
const VSCODE_TIMEOUT: Duration = Duration::from_secs(120);
/// The VS Code page the last call used: the default target when the model names none.
static LAST_PAGE: Mutex<Option<String>> = Mutex::new(None);
const CDP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WAIT_MS: u64 = 5_000;
const MAX_WAIT_MS: u64 = 30_000;
const POLL: Duration = Duration::from_millis(100);
/// URL schemes of VS Code's and the browser's own UI. Targets with these URLs are never driven.
const INTERNAL_SCHEMES: &[&str] = &["vscode-file", "vscode-webview", "vscode", "devtools"];
const START_HINT: &str = "Open Chrome or Edge with --remote-debugging-port=9222, or quit VS Code and start it with \
     `code --remote-debugging-port=9222` to drive its Simple Browser, or set TASK_X_CDP_ENDPOINT.";

const ACTIONS: &[&str] =
    &["tabs", "open", "goto", "click", "type", "press", "screenshot", "text", "html", "eval", "wait", "back", "forward", "close"];

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "browser",
        "Drive the browser the user has open, over the Chrome DevTools Protocol: list tabs (tabs), open a page (open), navigate (goto), click a CSS selector (click), type into a selector (type), press a key (press), read the visible text (text) or the HTML (html), take a screenshot (screenshot), run JavaScript in the page (eval), wait for a selector (wait), go back or forward (back, forward), and close the tab (close). Inside VS Code it drives VS Code's integrated browser (open puts the page in an editor tab; the target is the page id open returns; text returns an accessibility snapshot; close is not available). Outside VS Code it needs a Chrome or Edge started with --remote-debugging-port.",
        Permission::Network,
        || {
            object(
                json!({
                    "action": { "type": "string", "enum": ACTIONS, "description": "What to do" },
                    "target": { "type": "string", "description": "Tab id from the tabs action (default: the first page)" },
                    "url": { "type": "string", "description": "http(s) URL for open and goto" },
                    "selector": { "type": "string", "description": "CSS selector for click, type, and wait" },
                    "text": { "type": "string", "description": "Text to insert for type" },
                    "key": { "type": "string", "description": "Key name for press, e.g. Enter, Tab, Escape, ArrowDown" },
                    "script": { "type": "string", "description": "JavaScript expression for eval; its value is returned" },
                    "timeoutMs": int("How long open, goto, and wait may take (default 5000, max 30000)", 0),
                }),
                &["action"],
            )
        },
        browser,
    )]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserInput {
    action: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// The DevTools endpoint: `TASK_X_CDP_ENDPOINT`, else Chrome's default debugging port.
fn endpoint() -> Result<url::Url, ToolError> {
    let raw = std::env::var(ENV_ENDPOINT).ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_ENDPOINT.into());
    let url = url::Url::parse(raw.trim()).map_err(|e| ToolError::invalid(format!("{ENV_ENDPOINT} is not a URL: {e}")))?;
    check_endpoint(&url)?;
    Ok(url)
}

/// The DevTools endpoint must be local http: that is what makes it the browser on this machine.
fn check_endpoint(url: &url::Url) -> Result<(), ToolError> {
    const HINT: &str = "start Chrome, Edge, or VS Code with --remote-debugging-port=9222 and set TASK_X_CDP_ENDPOINT to its local address";
    if url.scheme() != "http" {
        return Err(ToolError::invalid(format!("the browser endpoint must be http, not {}; {HINT}", url.scheme())));
    }
    let host = url.host_str().ok_or_else(|| ToolError::invalid(format!("the browser endpoint needs a host; {HINT}")))?;
    let name = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let local = if let Ok(ip) = name.parse::<IpAddr>() { ip.is_loopback() } else { name == "localhost" };
    if !local {
        return Err(ToolError::invalid(format!("the browser endpoint must be this machine ({host} is not loopback); {HINT}")));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct Target {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    ws: Option<String>,
}

fn http() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(CDP_TIMEOUT)
        .build()
        .map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("could not create an HTTP client: {e}")))
}

fn unreachable(e: &reqwest::Error) -> ToolError {
    ToolError::new(
        ToolErrorKind::Failed,
        format!("no browser answered at the DevTools endpoint ({e}). {START_HINT}"),
    )
}

/// Page targets the endpoint lists, newest first as Chrome reports them.
async fn targets(base: &url::Url) -> Result<Vec<Target>, ToolError> {
    let url = base.join("json/list").map_err(|e| ToolError::invalid(format!("bad endpoint: {e}")))?;
    let res = http()?.get(url).send().await.map_err(|e| unreachable(&e))?;
    res.json::<Vec<Target>>().await.map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("the endpoint did not list targets: {e}")))
}

fn is_internal(url: &str) -> bool {
    url.split_once(':').is_some_and(|(s, _)| INTERNAL_SCHEMES.contains(&s.to_ascii_lowercase().as_str()))
}

/// A page the agent may drive: a tab, or a page embedded in a view (a VS Code Simple Browser).
fn is_page(t: &Target) -> bool {
    matches!(t.kind.as_str(), "page" | "iframe" | "webview") && !is_internal(&t.url)
}

/// A page inside another page's view, not a tab of its own.
fn is_embedded(t: &Target) -> bool {
    t.kind != "page"
}

/// Drivable pages, tabs first, so a Chrome tab is still the default ahead of its iframes.
async fn pages(base: &url::Url) -> Result<Vec<Target>, ToolError> {
    let mut pages: Vec<Target> = targets(base).await?.into_iter().filter(is_page).collect();
    pages.sort_by_key(is_embedded);
    Ok(pages)
}

async fn pick_target(base: &url::Url, wanted: Option<&str>) -> Result<Target, ToolError> {
    let pages = pages(base).await?;
    let Some(wanted) = wanted else {
        return pages
            .into_iter()
            .next()
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorKind::NotFound,
                    "the browser has no page open; use the open action, or in VS Code open a Simple Browser view (command Simple Browser: Show)",
                )
            });
    };
    pages
        .iter()
        .find(|t| t.id == wanted)
        .cloned()
        .ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, format!("no page tab '{wanted}'; list them with the tabs action")))
}

/// One CDP session over a websocket: request ids are matched to their replies.
struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<Self, ToolError> {
        let (ws, _) = tokio::time::timeout(CDP_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
            .await
            .map_err(|_| ToolError::new(ToolErrorKind::Timeout, "connecting to the browser timed out"))?
            .map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("could not connect to the page: {e}")))?;
        Ok(Self { ws, next_id: 1 })
    }

    /// Sends one CDP request and returns its `result`, skipping events and other ids.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws
            .send(Message::Text(msg.into()))
            .await
            .map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("{method}: send failed: {e}")))?;
        let deadline = Instant::now() + CDP_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ToolError::new(ToolErrorKind::Timeout, format!("{method} timed out")));
            }
            let next = tokio::time::timeout(left, self.ws.next())
                .await
                .map_err(|_| ToolError::new(ToolErrorKind::Timeout, format!("{method} timed out")))?;
            let Some(frame) = next else {
                return Err(ToolError::new(ToolErrorKind::Failed, format!("{method}: the browser closed the connection")));
            };
            let text = match frame.map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("{method}: {e}")))? {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(_) => {
                    return Err(ToolError::new(ToolErrorKind::Failed, format!("{method}: the browser closed the connection")));
                }
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            if v["id"].as_u64() != Some(id) {
                continue; // An event, or a reply to something else.
            }
            if let Some(err) = v.get("error") {
                let m = err["message"].as_str().unwrap_or("unknown error");
                return Err(ToolError::new(ToolErrorKind::Failed, format!("{method}: {m}")));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Evaluates a JavaScript expression in the page and returns its value.
    async fn eval(&mut self, expression: &str) -> Result<Value, ToolError> {
        let r = self.call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true, "awaitPromise": true })).await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let m = ex["exception"]["description"].as_str().or_else(|| ex["text"].as_str()).unwrap_or("a page error");
            return Err(ToolError::invalid(format!("the page raised an error: {m}")));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// Waits until the document finishes loading, so reading text after a navigation sees the page.
    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), ToolError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.eval("document.readyState").await.ok().as_ref().and_then(Value::as_str) == Some("complete") {
                return Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
        Err(ToolError::new(ToolErrorKind::Timeout, "the page did not finish loading"))
    }
}

/// A JavaScript expression that reads one element's centre, or `null` when it is missing.
fn center_expr(selector: &str) -> String {
    format!(
        "(() => {{ const e = document.querySelector({sel}); if (!e) return null; \
         e.scrollIntoView({{block:'center', inline:'center'}}); const r = e.getBoundingClientRect(); \
         return {{ x: r.left + r.width/2, y: r.top + r.height/2, w: r.width, h: r.height }}; }})()",
        sel = json!(selector)
    )
}

/// A JavaScript expression that clicks one element, for views that refuse synthetic input.
fn click_expr(selector: &str) -> String {
    format!("(() => {{ const e = document.querySelector({sel}); if (!e) return false; e.click(); return true; }})()", sel = json!(selector))
}

/// A JavaScript expression that inserts text at the focus, for views that refuse synthetic input.
fn insert_expr(text: &str) -> String {
    format!("document.execCommand('insertText', false, {t})", t = json!(text))
}

/// A JavaScript expression that sends one key to the focused element; Enter submits its form.
fn key_expr(key: &str) -> String {
    format!(
        "((k) => {{ const e = document.activeElement || document.body; \
         for (const t of ['keydown', 'keyup']) e.dispatchEvent(new KeyboardEvent(t, {{ key: k, bubbles: true, cancelable: true }})); \
         if (k === 'Enter' && e.form) e.form.requestSubmit(); return true; }})({k})",
        k = json!(key)
    )
}

/// A JavaScript expression that focuses one element, or reports it missing.
fn focus_expr(selector: &str) -> String {
    format!("(() => {{ const e = document.querySelector({sel}); if (!e) return false; e.focus(); return true; }})()", sel = json!(selector))
}

fn key_definition(key: &str) -> (&'static str, &'static str, i32) {
    match key {
        "Enter" | "Return" => ("Enter", "Enter", 13),
        "Tab" => ("Tab", "Tab", 9),
        "Escape" | "Esc" => ("Escape", "Escape", 27),
        "Backspace" => ("Backspace", "Backspace", 8),
        "Delete" => ("Delete", "Delete", 46),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39),
        "Home" => ("Home", "Home", 36),
        "End" => ("End", "End", 35),
        "PageUp" => ("PageUp", "PageUp", 33),
        "PageDown" => ("PageDown", "PageDown", 34),
        " " | "Space" => (" ", "Space", 32),
        _ => ("", "", 0),
    }
}

fn required<'a>(v: &'a Option<String>, field: &str, action: &str) -> Result<&'a str, ToolError> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| ToolError::invalid(format!("the {action} action needs {field}")))
}

fn browser(ctx: &ToolCtx, input: Value) -> ToolResult {
    let p: BrowserInput = parse(input)?;
    if !ACTIONS.contains(&p.action.as_str()) {
        return Err(ToolError::invalid(format!("unknown browser action '{}'; use one of: {}", p.action, ACTIONS.join(", "))));
    }
    // Every argument error is reported before the runtime is entered, so a bad call needs no reactor.
    let action = p.action.as_str();
    match action {
        "open" | "goto" => check_page_url(required(&p.url, "url", action)?)?,
        "click" | "wait" => {
            required(&p.selector, "selector", action)?;
        }
        "type" => {
            required(&p.selector, "selector", action)?;
        }
        "press" => {
            if key_definition(required(&p.key, "key", action)?).2 == 0 {
                return Err(ToolError::invalid(format!(
                    "unsupported key '{}'; use a name such as Enter, Tab, Escape, or ArrowDown",
                    p.key.as_deref().unwrap_or_default()
                )));
            }
        }
        "eval" => {
            required(&p.script, "script", action)?;
        }
        _ => {}
    }
    let cancel = ctx.cancel.clone();
    tokio::runtime::Handle::current().block_on(async {
        tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled")),
            r = run(&p) => r,
        }
    })
}

async fn run(p: &BrowserInput) -> ToolResult {
    let action = p.action.as_str();
    let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS));
    // An explicit DevTools endpoint wins; otherwise VS Code's own browser, when the extension serves it.
    if std::env::var(ENV_ENDPOINT).map_or(true, |s| s.trim().is_empty()) {
        if let Some(bridge) = Bridge::from_env() {
            return run_vscode(p, &bridge, timeout).await;
        }
    }
    let base = endpoint()?;

    // `tabs` needs no page session: it is how the model finds a target id.
    if action == "tabs" {
        let tabs: Vec<Value> = pages(&base)
            .await?
            .into_iter()
            .map(|t| json!({ "id": t.id, "url": t.url, "title": t.title, "embedded": is_embedded(&t) }))
            .collect();
        return Ok(json!({ "tabs": tabs, "count": tabs.len(), "endpoint": base.as_str() }));
    }

    if action == "open" {
        let url = required(&p.url, "url", action)?;
        check_page_url(url)?;
        return open_tab(&base, url).await;
    }

    let target = pick_target(&base, p.target.as_deref()).await?;
    if action == "close" {
        if is_embedded(&target) {
            return Err(ToolError::invalid("this page is inside a view (a VS Code Simple Browser); close the view in VS Code"));
        }
        let id = target.id;
        let url = base.join(&format!("json/close/{id}")).map_err(|e| ToolError::invalid(format!("bad endpoint: {e}")))?;
        http()?.get(url).send().await.map_err(|e| unreachable(&e))?;
        return Ok(json!({ "closed": id }));
    }

    let ws = target.ws.clone().ok_or_else(|| {
        ToolError::new(ToolErrorKind::Failed, format!("tab {} publishes no DevTools address; reopen it with the open action", target.id))
    })?;
    let mut cdp = Cdp::connect(&ws).await?;
    let _ = cdp.call("Page.enable", json!({})).await;

    match action {
        "goto" => {
            let url = required(&p.url, "url", action)?;
            check_page_url(url)?;
            navigate(&mut cdp, url, timeout).await?;
            let (u, title) = page_identity(&mut cdp).await;
            Ok(json!({ "url": u, "title": title, "tab": target.id }))
        }
        "click" => {
            let selector = required(&p.selector, "selector", action)?;
            click(&mut cdp, selector).await?;
            Ok(json!({ "clicked": selector, "url": cdp.eval("location.href").await.unwrap_or(Value::Null) }))
        }
        "type" => {
            let selector = required(&p.selector, "selector", action)?;
            let text = p.text.as_deref().unwrap_or_default();
            if cdp.eval(&focus_expr(selector)).await.unwrap_or(Value::Bool(false)) != Value::Bool(true) {
                return Err(ToolError::new(ToolErrorKind::NotFound, format!("no element matches {selector}")));
            }
            if let Err(e) = cdp.call("Input.insertText", json!({ "text": text })).await {
                if cdp.eval(&insert_expr(text)).await? != Value::Bool(true) {
                    return Err(e);
                }
            }
            Ok(json!({ "typed": text.chars().count(), "into": selector }))
        }
        "press" => {
            let key = required(&p.key, "key", action)?;
            press(&mut cdp, key).await?;
            Ok(json!({ "pressed": key }))
        }
        "screenshot" => {
            let r = cdp.call("Page.captureScreenshot", json!({ "format": "png" })).await.map_err(|e| {
                if is_embedded(&target) {
                    ToolError::new(
                        ToolErrorKind::Failed,
                        format!("{}; this page is inside a view (a VS Code Simple Browser), use text or html instead", e.message),
                    )
                } else {
                    e
                }
            })?;
            let data = r["data"].as_str().unwrap_or_default();
            Ok(
                json!({ "format": "png", "base64": data, "bytes": data.len() * 3 / 4, "url": cdp.eval("location.href").await.unwrap_or(Value::Null) }),
            )
        }
        "text" => {
            let body = cdp.eval("document.body ? document.body.innerText : ''").await?;
            let text = body.as_str().unwrap_or_default();
            Ok(json!({ "url": cdp.eval("location.href").await.unwrap_or(Value::Null), "text": text, "chars": text.chars().count() }))
        }
        "html" => {
            let html = cdp.eval("document.documentElement.outerHTML").await?;
            let s = html.as_str().unwrap_or_default();
            Ok(json!({ "url": cdp.eval("location.href").await.unwrap_or(Value::Null), "html": s, "chars": s.chars().count() }))
        }
        "eval" => {
            let script = required(&p.script, "script", action)?;
            Ok(json!({ "value": cdp.eval(script).await? }))
        }
        "wait" => {
            let selector = required(&p.selector, "selector", action)?;
            wait_for(&mut cdp, selector, timeout).await?;
            Ok(json!({ "found": selector }))
        }
        "back" | "forward" => {
            let js = if action == "back" { "history.back(); true" } else { "history.forward(); true" };
            cdp.eval(js).await?;
            wait_quiet(&mut cdp, timeout).await;
            let (u, title) = page_identity(&mut cdp).await;
            Ok(json!({ "url": u, "title": title }))
        }
        other => Err(ToolError::invalid(format!("unsupported browser action '{other}'"))),
    }
}

/// Environment the MCP server child needs to reach the same browser as this runtime.
pub fn bridge_env() -> Vec<(&'static str, String)> {
    [VSCODE_ENV, VSCODE_TOKEN_ENV, ENV_ENDPOINT]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()).map(|v| (k, v)))
        .collect()
}

/// The extension's loopback bridge to VS Code's integrated-browser tools.
struct Bridge {
    url: url::Url,
    token: String,
}

impl Bridge {
    fn from_env() -> Option<Self> {
        let url = url::Url::parse(std::env::var(VSCODE_ENV).ok()?.trim()).ok().filter(|u| check_endpoint(u).is_ok())?;
        let token = std::env::var(VSCODE_TOKEN_ENV).ok().filter(|t| !t.is_empty())?;
        Some(Self { url, token })
    }

    /// Calls one VS Code browser tool and returns its content parts.
    async fn call(&self, tool: &str, input: Value) -> Result<Vec<Value>, ToolError> {
        let url = self.url.join("invoke").map_err(|e| ToolError::invalid(format!("bad {VSCODE_ENV}: {e}")))?;
        let client = reqwest::Client::builder()
            .timeout(VSCODE_TIMEOUT)
            .build()
            .map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("could not create an HTTP client: {e}")))?;
        let res = client.post(url).bearer_auth(&self.token).json(&json!({ "tool": tool, "input": input })).send().await.map_err(|e| {
            ToolError::new(ToolErrorKind::Failed, format!("VS Code's browser bridge did not answer ({e}); reload the VS Code window"))
        })?;
        let status = res.status();
        let body: Value = res
            .json()
            .await
            .map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("VS Code's browser bridge sent no JSON ({status}): {e}")))?;
        if let Some(err) = body["error"].as_str() {
            return Err(ToolError::new(ToolErrorKind::Failed, format!("{tool}: {err}")));
        }
        Ok(body["content"].as_array().cloned().unwrap_or_default())
    }
}

fn text_of(parts: &[Value]) -> String {
    parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")
}

/// The page id in an `open_browser_page` result (`Page ID: <id>`).
fn page_id_of(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.trim().strip_prefix("Page ID:")).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn remember(page: &str) {
    if let Ok(mut last) = LAST_PAGE.lock() {
        *last = Some(page.to_owned());
    }
}

/// The VS Code tool and input for one page action.
fn vscode_call(p: &BrowserInput, page: &str, timeout: Duration) -> Result<(&'static str, Value), ToolError> {
    let action = p.action.as_str();
    let ms = timeout.as_millis() as u64;
    Ok(match action {
        "goto" => {
            let url = required(&p.url, "url", action)?;
            check_page_url(url)?;
            ("navigate_page", json!({ "pageId": page, "type": "url", "url": url }))
        }
        "back" | "forward" => ("navigate_page", json!({ "pageId": page, "type": action })),
        "text" => ("read_page", json!({ "pageId": page })),
        "html" => ("run_playwright_code", json!({ "pageId": page, "code": "return await page.content()", "timeoutMs": ms })),
        "eval" => {
            let script = required(&p.script, "script", action)?;
            ("run_playwright_code", json!({ "pageId": page, "code": format!("return await page.evaluate({})", json!(script)), "timeoutMs": ms }))
        }
        "wait" => {
            let sel = required(&p.selector, "selector", action)?;
            let code = format!("await page.waitForSelector({}, {{ timeout: {ms} }}); return true", json!(sel));
            ("run_playwright_code", json!({ "pageId": page, "code": code, "timeoutMs": ms + 1_000 }))
        }
        "click" => {
            let sel = required(&p.selector, "selector", action)?;
            ("click_element", json!({ "pageId": page, "selector": sel, "element": sel }))
        }
        "type" => {
            let sel = required(&p.selector, "selector", action)?;
            let text = p.text.as_deref().unwrap_or_default();
            ("type_in_page", json!({ "pageId": page, "selector": sel, "element": sel, "text": text }))
        }
        "press" => ("type_in_page", json!({ "pageId": page, "key": key_definition(required(&p.key, "key", action)?).1 })),
        "screenshot" => ("screenshot_page", json!({ "pageId": page })),
        other => return Err(ToolError::invalid(format!("unsupported browser action '{other}'"))),
    })
}

async fn run_vscode(p: &BrowserInput, b: &Bridge, timeout: Duration) -> ToolResult {
    let action = p.action.as_str();
    match action {
        "tabs" => return Ok(json!({ "browser": "vscode", "pages": text_of(&b.call("list_browser_pages", json!({})).await?) })),
        "open" => {
            let url = required(&p.url, "url", action)?;
            check_page_url(url)?;
            let text = text_of(&b.call("open_browser_page", json!({ "url": url })).await?);
            let id = page_id_of(&text).ok_or_else(|| ToolError::new(ToolErrorKind::Failed, format!("VS Code did not open the page: {text}")))?;
            remember(&id);
            return Ok(json!({ "browser": "vscode", "id": id, "url": url, "summary": text }));
        }
        "close" => return Err(ToolError::invalid("VS Code's integrated browser pages are closed from their editor tab, not by the agent")),
        _ => {}
    }
    let page = p
        .target
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| LAST_PAGE.lock().ok().and_then(|l| l.clone()))
        .ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, "no VS Code browser page yet; use open with a url, or pass a page id from tabs as target"))?;
    let (tool, input) = vscode_call(p, &page, timeout)?;
    let parts = b.call(tool, input).await?;
    remember(&page);
    if action == "screenshot" {
        let img = parts.iter().find(|x| x["type"] == "image").ok_or_else(|| {
            ToolError::new(ToolErrorKind::Failed, format!("VS Code returned no image: {}", text_of(&parts)))
        })?;
        let data = img["base64"].as_str().unwrap_or_default();
        return Ok(json!({ "format": img["mimeType"], "base64": data, "bytes": data.len() * 3 / 4, "page": page }));
    }
    Ok(json!({ "browser": "vscode", "page": page, "result": text_of(&parts) }))
}

/// A page URL the browser may open: http(s) only, and a real host.
fn check_page_url(raw: &str) -> Result<(), ToolError> {
    let url = url::Url::parse(raw.trim()).map_err(|e| ToolError::invalid(format!("invalid url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ToolError::invalid("the browser opens only http:// and https:// URLs with a host"));
    }
    Ok(())
}

async fn open_tab(base: &url::Url, url: &str) -> ToolResult {
    // Chrome accepts PUT /json/new?{url}; older builds need a GET. Try the modern form first.
    let target = base.join(&format!("json/new?{}", urlencoding(url))).map_err(|e| ToolError::invalid(format!("bad endpoint: {e}")))?;
    let client = http()?;
    let res = match client.put(target.clone()).send().await {
        Ok(r) => r,
        Err(e) => return Err(unreachable(&e)),
    };
    let res = if res.status().is_success() { res } else { client.get(target).send().await.map_err(|e| unreachable(&e))? };
    let t: Target = res.json().await.map_err(|e| {
        ToolError::new(
            ToolErrorKind::Failed,
            format!(
                "the tab did not open: {e}. VS Code cannot open tabs over DevTools; open a Simple Browser view \
                 (command Simple Browser: Show) and use goto on it"
            ),
        )
    })?;
    Ok(json!({ "id": t.id, "url": t.url, "title": t.title }))
}

/// Percent-encodes a URL for the `json/new` query string.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn navigate(cdp: &mut Cdp, url: &str, timeout: Duration) -> Result<(), ToolError> {
    let r = cdp.call("Page.navigate", json!({ "url": url })).await?;
    if let Some(err) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
        return Err(ToolError::new(ToolErrorKind::Failed, format!("navigation to {url} failed: {err}")));
    }
    cdp.wait_ready(timeout).await
}

async fn click(cdp: &mut Cdp, selector: &str) -> Result<(), ToolError> {
    let Some(box_v) = cdp.eval(&center_expr(selector)).await?.as_object().cloned() else {
        return Err(ToolError::new(ToolErrorKind::NotFound, format!("no element matches {selector}")));
    };
    let (x, y) = (box_v.get("x").and_then(Value::as_f64).unwrap_or(0.0), box_v.get("y").and_then(Value::as_f64).unwrap_or(0.0));
    let base = json!({ "x": x, "y": y, "button": "left", "clickCount": 1 });
    let mut down = base.clone();
    down["type"] = json!("mousePressed");
    let mut up = base;
    up["type"] = json!("mouseReleased");
    let sent = match cdp.call("Input.dispatchMouseEvent", down).await {
        Ok(_) => cdp.call("Input.dispatchMouseEvent", up).await.map(drop),
        Err(e) => Err(e),
    };
    // A view that refuses synthetic input still takes a DOM click.
    match sent {
        Err(e) if cdp.eval(&click_expr(selector)).await? != Value::Bool(true) => Err(e),
        _ => Ok(()),
    }
}

async fn press(cdp: &mut Cdp, key: &str) -> Result<(), ToolError> {
    let (k, code, vk) = key_definition(key);
    if vk == 0 {
        return Err(ToolError::invalid(format!("unsupported key '{key}'; use a name such as Enter, Tab, Escape, or ArrowDown")));
    }
    for kind in ["keyDown", "keyUp"] {
        let params = json!({ "type": kind, "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
        if let Err(e) = cdp.call("Input.dispatchKeyEvent", params).await {
            // A view that refuses synthetic input still takes a DOM key event.
            return if kind == "keyDown" && cdp.eval(&key_expr(k)).await? == Value::Bool(true) { Ok(()) } else { Err(e) };
        }
    }
    Ok(())
}

async fn wait_for(cdp: &mut Cdp, selector: &str, timeout: Duration) -> Result<(), ToolError> {
    let expr = format!("!!document.querySelector({})", json!(selector));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cdp.eval(&expr).await.unwrap_or(Value::Bool(false)) == Value::Bool(true) {
            return Ok(());
        }
        tokio::time::sleep(POLL).await;
    }
    Err(ToolError::new(ToolErrorKind::Timeout, format!("no element matching {selector} appeared")))
}

/// A best-effort wait after a history move, whose load state is not tied to a navigation call.
async fn wait_quiet(cdp: &mut Cdp, timeout: Duration) {
    let _ = cdp.wait_ready(timeout).await;
}

async fn page_identity(cdp: &mut Cdp) -> (Value, Value) {
    (cdp.eval("location.href").await.unwrap_or(Value::Null), cdp.eval("document.title").await.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests::{ctx, run};

    fn url_of(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn the_endpoint_must_be_this_machine() {
        for ok in ["http://127.0.0.1:9222", "http://localhost:9222", "http://[::1]:9222"] {
            assert!(check_endpoint(&url_of(ok)).is_ok(), "{ok} is loopback");
        }
        for bad in ["https://127.0.0.1:9222", "http://10.0.0.5:9222", "http://example.com:9222"] {
            assert!(check_endpoint(&url_of(bad)).is_err(), "{bad} must be refused");
        }
    }

    fn target(kind: &str, url: &str) -> Target {
        Target { id: "t".into(), kind: kind.into(), url: url.into(), title: String::new(), ws: None }
    }

    #[test]
    fn simple_browser_pages_are_drivable_and_vscode_ui_is_not() {
        assert!(is_page(&target("page", "https://example.com/")));
        assert!(is_page(&target("iframe", "http://localhost:3000/")), "a Simple Browser page is an iframe target");
        for (kind, url) in [
            ("page", "vscode-file://vscode-app/out/vs/code/electron-browser/workbench/workbench.html"),
            ("iframe", "vscode-webview://abc/index.html?id=1"),
            ("page", "devtools://devtools/bundled/inspector.html"),
            ("service_worker", "https://example.com/sw.js"),
            ("other", "https://example.com/"),
        ] {
            assert!(!is_page(&target(kind, url)), "{kind} {url} must not be driven");
        }
        assert!(is_embedded(&target("iframe", "https://a.test/")) && !is_embedded(&target("page", "https://a.test/")));
    }

    fn input(v: Value) -> BrowserInput {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn vscode_actions_map_to_its_browser_tools() {
        let t = Duration::from_millis(5_000);
        let call = |v: Value| vscode_call(&input(v), "p1", t).unwrap();
        assert_eq!(
            call(json!({ "action": "goto", "url": "https://google.com" })),
            ("navigate_page", json!({ "pageId": "p1", "type": "url", "url": "https://google.com" }))
        );
        assert_eq!(call(json!({ "action": "back" })).1["type"], "back");
        assert_eq!(call(json!({ "action": "text" })).0, "read_page");
        assert_eq!(call(json!({ "action": "press", "key": "Space" })).1["key"], "Space");
        let (tool, v) = call(json!({ "action": "click", "selector": "button.go" }));
        assert_eq!((tool, v["selector"].as_str(), v["element"].as_str()), ("click_element", Some("button.go"), Some("button.go")));
        let eval = call(json!({ "action": "eval", "script": "document.title + \"x\"" })).1;
        assert_eq!(eval["code"], r#"return await page.evaluate("document.title + \"x\"")"#, "the script is a JS string literal");
        assert!(call(json!({ "action": "wait", "selector": "#q" })).1["code"].as_str().unwrap().contains("timeout: 5000"));
        assert!(vscode_call(&input(json!({ "action": "goto", "url": "file:///etc/passwd" })), "p1", t).is_err());
    }

    #[test]
    fn the_page_id_is_read_from_the_open_result() {
        assert_eq!(page_id_of("Page ID: 3f2a-11\n\nSummary:\n").as_deref(), Some("3f2a-11"));
        assert_eq!(page_id_of("The user declined to share the page."), None);
    }

    #[test]
    fn fallback_scripts_escape_their_input() {
        assert!(click_expr("a'b").contains(r#""a'b""#));
        assert!(insert_expr("x\"); alert(1); (\"").contains(r#""x\"); alert(1); (\"""#));
        assert!(key_expr("Enter").ends_with(r#"("Enter")"#));
    }

    #[test]
    fn selectors_are_escaped_into_javascript() {
        let expr = center_expr("a[href=\"x\"]");
        assert!(expr.contains(r#""a[href=\"x\"]""#), "the selector is a JS string literal: {expr}");
        assert!(focus_expr("button.go").starts_with("(() => {"));
        // A quote in a selector must not break out of the string literal.
        assert!(center_expr("a'b").contains(r#""a'b""#));
    }

    #[test]
    fn key_names_map_to_cdp_codes() {
        assert_eq!(key_definition("Enter"), ("Enter", "Enter", 13));
        assert_eq!(key_definition("ArrowDown"), ("ArrowDown", "ArrowDown", 40));
        assert_eq!(key_definition("nope").2, 0, "an unknown key is refused");
    }

    #[test]
    fn page_urls_are_limited_to_http_and_a_host() {
        assert!(check_page_url("https://example.com/a?b=1").is_ok());
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "ftp://x/y", "https://"] {
            assert!(check_page_url(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn urls_are_percent_encoded_for_the_open_call() {
        assert_eq!(urlencoding("https://a.test/x?y=1&z=2"), "https%3A%2F%2Fa.test%2Fx%3Fy%3D1%26z%3D2");
    }

    #[test]
    fn actions_are_validated_before_any_request() {
        let c = ctx(&[("a.txt", "x\n")]);
        let unknown = run(&c, "browser", json!({ "action": "fly" })).unwrap_err();
        assert_eq!(unknown.kind, ToolErrorKind::InvalidInput);
        assert!(unknown.message.contains("unknown browser action"), "{}", unknown.message);
        let missing = run(&c, "browser", json!({ "action": "goto" })).unwrap_err();
        assert!(missing.message.contains("needs url"), "{}", missing.message);
        let bad_key = run(&c, "browser", json!({ "action": "press", "key": "Hyper" })).unwrap_err();
        assert!(bad_key.message.contains("unsupported key"), "{}", bad_key.message);
    }

    #[test]
    fn the_tool_is_registered_with_network_permission() {
        let def = crate::tools::find("browser").expect("browser is registered");
        assert_eq!(def.permission, Permission::Network);
        assert_eq!((def.schema)()["required"], json!(["action"]));
        assert_eq!((def.schema)()["properties"]["action"]["enum"], json!(ACTIONS));
    }
}
