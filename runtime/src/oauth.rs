//! OAuth 2.1 sign-in for remote MCP servers, as the MCP authorization spec describes:
//! 1. the server answers 401 with `WWW-Authenticate: Bearer resource_metadata="…"`;
//! 2. the protected-resource metadata names the authorization server, whose metadata gives the
//!    endpoints (RFC 9728, RFC 8414);
//! 3. task-x registers itself as a public client (RFC 7591) when the server allows it;
//! 4. the user signs in in the browser (authorization code with PKCE S256), and the code comes back
//!    to a one-shot listener on 127.0.0.1;
//! 5. the code becomes tokens, which the client keeps (VS Code SecretStorage) and sends back with
//!    `mcp/configure`; an expired access token is refreshed before the server starts.
//!
//! Every endpoint must be https (http only on loopback). Tokens are never logged.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// An access token this close to its end is refreshed first.
const REFRESH_MARGIN_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub token_endpoint: String,
    /// The MCP server URL the tokens are for (the RFC 8707 `resource`).
    pub resource: String,
}

impl Tokens {
    pub fn expired(&self) -> bool {
        self.expires_at_ms.is_some_and(|t| crate::agent::store::now_ms() + REFRESH_MARGIN_MS >= t)
    }
}

/// Opens the sign-in page for the user (the client opens a browser); the future ends when it is shown.
pub type Opener = std::sync::Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Debug, Clone)]
struct AuthServer {
    authorization_endpoint: url::Url,
    token_endpoint: url::Url,
    registration_endpoint: Option<url::Url>,
    scopes: Vec<String>,
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

fn loopback(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// An endpoint the server named: https, or http on this machine.
fn endpoint(raw: &str) -> Result<url::Url, String> {
    let u = url::Url::parse(raw).map_err(|e| format!("invalid OAuth endpoint {raw}: {e}"))?;
    if u.scheme() == "https" || (u.scheme() == "http" && loopback(&u)) {
        Ok(u)
    } else {
        Err(format!("refusing the OAuth endpoint {raw}: it must use https"))
    }
}

/// The `resource_metadata` URL of a `WWW-Authenticate: Bearer …` header.
pub fn resource_metadata(www_authenticate: &str) -> Option<String> {
    let i = www_authenticate.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &www_authenticate[i..];
    let v = match rest.strip_prefix('"') {
        Some(q) => &q[..q.find('"')?],
        None => rest.split([',', ' ']).next()?,
    };
    (!v.is_empty()).then(|| v.to_owned())
}

/// `{origin}/.well-known/{name}{path}`: the well-known URL for an issuer or resource with a path.
fn well_known(base: &url::Url, name: &str) -> Vec<url::Url> {
    let path = base.path().trim_end_matches('/');
    let mut out = Vec::new();
    for p in [format!("/.well-known/{name}{path}"), format!("/.well-known/{name}")] {
        if let Ok(u) = base.join(&p) {
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

async fn get_json(http: &reqwest::Client, u: &url::Url) -> Option<Value> {
    let r = http.get(u.clone()).header("Accept", "application/json").send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// Finds the authorization server of an MCP server.
async fn discover(http: &reqwest::Client, server: &url::Url, www_authenticate: Option<&str>) -> Result<AuthServer, String> {
    let mut resource_urls: Vec<url::Url> = www_authenticate.and_then(resource_metadata).and_then(|u| endpoint(&u).ok()).into_iter().collect();
    resource_urls.extend(well_known(server, "oauth-protected-resource"));
    let mut issuer: Option<url::Url> = None;
    for u in &resource_urls {
        if let Some(v) = get_json(http, u).await {
            issuer = v["authorization_servers"].as_array().and_then(|a| a.first()).and_then(Value::as_str).and_then(|s| endpoint(s).ok());
            if issuer.is_some() {
                break;
            }
        }
    }
    // Servers from before RFC 9728 act as their own authorization server.
    let issuer = issuer.unwrap_or_else(|| {
        let mut o = server.clone();
        o.set_path("");
        o.set_query(None);
        o
    });
    let mut meta = None;
    for name in ["oauth-authorization-server", "openid-configuration"] {
        for u in well_known(&issuer, name) {
            if let Some(v) = get_json(http, &u).await.filter(|v| v["authorization_endpoint"].is_string()) {
                meta = Some(v);
                break;
            }
        }
        if meta.is_some() {
            break;
        }
    }
    let m = meta.ok_or_else(|| format!("{issuer} has no OAuth authorization server metadata"))?;
    let methods = m["code_challenge_methods_supported"].as_array();
    if methods.is_some_and(|a| !a.iter().any(|x| x == "S256")) {
        return Err("the authorization server does not support PKCE S256".into());
    }
    Ok(AuthServer {
        authorization_endpoint: endpoint(m["authorization_endpoint"].as_str().unwrap_or_default())?,
        token_endpoint: endpoint(m["token_endpoint"].as_str().ok_or("the authorization server names no token endpoint")?)?,
        registration_endpoint: m["registration_endpoint"].as_str().map(endpoint).transpose()?,
        scopes: m["scopes_supported"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
    })
}

/// RFC 7591 dynamic registration as a public client with this redirect URI.
async fn register(http: &reqwest::Client, a: &AuthServer, redirect: &str) -> Result<(String, Option<String>), String> {
    let reg = a.registration_endpoint.clone().ok_or("the authorization server does not allow client registration; put a token in the server's headers instead")?;
    let body = serde_json::json!({
        "client_name": "task-x",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let r = http.post(reg).json(&body).send().await.map_err(|e| format!("client registration: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("client registration answered HTTP {}", r.status()));
    }
    let v: Value = r.json().await.map_err(|e| format!("client registration: {e}"))?;
    let id = v["client_id"].as_str().filter(|s| !s.is_empty()).ok_or("client registration returned no client_id")?;
    Ok((id.to_owned(), v["client_secret"].as_str().map(str::to_owned)))
}

/// base64url without padding (RFC 4648 §5).
pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    use ring::rand::SecureRandom;
    let mut b = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| "no secure random source".to_owned())?;
    Ok(b)
}

/// A PKCE verifier and its S256 challenge.
pub fn pkce() -> Result<(String, String), String> {
    let verifier = b64url(&random(32)?);
    let challenge = b64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
    Ok((verifier, challenge))
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

async fn token_request(http: &reqwest::Client, token_endpoint: &str, pairs: &[(&str, &str)]) -> Result<Value, String> {
    let r = http
        .post(endpoint(token_endpoint)?)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form(pairs))
        .send()
        .await
        .map_err(|e| format!("token request: {e}"))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("the token endpoint answered HTTP {status}: {}", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no reason given")));
    }
    if !v["access_token"].is_string() {
        return Err("the token endpoint returned no access_token".into());
    }
    Ok(v)
}

fn tokens_from(v: &Value, client_id: &str, client_secret: Option<String>, token_endpoint: &str, resource: &str, old_refresh: Option<String>) -> Tokens {
    Tokens {
        access_token: v["access_token"].as_str().unwrap_or_default().to_owned(),
        refresh_token: v["refresh_token"].as_str().map(str::to_owned).or(old_refresh),
        expires_at_ms: v["expires_in"].as_u64().map(|s| crate::agent::store::now_ms() + s * 1000),
        client_id: client_id.to_owned(),
        client_secret,
        token_endpoint: token_endpoint.to_owned(),
        resource: resource.to_owned(),
    }
}

/// New tokens from the refresh token.
pub async fn refresh(t: &Tokens) -> Result<Tokens, String> {
    let rt = t.refresh_token.as_deref().ok_or("no refresh token; sign in again")?;
    let mut pairs = vec![("grant_type", "refresh_token"), ("refresh_token", rt), ("client_id", t.client_id.as_str()), ("resource", t.resource.as_str())];
    if let Some(s) = &t.client_secret {
        pairs.push(("client_secret", s));
    }
    let v = token_request(&http()?, &t.token_endpoint, &pairs).await?;
    Ok(tokens_from(&v, &t.client_id, t.client_secret.clone(), &t.token_endpoint, &t.resource, t.refresh_token.clone()))
}

/// Waits on the listener for the browser's redirect and returns the code (checking `state`).
async fn wait_code(listener: tokio::net::TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
        let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let u = url::Url::parse(&format!("http://127.0.0.1{target}")).map_err(|e| e.to_string())?;
        if u.path() != "/callback" {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            continue;
        }
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        let (ok, page) = match (q.get("code"), q.get("state"), q.get("error")) {
            (_, _, Some(e)) => (Err(format!("sign-in failed: {e} {}", q.get("error_description").map_or("", String::as_str))), "Sign-in failed. You can close this tab."),
            (Some(code), Some(s), None) if s == state => (Ok(code.clone()), "Signed in to the MCP server. You can close this tab and return to task-x."),
            _ => (Err("the sign-in answer did not match this request (state)".to_owned()), "This sign-in link is not valid."),
        };
        let body = format!("<!doctype html><meta charset=utf-8><title>task-x</title><p style=\"font:16px system-ui;margin:3em\">{page}</p>");
        let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        return ok;
    }
}

/// The whole sign-in for the MCP server at `server_url`. `www_authenticate` is the header of its
/// 401 answer, when there was one. `open` shows the sign-in page to the user.
pub async fn sign_in(server_url: &str, www_authenticate: Option<&str>, open: Opener, cancel: &CancellationToken) -> Result<Tokens, String> {
    let server = endpoint(server_url)?;
    let http = http()?;
    let a = discover(&http, &server, www_authenticate).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("cannot listen for the sign-in answer: {e}"))?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(|e| e.to_string())?.port());
    let (client_id, client_secret) = register(&http, &a, &redirect).await?;
    let (verifier, challenge) = pkce()?;
    let state = b64url(&random(16)?);
    let mut auth = a.authorization_endpoint.clone();
    {
        let mut q = auth.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", server.as_str());
        if !a.scopes.is_empty() {
            q.append_pair("scope", &a.scopes.join(" "));
        }
    }
    open(auth.to_string()).await;
    let code = tokio::select! {
        _ = cancel.cancelled() => return Err("sign-in cancelled".into()),
        r = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_code(listener, &state)) => r.map_err(|_| "sign-in timed out after 5 minutes".to_owned())??,
    };
    let token = a.token_endpoint.to_string();
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", server.as_str()),
    ];
    if let Some(s) = &client_secret {
        pairs.push(("client_secret", s));
    }
    let v = token_request(&http, &token, &pairs).await?;
    Ok(tokens_from(&v, &client_id, client_secret, &token, server.as_str(), None))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn encodes_base64url_and_reads_the_metadata_header() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        let (v, c) = pkce().unwrap();
        assert_eq!(v.len(), 43);
        assert_eq!(c, b64url(ring::digest::digest(&ring::digest::SHA256, v.as_bytes()).as_ref()));
        assert_eq!(
            resource_metadata(r#"Bearer realm="mcp", resource_metadata="https://a.example/.well-known/oauth-protected-resource""#).as_deref(),
            Some("https://a.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(resource_metadata("Bearer resource_metadata=https://x/m, error=\"invalid\"").as_deref(), Some("https://x/m"));
        assert!(resource_metadata("Bearer realm=x").is_none());
        assert!(endpoint("http://auth.example/token").is_err(), "plain http only on loopback");
        assert!(endpoint("http://127.0.0.1:9/token").is_ok());
    }

    /// A local OAuth server: resource and server metadata, registration, and a token endpoint that
    /// checks the PKCE verifier against the challenge of the authorize request.
    pub async fn auth_server() -> (String, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (b, s) = (base.clone(), seen.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else { return };
                let mut buf = vec![0u8; 16384];
                let n = c.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                let body = req.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
                s.lock().unwrap().push(format!("{line} {body}"));
                let path = line.split_whitespace().nth(1).unwrap_or("/");
                // The MCP server itself: 401 without the token, a minimal server with it.
                if path == "/mcp" {
                    let authorized = req.lines().any(|l| l.eq_ignore_ascii_case("authorization: Bearer tok-1"));
                    if !authorized {
                        let h = format!("Bearer resource_metadata=\"{b}/.well-known/oauth-protected-resource\"");
                        let _ = c.write_all(format!("HTTP/1.1 401 Unauthorized\r\nwww-authenticate: {h}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await;
                        continue;
                    }
                    let msg: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let result = match msg["method"].as_str() {
                        Some("initialize") => serde_json::json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} }, "serverInfo": { "name": "secure" } }),
                        Some("tools/list") => serde_json::json!({ "tools": [{ "name": "whoami", "inputSchema": { "type": "object" } }] }),
                        _ => serde_json::json!({}),
                    };
                    let out = if msg["id"].is_null() { String::new() } else { serde_json::json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result }).to_string() };
                    let status = if out.is_empty() { "202 Accepted" } else { "200 OK" };
                    let _ = c.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len()).as_bytes()).await;
                    continue;
                }
                let json = if path.starts_with("/.well-known/oauth-protected-resource") {
                    format!(r#"{{"resource":"{b}/mcp","authorization_servers":["{b}"]}}"#)
                } else if path.starts_with("/.well-known/oauth-authorization-server") {
                    format!(r#"{{"issuer":"{b}","authorization_endpoint":"{b}/authorize","token_endpoint":"{b}/token","registration_endpoint":"{b}/register","code_challenge_methods_supported":["S256"],"scopes_supported":["mcp"]}}"#)
                } else if path == "/register" {
                    r#"{"client_id":"client-1"}"#.into()
                } else if path == "/token" && body.contains("grant_type=refresh_token") {
                    r#"{"access_token":"tok-2","expires_in":3600,"token_type":"Bearer"}"#.into()
                } else if path == "/token" && body.contains("code=the-code") && body.contains("code_verifier=") {
                    r#"{"access_token":"tok-1","refresh_token":"ref-1","expires_in":3600,"token_type":"Bearer"}"#.into()
                } else {
                    let _ = c.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    continue;
                };
                let _ = c.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}", json.len()).as_bytes()).await;
            }
        });
        (base, seen)
    }

    /// A stand-in for the browser: follows the sign-in page straight to the redirect with a code.
    pub fn browser() -> (Opener, Arc<Mutex<Option<String>>>) {
        let shown = Arc::new(Mutex::new(None));
        let s = shown.clone();
        let open: Opener = Arc::new(move |u: String| {
            *s.lock().unwrap() = Some(u.clone());
            Box::pin(async move {
                let u = url::Url::parse(&u).unwrap();
                let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
                let back = format!("{}?code=the-code&state={}", q["redirect_uri"], q["state"]);
                tokio::spawn(async move {
                    let _ = reqwest::get(back).await;
                });
            })
        });
        (open, shown)
    }

    #[tokio::test]
    async fn signs_in_with_pkce_and_refreshes() {
        let (base, seen) = auth_server().await;
        let (open, shown) = browser();
        let header = format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#);
        let t = sign_in(&format!("{base}/mcp"), Some(&header), open, &CancellationToken::new()).await.unwrap();
        assert_eq!((t.access_token.as_str(), t.refresh_token.as_deref(), t.client_id.as_str()), ("tok-1", Some("ref-1"), "client-1"));
        assert!(!t.expired());
        let page = shown.lock().unwrap().clone().unwrap();
        assert!(page.contains("code_challenge_method=S256") && page.contains("scope=mcp") && page.contains("resource="), "{page}");
        assert!(seen.lock().unwrap().iter().any(|r| r.starts_with("POST /token") && r.contains("code_verifier=")));
        let mut old = t.clone();
        old.expires_at_ms = Some(1);
        assert!(old.expired());
        let r = refresh(&old).await.unwrap();
        assert_eq!((r.access_token.as_str(), r.refresh_token.as_deref()), ("tok-2", Some("ref-1")), "the refresh token is kept when none comes back");
        let none = Tokens { refresh_token: None, ..old };
        assert!(refresh(&none).await.is_err());
    }
}
