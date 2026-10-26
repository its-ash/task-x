//! Browser automation over the Chrome DevTools Protocol (CDP).
//!
//! task-x drives a browser the user already has open, through its DevTools endpoint. A VS Code
//! *Simple Browser* view is a webview and publishes no DevTools protocol, so the browser must be a
//! real Chrome or Edge started with a remote debugging port (`--remote-debugging-port=9222`).
//!
//! The endpoint is the one place where reaching loopback is required, so it is checked by its own
//! rule: only a local http endpoint is accepted. `web_fetch` keeps refusing loopback.

use super::schema::{int, object};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9222";
const ENV_ENDPOINT: &str = "TASK_X_CDP_ENDPOINT";
const CDP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WAIT_MS: u64 = 5_000;
const MAX_WAIT_MS: u64 = 30_000;
const POLL: Duration = Duration::from_millis(100);

const ACTIONS: &[&str] =
    &["tabs", "open", "goto", "click", "type", "press", "screenshot", "text", "html", "eval", "wait", "back", "forward", "close"];

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "browser",
        "Drive the browser the user has open, over the Chrome DevTools Protocol: list tabs (tabs), open a page (open), navigate (goto), click a CSS selector (click), type into a selector (type), press a key (press), read the visible text (text) or the HTML (html), take a screenshot (screenshot), run JavaScript in the page (eval), wait for a selector (wait), go back or forward (back, forward), and close the tab (close). Requires a Chrome or Edge started with --remote-debugging-port (a VS Code Simple Browser view cannot be driven).",
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
    const HINT: &str = "start Chrome or Edge with --remote-debugging-port=9222 and set TASK_X_CDP_ENDPOINT to its local address";
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
        format!(
            "no browser answered at the DevTools endpoint ({e}). Open Chrome or Edge with --remote-debugging-port=9222, \
             or set {ENV_ENDPOINT}. A VS Code Simple Browser view is a webview and cannot be driven."
        ),
    )
}

/// Page targets the endpoint lists, newest first as Chrome reports them.
async fn targets(base: &url::Url) -> Result<Vec<Target>, ToolError> {
    let url = base.join("json/list").map_err(|e| ToolError::invalid(format!("bad endpoint: {e}")))?;
    let res = http()?.get(url).send().await.map_err(|e| unreachable(&e))?;
    res.json::<Vec<Target>>().await.map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("the endpoint did not list targets: {e}")))
}

fn is_page(t: &Target) -> bool {
    t.kind == "page"
}

async fn pick_target(base: &url::Url, wanted: Option<&str>) -> Result<Target, ToolError> {
    let pages: Vec<Target> = targets(base).await?.into_iter().filter(is_page).collect();
    let Some(wanted) = wanted else {
        return pages
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, "the browser has no page tab open; use the open action to start one"));
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
    let base = endpoint()?;
    let action = p.action.as_str();
    let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS));

    // `tabs` needs no page session: it is how the model finds a target id.
    if action == "tabs" {
        let tabs: Vec<Value> =
            targets(&base).await?.into_iter().filter(is_page).map(|t| json!({ "id": t.id, "url": t.url, "title": t.title })).collect();
        return Ok(json!({ "tabs": tabs, "count": tabs.len(), "endpoint": base.as_str() }));
    }

    if action == "open" {
        let url = required(&p.url, "url", action)?;
        check_page_url(url)?;
        return open_tab(&base, url).await;
    }

    let target = pick_target(&base, p.target.as_deref()).await?;
    if action == "close" {
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
            cdp.call("Input.insertText", json!({ "text": text })).await?;
            Ok(json!({ "typed": text.chars().count(), "into": selector }))
        }
        "press" => {
            let key = required(&p.key, "key", action)?;
            press(&mut cdp, key).await?;
            Ok(json!({ "pressed": key }))
        }
        "screenshot" => {
            let r = cdp.call("Page.captureScreenshot", json!({ "format": "png" })).await?;
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
    let t: Target = res.json().await.map_err(|e| ToolError::new(ToolErrorKind::Failed, format!("the tab did not open: {e}")))?;
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
    cdp.call("Input.dispatchMouseEvent", down).await?;
    cdp.call("Input.dispatchMouseEvent", up).await?;
    Ok(())
}

async fn press(cdp: &mut Cdp, key: &str) -> Result<(), ToolError> {
    let (k, code, vk) = key_definition(key);
    if vk == 0 {
        return Err(ToolError::invalid(format!("unsupported key '{key}'; use a name such as Enter, Tab, Escape, or ArrowDown")));
    }
    for kind in ["keyDown", "keyUp"] {
        cdp.call(
            "Input.dispatchKeyEvent",
            json!({ "type": kind, "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
        )
        .await?;
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
